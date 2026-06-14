use super::*;

impl UiModel {
    pub(in crate::app) fn handle_terminal_event(&mut self, event: CrosstermEvent) -> Vec<UiEffect> {
        match event {
            CrosstermEvent::Key(key_event) => self.handle_key_event(key_event),
            CrosstermEvent::Paste(text) => self.handle_paste_event(text),
            CrosstermEvent::Resize(width, height) => {
                self.terminal_width = width;
                self.viewport_height = self.transcript_viewport_height(width, height);
                self.render_cache
                    .evict_stale_widths(self.transcript_cache_width_hint(width));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn handle_paste_event(&mut self, text: String) -> Vec<UiEffect> {
        if self.screen == Screen::Workspace {
            if let Some(picker) = self.question_picker.as_mut() {
                picker.handle_paste(&text);
            } else if self.mode == UiMode::Insert {
                self.composer.insert_paste(&text);
                self.sync_skill_popup();
            }
        }
        Vec::new()
    }

    pub(in crate::app) fn handle_key_event(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        self.handle_key_event_at(key_event, Instant::now())
    }

    pub(in crate::app) fn is_ctrl_o(key_event: KeyEvent) -> bool {
        matches!(key_event.code, KeyCode::Char('o') | KeyCode::Char('O'))
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
    }

    pub(in crate::app) fn is_disambiguated_ctrl_i(key_event: KeyEvent) -> bool {
        matches!(key_event.code, KeyCode::Char('i') | KeyCode::Char('I'))
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
    }

    pub(in crate::app) fn is_preview_forward_key(&self, key_event: KeyEvent) -> bool {
        Self::is_disambiguated_ctrl_i(key_event)
            || (!self.preview_forward_stack.is_empty()
                && matches!(key_event.code, KeyCode::Tab)
                && (key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::CONTROL))
    }

    pub(in crate::app) fn preview_forward_allowed_in_current_context(&self) -> bool {
        self.screen == Screen::Workspace
            && self.question_picker.is_none()
            && self.plan_approval.is_none()
            && !matches!(
                self.mode,
                UiMode::Command | UiMode::Insert | UiMode::Overlay
            )
    }

    pub(in crate::app) fn handle_key_event_at(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
    ) -> Vec<UiEffect> {
        if key_event.kind != crossterm::event::KeyEventKind::Press {
            return Vec::new();
        }

        // Any non-Esc key breaks the double-Esc chord.
        if key_event.code != KeyCode::Esc {
            self.last_esc = None;
        }

        if matches!(key_event.code, KeyCode::Char('c'))
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.is_turn_running {
                return self.request_turn_cancel();
            }
            return vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)];
        }

        if self.preview_forward_allowed_in_current_context()
            && !self.preview_forward_stack.is_empty()
            && self.is_preview_forward_key(key_event)
        {
            return self.reopen_forward_preview();
        }

        // A stacked subagent preview owns the keyboard (server-driven
        // overlays clear the stack on arrival, so they cannot coexist).
        if self.screen == Screen::Workspace && !self.preview_stack.is_empty() {
            return self.handle_preview_key(key_event, now);
        }

        if self.screen == Screen::Workspace && self.question_picker.is_some() {
            return self.dispatch_picker_key(key_event);
        }

        if self.screen == Screen::Workspace && self.plan_approval.is_some() {
            return self.dispatch_plan_approval_key(key_event);
        }

        if self.mode == UiMode::Command {
            return self.handle_command_key(key_event);
        }

        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key_event),
            Screen::Workspace => self.handle_workspace_key(key_event, now),
        }
    }

    pub(in crate::app) fn handle_dashboard_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        match key_event.code {
            KeyCode::Char('n') => {
                self.status_line = String::from("Starting new thread");
                vec![self.effect(EffectContext::ThreadStart, UiEffectKind::ThreadStart)]
            }
            KeyCode::Enter => {
                let Some(item) = self.dashboard.items.get(self.dashboard.selected) else {
                    return Vec::new();
                };
                match item.thread_id.parse::<ThreadId>() {
                    Ok(thread_id) => {
                        self.status_line = format!("Resuming thread {}", item.thread_id);
                        vec![self.effect(
                            EffectContext::ThreadResume { thread_id },
                            UiEffectKind::ThreadResume { thread_id },
                        )]
                    }
                    Err(err) => {
                        self.dashboard.error = Some(format!("invalid thread id: {err}"));
                        Vec::new()
                    }
                }
            }
            KeyCode::Char(':') => {
                self.mode = UiMode::Command;
                self.command.clear();
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.dashboard.selected = self.dashboard.selected.saturating_sub(1);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.dashboard.items.len().saturating_sub(1);
                self.dashboard.selected = self.dashboard.selected.saturating_add(1).min(max);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::PageUp => {
                let amount = self
                    .dashboard_visible_item_count(self.viewport_height)
                    .max(1);
                self.dashboard.selected = self.dashboard.selected.saturating_sub(amount);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::PageDown => {
                let amount = self
                    .dashboard_visible_item_count(self.viewport_height)
                    .max(1);
                let max = self.dashboard.items.len().saturating_sub(1);
                self.dashboard.selected = self.dashboard.selected.saturating_add(amount).min(max);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::Home => {
                self.dashboard.selected = 0;
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::End => {
                self.dashboard.selected = self.dashboard.items.len().saturating_sub(1);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)]
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn handle_workspace_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
    ) -> Vec<UiEffect> {
        match self.mode {
            UiMode::Normal => self.handle_normal_key(key_event, now),
            UiMode::Insert => self.handle_insert_key(key_event, now),
            UiMode::TranscriptSelect => self.handle_transcript_select_key(key_event, now),
            UiMode::Command | UiMode::Overlay => Vec::new(),
        }
    }

    pub(in crate::app) fn handle_normal_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
    ) -> Vec<UiEffect> {
        match key_event.code {
            KeyCode::Char('i') if key_event.modifiers.is_empty() => {
                self.mode = UiMode::Insert;
                self.focus = FocusTarget::Composer;
                Vec::new()
            }
            KeyCode::Char('I')
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.toggle_inspector_visible();
                Vec::new()
            }
            KeyCode::Char(':') => {
                self.mode = UiMode::Command;
                self.command.clear();
                Vec::new()
            }
            KeyCode::Char('q') if self.composer.is_empty() => {
                vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)]
            }
            KeyCode::Tab => {
                self.focus_next();
                Vec::new()
            }
            KeyCode::BackTab => {
                self.focus_prev();
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down(1, self.viewport_height);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.scroll_up(self.viewport_height.saturating_sub(1).max(1));
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll_down(
                    self.viewport_height.saturating_sub(1).max(1),
                    self.viewport_height,
                );
                Vec::new()
            }
            KeyCode::Home => {
                self.scroll = 0;
                self.auto_scroll = false;
                Vec::new()
            }
            KeyCode::End => {
                self.auto_scroll = true;
                self.scroll_to_bottom(self.viewport_height);
                Vec::new()
            }
            KeyCode::Esc => {
                if self
                    .last_esc
                    .is_some_and(|t| now.duration_since(t) <= DOUBLE_ESC_WINDOW)
                {
                    self.last_esc = None;
                    self.enter_transcript_select();
                } else {
                    self.last_esc = Some(now);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Enter transcript-select mode (double-Esc). Selection starts on the last
    /// transcript row; follow-mode is suspended so streaming output doesn't
    /// yank the view while the user navigates.
    pub(in crate::app) fn enter_transcript_select(&mut self) {
        if self.screen != Screen::Workspace || self.mode != UiMode::Normal {
            return;
        }
        if self.transcript_items.is_empty() {
            self.status_line = String::from("Transcript is empty");
            return;
        }
        self.mode = UiMode::TranscriptSelect;
        self.focus = FocusTarget::Transcript;
        self.auto_scroll = false;
        let selected = self.transcript_items.len().saturating_sub(1);
        self.transcript_select = Some(TranscriptSelectState {
            selected,
            selected_tool_entry: self.default_tool_entry_for_selection(selected, true),
            pending_args: None,
            pending_g: None,
        });
        self.transcript_select_ensure_visible(self.viewport_height);
        self.status_line = String::from("Transcript select");
    }

    fn default_tool_entry_for_selection(
        &self,
        selected: usize,
        prefer_last: bool,
    ) -> Option<usize> {
        let group = self
            .transcript_items
            .get(selected)
            .and_then(|item| item.tool_group_cell())?;
        if !group.is_batch() {
            return None;
        }
        Some(if prefer_last {
            group.entry_count().saturating_sub(1)
        } else {
            0
        })
    }

    fn move_transcript_selection_up(&self, state: &mut TranscriptSelectState) {
        if let Some(entry_idx) = state.selected_tool_entry
            && entry_idx > 0
        {
            state.selected_tool_entry = Some(entry_idx - 1);
            return;
        }
        state.selected = state.selected.saturating_sub(1);
        state.selected_tool_entry = self.default_tool_entry_for_selection(state.selected, true);
    }

    fn move_transcript_selection_down(&self, state: &mut TranscriptSelectState) {
        if let Some(entry_idx) = state.selected_tool_entry
            && let Some(group) = self
                .transcript_items
                .get(state.selected)
                .and_then(|item| item.tool_group_cell())
            && entry_idx + 1 < group.entry_count()
        {
            state.selected_tool_entry = Some(entry_idx + 1);
            return;
        }
        let last = self.transcript_items.len().saturating_sub(1);
        state.selected = state.selected.saturating_add(1).min(last);
        state.selected_tool_entry = self.default_tool_entry_for_selection(state.selected, false);
    }

    /// Leave transcript-select mode. Safe to call when not in it (no-op apart
    /// from clearing any stale selection state).
    pub(in crate::app) fn exit_transcript_select(&mut self) {
        self.transcript_select = None;
        if self.mode == UiMode::TranscriptSelect {
            self.mode = UiMode::Normal;
            self.focus = FocusTarget::Transcript;
            let max_scroll = self.max_scroll(self.viewport_height);
            self.auto_scroll = self.scroll >= max_scroll;
        }
    }

    pub(in crate::app) fn handle_transcript_select_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
    ) -> Vec<UiEffect> {
        let Some(mut state) = self.transcript_select else {
            self.exit_transcript_select();
            return Vec::new();
        };
        let last = self.transcript_items.len().saturating_sub(1);
        match key_event.code {
            // Unlike Normal mode, `q` exits the selection, not the app.
            KeyCode::Esc | KeyCode::Char('q') => {
                self.exit_transcript_select();
                return Vec::new();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_transcript_selection_up(&mut state);
                state.pending_args = None;
                state.pending_g = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_transcript_selection_down(&mut state);
                state.pending_args = None;
                state.pending_g = None;
            }
            // `g` is a prefix: `gg` jumps to the top, `gd` opens the selected
            // row's subagent session. `Home` keeps the direct jump.
            KeyCode::Char('g') => {
                state.pending_args = None;
                let chord = state
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                if chord {
                    state.pending_g = None;
                    state.selected = 0;
                    state.selected_tool_entry = self.default_tool_entry_for_selection(0, false);
                } else {
                    state.pending_g = Some(now);
                    self.transcript_select = Some(state);
                    return Vec::new();
                }
            }
            KeyCode::Char('d') => {
                let chord = state
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                state.pending_g = None;
                if chord {
                    return self.goto_selected_subagent(state);
                }
                self.transcript_select = Some(state);
                return Vec::new();
            }
            KeyCode::Home => {
                state.selected = 0;
                state.selected_tool_entry = self.default_tool_entry_for_selection(0, false);
                state.pending_args = None;
                state.pending_g = None;
            }
            KeyCode::End | KeyCode::Char('G') => {
                state.selected = last;
                state.selected_tool_entry = self.default_tool_entry_for_selection(last, true);
                state.pending_args = None;
                state.pending_g = None;
            }
            KeyCode::Char('y') => {
                state.pending_g = None;
                return self.copy_selected_transcript_row(state, now);
            }
            _ => return Vec::new(),
        }
        self.transcript_select = Some(state);
        self.transcript_select_ensure_visible(self.viewport_height);
        Vec::new()
    }

    /// `gd` in transcript-select mode: open the selected `spawn_agent` row's
    /// subagent session as a live preview. The selection and scroll state are
    /// left untouched, so closing the preview restores this view exactly.
    pub(in crate::app) fn goto_selected_subagent(
        &mut self,
        state: TranscriptSelectState,
    ) -> Vec<UiEffect> {
        self.transcript_select = Some(state);
        let group = self
            .transcript_items
            .get(state.selected)
            .and_then(|item| item.tool_group_cell());
        let Some(group) = group else {
            self.status_line = String::from("Not a subagent row (gd opens spawn_agent sessions)");
            return Vec::new();
        };
        let thread_id = if group.is_batch() {
            state
                .selected_tool_entry
                .and_then(|entry_idx| group.entry_subagent_thread_id(entry_idx))
        } else {
            group.subagent_thread_id()
        };
        let Some(thread_id) = thread_id else {
            self.status_line = if group.is_spawn_agent() {
                String::from("Subagent not started yet — no session to open")
            } else {
                String::from("Not a subagent row (gd opens spawn_agent sessions)")
            };
            return Vec::new();
        };
        self.preview_forward_stack.clear();
        self.status_line = String::from("Opening subagent…");
        vec![self.effect(
            EffectContext::ThreadPreview { thread_id },
            UiEffectKind::ThreadPreview { thread_id },
        )]
    }

    /// Enter submits only with Ctrl. Cmd/Super is intentionally not accepted:
    /// macOS terminals reserve Cmd for their own bindings (e.g. Ghostty maps
    /// `super+enter` to toggle-fullscreen), so it never reaches the app.
    /// Distinguishing Ctrl+Enter from a bare Enter requires the kitty keyboard
    /// protocol, which `init` enables.
    pub(in crate::app) fn is_submit_key(key_event: KeyEvent) -> bool {
        key_event.code == KeyCode::Enter && key_event.modifiers.contains(KeyModifiers::CONTROL)
    }

    pub(in crate::app) fn handle_insert_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
    ) -> Vec<UiEffect> {
        // The skill popup is an Insert-mode adornment: it intercepts only
        // navigation/accept/dismiss keys; everything else edits the composer
        // as usual (followed by a popup resync below).
        if self.skill_popup.is_some() {
            match key_event.code {
                _ if Self::is_submit_key(key_event) => {
                    self.skill_popup = None;
                    return self.request_insert_turn_start();
                }
                KeyCode::Esc => {
                    self.skill_popup = None;
                    return Vec::new();
                }
                KeyCode::Up => {
                    if let Some(popup) = self.skill_popup.as_mut() {
                        popup.move_up();
                    }
                    return Vec::new();
                }
                KeyCode::Down => {
                    if let Some(popup) = self.skill_popup.as_mut() {
                        popup.move_down();
                    }
                    return Vec::new();
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.accept_skill_completion();
                    return Vec::new();
                }
                _ => {}
            }
        }
        let effects = match key_event.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                // Arm the chord so Esc-Esc from the composer reaches
                // transcript-select mode.
                self.last_esc = Some(now);
                Vec::new()
            }
            _ if Self::is_submit_key(key_event) => self.request_insert_turn_start(),
            KeyCode::Enter => {
                self.composer.insert_char('\n');
                Vec::new()
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                Vec::new()
            }
            KeyCode::Delete => {
                self.composer.delete();
                Vec::new()
            }
            KeyCode::Tab => {
                self.composer.insert_str("    ");
                Vec::new()
            }
            KeyCode::Left => {
                self.composer.move_left();
                Vec::new()
            }
            KeyCode::Right => {
                self.composer.move_right();
                Vec::new()
            }
            KeyCode::Up => {
                self.composer.move_visual_up(self.composer_inner_width());
                Vec::new()
            }
            KeyCode::Down => {
                self.composer.move_visual_down(self.composer_inner_width());
                Vec::new()
            }
            KeyCode::Home => {
                self.composer.move_line_start();
                Vec::new()
            }
            KeyCode::End => {
                self.composer.move_line_end();
                Vec::new()
            }
            KeyCode::Char(ch)
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.composer.insert_char(ch);
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.sync_skill_popup();
        effects
    }

    /// Query for the skill popup: `Some(text after the leading slash, up to
    /// the cursor)` while the cursor sits inside a leading `/token` (no
    /// whitespace between the slash and the cursor), `None` otherwise.
    pub(in crate::app) fn skill_popup_query(&self) -> Option<String> {
        let text = self.composer.as_str();
        let rest = text.strip_prefix('/')?;
        let cursor = self.composer.cursor();
        if cursor < 1 {
            return None;
        }
        let before_cursor = rest.get(..cursor - 1)?;
        if before_cursor.contains(char::is_whitespace) {
            return None;
        }
        Some(before_cursor.to_string())
    }

    /// Open, refresh, or dismiss the skill popup to match the composer state.
    /// Skills are rescanned from disk each time the popup transitions to open,
    /// so newly added skills show up without restarting.
    pub(in crate::app) fn sync_skill_popup(&mut self) {
        let Some(query) = self.skill_popup_query() else {
            self.skill_popup = None;
            return;
        };
        if self.skill_popup.is_none() {
            let skills = tools::list_skills(
                &self.skills_root,
                crate::config_state::current().tools.max_skill_bytes,
            );
            if skills.is_empty() {
                return;
            }
            self.skill_popup = Some(SkillPopup::new(skills));
        }
        if let Some(popup) = self.skill_popup.as_mut() {
            popup.set_query(&query);
            if popup.is_empty() {
                self.skill_popup = None;
            }
        }
    }

    /// Replace the leading `/token` in the composer with the selected skill
    /// name and close the popup, leaving the cursor at the end of the text.
    pub(in crate::app) fn accept_skill_completion(&mut self) {
        let Some(name) = self
            .skill_popup
            .as_ref()
            .and_then(|popup| popup.selected_name())
            .map(str::to_string)
        else {
            self.skill_popup = None;
            return;
        };
        let text = self.composer.take_text();
        let token_end = text
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, _)| idx)
            .unwrap_or(text.len());
        let remainder = &text[token_end..];
        let replaced = if remainder.is_empty() {
            format!("/{name} ")
        } else {
            format!("/{name}{remainder}")
        };
        self.composer.set_text(replaced);
        self.skill_popup = None;
    }

    pub(in crate::app) fn handle_command_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        match key_event.code {
            KeyCode::Esc => {
                self.command.clear();
                self.mode = if self.screen == Screen::Workspace
                    && (self.question_picker.is_some() || self.plan_approval.is_some())
                {
                    UiMode::Overlay
                } else {
                    UiMode::Normal
                };
                Vec::new()
            }
            KeyCode::Enter => {
                let command = std::mem::take(&mut self.command);
                self.mode = UiMode::Normal;
                self.execute_command(&command)
            }
            KeyCode::Backspace => {
                self.command.pop();
                Vec::new()
            }
            KeyCode::Char(ch)
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.command.push(ch);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn execute_command(&mut self, command: &str) -> Vec<UiEffect> {
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "" => Vec::new(),
            "send" => self.request_turn_start(),
            "cancel" => self.request_turn_cancel(),
            "plan" => self.request_plan_toggle(),
            "quit" | "q" => vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)],
            "clear" => {
                self.clear_transcript();
                Vec::new()
            }
            "focus" => {
                let target = parts.next().unwrap_or_default();
                match target {
                    "transcript" | "main" => self.focus = FocusTarget::Transcript,
                    "inspector" | "side" => {
                        self.inspector_visible = true;
                        self.focus = FocusTarget::Inspector;
                    }
                    "composer" | "input" => {
                        self.focus = FocusTarget::Composer;
                        self.mode = UiMode::Insert;
                    }
                    "dashboard" => {
                        self.exit_transcript_select();
                        self.focus = FocusTarget::Dashboard;
                        self.screen = Screen::Dashboard;
                    }
                    _ => self.push_info("usage: :focus transcript|inspector|composer|dashboard"),
                }
                Vec::new()
            }
            "inspector" => {
                let action = parts.next().unwrap_or("toggle");
                match action {
                    "toggle" => self.toggle_inspector_visible(),
                    "show" => self.set_inspector_visible(true),
                    "hide" => self.set_inspector_visible(false),
                    _ => self.push_info("usage: :inspector toggle|show|hide"),
                }
                Vec::new()
            }
            "help" => {
                self.push_info(":send  :cancel  :plan  :quit  :clear  :focus transcript|inspector|composer|dashboard  :inspector toggle|show|hide  I toggle inspector  :help");
                Vec::new()
            }
            other => {
                self.push_error(format!("unknown command: {other}"));
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn dispatch_picker_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        let outcome = self
            .question_picker
            .as_mut()
            .map(|picker| picker.handle_key(key_event))
            .unwrap_or(PickerOutcome::None);
        match outcome {
            PickerOutcome::None => Vec::new(),
            PickerOutcome::Confirm(response) => {
                let Some(picker) = self.question_picker.take() else {
                    return Vec::new();
                };
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                let id = self.next_item_id();
                self.push_history(TranscriptItem::question_answers(id, &response.answers));
                vec![self.effect(
                    EffectContext::ServerRequest,
                    UiEffectKind::AnswerQuestion {
                        request_id: picker.request_id,
                        response,
                    },
                )]
            }
            PickerOutcome::Cancel => {
                let Some(picker) = self.question_picker.take() else {
                    return Vec::new();
                };
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                vec![
                    self.effect(
                        EffectContext::ServerRequest,
                        UiEffectKind::FailQuestion {
                            request_id: picker.request_id,
                            error: JsonRpcError::new(
                                -32001,
                                ErrorInfo::new("user_declined", "user declined to answer")
                                    .with_source("smooth-tui"),
                            ),
                        },
                    ),
                ]
            }
        }
    }

    pub(in crate::app) fn dispatch_plan_approval_key(
        &mut self,
        key_event: KeyEvent,
    ) -> Vec<UiEffect> {
        let outcome = self
            .plan_approval
            .as_mut()
            .map(|overlay| overlay.handle_key(key_event))
            .unwrap_or(PlanApprovalOutcome::None);
        match outcome {
            PlanApprovalOutcome::None => Vec::new(),
            PlanApprovalOutcome::Respond(response) => {
                let Some(overlay) = self.plan_approval.take() else {
                    return Vec::new();
                };
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                vec![self.effect(
                    EffectContext::ServerRequest,
                    UiEffectKind::RespondPlanApproval {
                        request_id: overlay.request_id,
                        response,
                    },
                )]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn insert_mode_paste_inserts_at_cursor_and_normalizes_newlines() {
        let mut model = workspace_insert_model();
        model.composer.set_text("ab".to_string());

        let _ = model.handle_key_event(key(KeyCode::Left));
        let effects = model.handle_terminal_event(CrosstermEvent::Paste("X\r\nY\rZ".to_string()));

        assert!(effects.is_empty());
        assert_eq!(model.composer.as_str(), "aX\nY\nZb");
        assert_eq!(model.composer.cursor(), "aX\nY\nZ".len());
    }

    #[test]
    fn paste_while_other_editing_inserts_into_answer() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(42),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![
                        AskUserQuestionOption {
                            label: "A".to_string(),
                            description: "Use option A".to_string(),
                            preview: None,
                        },
                        AskUserQuestionOption {
                            label: "B".to_string(),
                            description: "Use option B".to_string(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
            },
        }));

        // Move to the "Other" row and start editing, then paste.
        let _ = model.handle_key_event(key(KeyCode::Down));
        let _ = model.handle_key_event(key(KeyCode::Down));
        let _ = model.handle_key_event(key(KeyCode::Enter));
        let _ = model.handle_terminal_event(CrosstermEvent::Paste("pasted\nanswer".to_string()));
        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::AnswerQuestion { request_id, response }
                if *request_id == RequestId(42)
                    && response.answers[0].selected == vec!["pasted answer".to_string()]
        ));
    }

    #[test]
    fn insert_mode_left_and_right_edit_inside_composer() {
        let mut model = workspace_insert_model();
        model.composer.set_text("helo".to_string());

        let _ = model.handle_key_event(key(KeyCode::Left));
        let _ = model.handle_key_event(key(KeyCode::Char('l')));
        let _ = model.handle_key_event(key(KeyCode::Right));
        let _ = model.handle_key_event(key(KeyCode::Char('!')));

        assert_eq!(model.composer.as_str(), "hello!");
        assert_eq!(model.composer.cursor(), "hello!".len());
    }

    #[test]
    fn insert_mode_backspace_and_delete_edit_around_cursor() {
        let mut model = workspace_insert_model();
        model.composer.set_text("abc".to_string());

        let _ = model.handle_key_event(key(KeyCode::Left));
        let _ = model.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(model.composer.as_str(), "ac");
        assert_eq!(model.composer.cursor(), "a".len());

        let _ = model.handle_key_event(key(KeyCode::Delete));
        assert_eq!(model.composer.as_str(), "a");
        assert_eq!(model.composer.cursor(), "a".len());
    }

    #[test]
    fn insert_mode_home_and_end_move_within_current_line() {
        let mut model = workspace_insert_model();
        model.composer.set_text("abc\ndef".to_string());

        let _ = model.handle_key_event(key(KeyCode::Home));
        assert_eq!(model.composer.cursor(), "abc\n".len());

        let _ = model.handle_key_event(key(KeyCode::End));
        assert_eq!(model.composer.cursor(), "abc\ndef".len());
    }

    #[test]
    fn insert_mode_up_and_down_preserve_visual_column() {
        let mut model = workspace_insert_model();
        model.composer.set_text("abcdef\nx\nabcdef".to_string());

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.composer.cursor(), "abcdef\nx".len());

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.composer.cursor(), "abcdef".len());

        let _ = model.handle_key_event(key(KeyCode::Down));
        assert_eq!(model.composer.cursor(), "abcdef\nx".len());

        let _ = model.handle_key_event(key(KeyCode::Down));
        assert_eq!(model.composer.cursor(), "abcdef\nx\nabcdef".len());
    }

    #[test]
    fn insert_mode_up_and_down_use_wrapped_visual_rows() {
        let mut model = workspace_insert_model();
        model.terminal_width = 5;
        model.composer.set_text("abcdef".to_string());

        let _ = model.handle_key_event(key(KeyCode::Up));

        assert_eq!(model.composer.cursor(), "a".len());
    }

    #[test]
    fn ctrl_enter_sends_full_composer_text_and_clears_cursor_state() {
        let mut model = workspace_insert_model();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.composer.set_text("hello\nworld".to_string());

        let effects = model.handle_key_event(modified_key(KeyCode::Enter, KeyModifiers::CONTROL));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::TurnStart {
                thread_id: got,
                input,
            } if *got == thread_id && input == "hello\nworld"
        ));
        assert!(model.composer.is_empty());
        assert_eq!(model.composer.cursor(), 0);
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
    }

    #[test]
    fn typing_slash_opens_skill_popup_and_filters() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        let Some(popup) = model.skill_popup.as_ref() else {
            panic!("expected popup to open on '/'");
        };
        assert_eq!(popup.selected_name(), Some("deploy"));

        let _ = model.handle_key_event(key(KeyCode::Char('r')));
        let Some(popup) = model.skill_popup.as_ref() else {
            panic!("expected popup to stay open while filtering");
        };
        assert_eq!(popup.selected_name(), Some("review"));

        // No match closes the popup; backspacing back into a match reopens it.
        let _ = model.handle_key_event(key(KeyCode::Char('z')));
        assert!(model.skill_popup.is_none());
        let _ = model.handle_key_event(key(KeyCode::Backspace));
        assert!(model.skill_popup.is_some());
        Ok(())
    }

    #[test]
    fn slash_without_skills_does_not_open_popup() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::TempDir::new()?;
        let mut model = workspace_insert_model();
        model.skills_root = temp.path().to_path_buf();

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        assert!(model.skill_popup.is_none());
        Ok(())
    }

    #[test]
    fn slash_mid_text_does_not_open_popup() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;
        for ch in "run /".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        assert!(model.skill_popup.is_none());
        Ok(())
    }

    #[test]
    fn tab_accepts_selected_skill_completion() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        let _ = model.handle_key_event(key(KeyCode::Down));
        let _ = model.handle_key_event(key(KeyCode::Tab));

        assert_eq!(model.composer.as_str(), "/review ");
        assert!(model.skill_popup.is_none());
        assert_eq!(model.mode, UiMode::Insert);
        Ok(())
    }

    #[test]
    fn enter_accepts_skill_completion_instead_of_newline() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_temp, mut model) = skills_fixture()?;

        for ch in "/dep".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        let _ = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(model.composer.as_str(), "/deploy ");
        assert!(model.skill_popup.is_none());
        Ok(())
    }

    #[test]
    fn esc_closes_skill_popup_but_stays_in_insert_mode() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        assert!(model.skill_popup.is_some());

        let _ = model.handle_key_event(key(KeyCode::Esc));
        assert!(model.skill_popup.is_none());
        assert_eq!(model.mode, UiMode::Insert);
        assert_eq!(model.composer.as_str(), "/");

        // A second Esc leaves Insert mode as usual.
        let _ = model.handle_key_event(key(KeyCode::Esc));
        assert_eq!(model.mode, UiMode::Normal);
        Ok(())
    }

    #[test]
    fn ctrl_enter_with_skill_popup_open_still_submits() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);

        for ch in "/deploy".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        assert!(model.skill_popup.is_some());
        let effects = model.handle_key_event(modified_key(KeyCode::Enter, KeyModifiers::CONTROL));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::TurnStart { input, .. } if input == "/deploy"
        ));
        assert!(model.skill_popup.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
        Ok(())
    }

    #[test]
    fn super_enter_inserts_newline() {
        let mut model = workspace_insert_model();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.composer.set_text("hello".to_string());

        let effects = model.handle_key_event(modified_key(KeyCode::Enter, KeyModifiers::SUPER));

        assert!(effects.is_empty());
        assert_eq!(model.composer.as_str(), "hello\n");
    }

    #[test]
    fn dashboard_keys_do_not_dispatch_hidden_question_picker() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.dashboard.items = vec![ThreadListItem {
            thread_id: thread_id.to_string(),
            rollout_path: "session.jsonl".to_string(),
            created_at: "2026-05-31T00:00:00Z".to_string(),
            updated_at: "2026-05-31T00:01:00Z".to_string(),
            last_user_message: Some("hello".to_string()),
            last_assistant_message: None,
        }];
        model.question_picker = Some(QuestionPicker::new(
            RequestId(42),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        ));
        model.screen = Screen::Dashboard;

        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].kind,
            UiEffectKind::ThreadResume { thread_id: got } if got == thread_id
        ));
        assert!(model.question_picker.is_some());
    }

    #[test]
    fn dashboard_enter_resumes_selected_thread() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.dashboard.items = vec![ThreadListItem {
            thread_id: thread_id.to_string(),
            rollout_path: "session.jsonl".to_string(),
            created_at: "2026-05-31T00:00:00Z".to_string(),
            updated_at: "2026-05-31T00:01:00Z".to_string(),
            last_user_message: Some("hello".to_string()),
            last_assistant_message: None,
        }];

        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].kind,
            UiEffectKind::ThreadResume { thread_id: got } if got == thread_id
        ));
    }

    #[test]
    fn vim_modes_and_send_command_start_turn() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.composer.set_text("hello".to_string());

        let _ = model.handle_key_event(key(KeyCode::Char(':')));
        for ch in "send".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::TurnStart {
                thread_id: got,
                input,
            } if *got == thread_id && input == "hello"
        ));
        assert!(model.composer.is_empty());
        assert_eq!(model.composer.cursor(), 0);
    }

    #[test]
    fn ctrl_c_exits_across_modes() {
        for mode in [
            UiMode::Normal,
            UiMode::Insert,
            UiMode::Command,
            UiMode::Overlay,
        ] {
            let mut model = UiModel::new();
            model.screen = Screen::Workspace;
            model.mode = mode;
            if mode == UiMode::Overlay {
                model.question_picker = Some(QuestionPicker::new(
                    RequestId(1),
                    AskUserQuestionParams {
                        thread_id: "t".into(),
                        turn_id: "u".into(),
                        questions: Vec::new(),
                    },
                ));
            }
            let effects =
                model.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(effects.len(), 1);
            assert!(matches!(effects[0].kind, UiEffectKind::Exit));
        }
    }

    #[test]
    fn ctrl_c_cancels_running_turn_across_modes() {
        for mode in [
            UiMode::Normal,
            UiMode::Insert,
            UiMode::Command,
            UiMode::Overlay,
        ] {
            let mut model = UiModel::new();
            let thread_id = ThreadId::new();
            model.current_thread_id = Some(thread_id);
            model.screen = Screen::Workspace;
            model.mode = mode;
            model.is_turn_running = true;
            model.current_turn_id = Some("turn-1".to_string());
            if mode == UiMode::Overlay {
                model.question_picker = Some(QuestionPicker::new(
                    RequestId(1),
                    AskUserQuestionParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn-1".into(),
                        questions: Vec::new(),
                    },
                ));
            }

            let effects =
                model.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert_eq!(effects.len(), 1);
            assert!(matches!(
                effects[0].kind,
                UiEffectKind::TurnCancel { thread_id: got } if got == thread_id
            ));
            assert!(model.is_turn_cancelling);
            assert_eq!(model.status_line, "Cancelling turn");
        }
    }

    #[test]
    fn esc_does_not_cancel_running_turn_in_normal_mode() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Normal;
        model.is_turn_running = true;
        model.current_turn_id = Some("turn-1".to_string());

        let effects = model.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            effects.is_empty(),
            "interrupting is reserved for Ctrl-C; Esc must be a no-op"
        );
        assert!(!model.is_turn_cancelling);
    }

    #[test]
    fn esc_dismisses_overlay_instead_of_cancelling_turn() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Overlay;
        model.is_turn_running = true;
        model.current_turn_id = Some("turn-1".to_string());
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: vec![app_server_protocol::AskUserQuestion {
                    question: "Proceed?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![app_server_protocol::AskUserQuestionOption {
                        label: "Yes".to_string(),
                        description: "go ahead".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        ));

        let effects = model.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::TurnCancel { .. })),
            "overlay Esc must dismiss the picker, not cancel the turn"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::FailQuestion { .. })),
            "overlay Esc should decline the question"
        );
        assert!(!model.is_turn_cancelling);
        assert!(model.question_picker.is_none());
    }

    #[test]
    fn esc_in_insert_mode_returns_to_normal_even_while_turn_running() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Insert;
        model.is_turn_running = true;

        let effects = model.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::TurnCancel { .. })),
            "insert-mode Esc must switch modes, not cancel the turn"
        );
        assert_eq!(model.mode, UiMode::Normal);
        assert!(!model.is_turn_cancelling);
    }

    #[test]
    fn double_esc_within_window_enters_transcript_select() {
        let mut model = select_model_with_items(3);
        enter_select(&mut model, Instant::now());

        assert_eq!(model.mode, UiMode::TranscriptSelect);
        assert!(!model.auto_scroll);
        assert_eq!(model.transcript_select.map(|state| state.selected), Some(2));
    }

    #[test]
    fn slow_double_esc_stays_normal() {
        let mut model = select_model_with_items(1);
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(600));

        assert_eq!(model.mode, UiMode::Normal);
        assert!(model.transcript_select.is_none());
    }

    #[test]
    fn esc_from_insert_then_esc_enters_select() {
        let mut model = workspace_insert_model();
        let id = model.next_item_id();
        model.push_history(TranscriptItem::info(id, "row"));

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        assert_eq!(model.mode, UiMode::Normal);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));

        assert_eq!(model.mode, UiMode::TranscriptSelect);
    }

    #[test]
    fn key_between_escs_breaks_chord() {
        let mut model = select_model_with_items(1);
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(50));
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));

        assert_eq!(model.mode, UiMode::Normal);
    }

    #[test]
    fn double_esc_on_empty_transcript_stays_normal() {
        let mut model = workspace_normal_model();
        enter_select(&mut model, Instant::now());

        assert_eq!(model.mode, UiMode::Normal);
        assert!(model.transcript_select.is_none());
        assert_eq!(model.status_line, "Transcript is empty");
    }

    #[test]
    fn select_navigation_clamps_and_scrolls() {
        let mut model = select_model_with_items(8);
        model.viewport_height = 4;
        model.transcript_inner_width = 40;
        let t0 = Instant::now();
        enter_select(&mut model, t0);

        // Starts on the last item, scrolled so it is visible.
        assert_eq!(model.transcript_select.map(|state| state.selected), Some(7));
        assert!(model.scroll > 0);

        let t = t0 + Duration::from_millis(200);
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t);
        assert_eq!(
            model.transcript_select.map(|state| state.selected),
            Some(7),
            "j clamps at the last item"
        );

        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t);
        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t);
        assert_eq!(
            model.transcript_select.map(|state| state.selected),
            Some(0),
            "gg jumps to the first item"
        );
        assert_eq!(model.scroll, 0, "jumping to the first item scrolls to top");

        let _ = model.handle_key_event_at(key(KeyCode::Char('k')), t);
        assert_eq!(
            model.transcript_select.map(|state| state.selected),
            Some(0),
            "k clamps at the first item"
        );
    }

    #[test]
    fn select_esc_and_q_exit_without_app_exit() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut model = select_model_with_items(2);
            let t0 = Instant::now();
            enter_select(&mut model, t0);
            assert_eq!(model.mode, UiMode::TranscriptSelect);

            let effects = model.handle_key_event_at(key(code), t0 + Duration::from_secs(2));
            assert!(effects.is_empty(), "{code:?} must not exit the app");
            assert_eq!(model.mode, UiMode::Normal);
            assert!(model.transcript_select.is_none());
        }
    }

    fn complete_tool_call_with_output(app: &mut App, event_id: &str, call_id: &str, output: &str) {
        app.handle_session_event(
            event(
                event_id,
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: call_id.to_owned(),
                    success: true,
                    output_preview: Some(output.to_owned()),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: None,
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );
    }

    #[test]
    fn y_copies_tool_result_then_second_y_copies_args() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{\"cmd\":\"ls\"}");
        complete_tool_call(&mut app, "2", "call-1", true, None);

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        assert_eq!(app.model.mode, UiMode::TranscriptSelect);

        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some("BIG CONTENT"));

        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(300));
        assert_eq!(clipboard_content(&effects), Some("{\"cmd\":\"ls\"}"));

        // After the chord window the next y copies the result again.
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(1200));
        assert_eq!(clipboard_content(&effects), Some("BIG CONTENT"));
    }

    #[test]
    fn y_on_running_tool_falls_back_to_args() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "1",
            "call-1",
            "run_command",
            "{\"cmd\":\"sleep\"}",
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some("{\"cmd\":\"sleep\"}"));
    }

    #[test]
    fn batch_tool_selection_moves_between_entries_and_y_copies_one_entry() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{\"cmd\":\"one\"}");
        start_tool_call(&mut app, "2", "call-2", "run_command", "{\"cmd\":\"two\"}");
        complete_tool_call_with_output(&mut app, "3", "call-1", "output one");
        complete_tool_call_with_output(&mut app, "4", "call-2", "output two");

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        assert_eq!(
            app.model
                .transcript_select
                .and_then(|state| state.selected_tool_entry),
            Some(1),
            "selecting a final batch row starts on its last entry"
        );

        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(100));
        assert_eq!(clipboard_content(&effects), Some("output two"));

        let _ = app
            .model
            .handle_key_event_at(key(KeyCode::Char('k')), t0 + Duration::from_millis(200));
        assert_eq!(
            app.model
                .transcript_select
                .and_then(|state| state.selected_tool_entry),
            Some(0)
        );
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(300));
        assert_eq!(clipboard_content(&effects), Some("output one"));
    }

    #[test]
    fn yy_on_batch_tool_entry_copies_only_that_entry_args() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{\"cmd\":\"one\"}");
        start_tool_call(&mut app, "2", "call-2", "run_command", "{\"cmd\":\"two\"}");
        complete_tool_call_with_output(&mut app, "3", "call-1", "output one");
        complete_tool_call_with_output(&mut app, "4", "call-2", "output two");

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let _ = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(100));
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some("{\"cmd\":\"two\"}"));
    }

    #[test]
    fn y_on_running_batch_tool_entry_falls_back_to_that_entry_args() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{\"cmd\":\"one\"}");
        start_tool_call(&mut app, "2", "call-2", "run_command", "{\"cmd\":\"two\"}");

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(100));
        assert_eq!(clipboard_content(&effects), Some("{\"cmd\":\"two\"}"));
    }

    #[test]
    fn batch_tool_selection_leaves_row_after_last_entry() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{\"cmd\":\"one\"}");
        start_tool_call(&mut app, "2", "call-2", "run_command", "{\"cmd\":\"two\"}");
        complete_agent_message(&mut app, "3", "assistant-1", "done");

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let _ = app
            .model
            .handle_key_event_at(key(KeyCode::Char('k')), t0 + Duration::from_millis(100));
        assert_eq!(
            app.model
                .transcript_select
                .and_then(|state| state.selected_tool_entry),
            Some(1)
        );
        let _ = app
            .model
            .handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(200));
        assert_eq!(
            app.model.transcript_select.map(|state| state.selected),
            Some(1),
            "j from the last batch entry moves to the next transcript item"
        );
    }

    #[test]
    fn y_copies_user_message_and_assistant_raw_markdown() {
        let mut app = App::new();
        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::UserMessage {
                    text: String::from("hello there"),
                },
            ),
            20,
        );
        let markdown = "answer:\n\n```rust\nlet x = 1;\n```";
        complete_agent_message(&mut app, "2", "assistant-1", markdown);

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        // Last item is the assistant message.
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some(markdown));

        let t = t0 + Duration::from_secs(2);
        let _ = app.model.handle_key_event_at(key(KeyCode::Char('k')), t);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t + Duration::from_millis(100));
        assert_eq!(clipboard_content(&effects), Some("hello there"));
    }

    #[test]
    fn turn_interrupted_exits_select_mode() {
        let mut model = select_model_with_items(2);
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        enter_select(&mut model, Instant::now());
        assert_eq!(model.mode, UiMode::TranscriptSelect);

        model.apply_protocol_event(event(
            "interrupted",
            EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                reason: "interrupted".to_string(),
            }),
        ));

        assert_eq!(model.mode, UiMode::Normal);
        assert!(model.transcript_select.is_none());
    }

    #[test]
    fn clipboard_payload_truncates_on_char_boundary() {
        let long = "é".repeat(MAX_CLIPBOARD_BYTES);
        let (clipped, truncated) = clip_for_clipboard(long);
        assert!(truncated);
        assert!(clipped.len() <= MAX_CLIPBOARD_BYTES);
        assert!(clipped.chars().all(|c| c == 'é'));

        let (kept, truncated) = clip_for_clipboard(String::from("short"));
        assert!(!truncated);
        assert_eq!(kept, "short");
    }
}
