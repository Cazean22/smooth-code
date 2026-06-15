use super::*;

impl UiModel {
    pub(in crate::app) fn pop_preview_back(&mut self) -> Vec<UiEffect> {
        let Some(popped) = self.preview_stack.pop() else {
            return Vec::new();
        };
        let thread_id = popped.thread_id;
        self.preview_forward_stack.push(thread_id);
        self.status_line = match self.preview_stack.last() {
            Some(top) => format!("{} — Ctrl-O back, Ctrl-I forward", top.header_label()),
            None => String::from("Returned to parent session — Ctrl-I forward"),
        };
        vec![self.effect(
            EffectContext::ThreadUnwatch,
            UiEffectKind::ThreadUnwatch { thread_id },
        )]
    }

    pub(in crate::app) fn reopen_forward_preview(&mut self) -> Vec<UiEffect> {
        let Some(thread_id) = self.preview_forward_stack.pop() else {
            return Vec::new();
        };
        self.status_line = String::from("Opening subagent…");
        vec![self.effect(
            EffectContext::ThreadPreview { thread_id },
            UiEffectKind::ThreadPreview { thread_id },
        )]
    }

    /// Keys while a subagent preview is open. The top view owns the keyboard
    /// and starts in a Normal-like scroll sub-mode; `Esc Esc` switches it into
    /// a transcript-select sub-mode (mirroring the main view) for `Enter`/copy.
    pub(in crate::app) fn handle_preview_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
    ) -> Vec<UiEffect> {
        let width = self.terminal_width.max(1);
        let viewport = self.viewport_height;
        let select_mode = match self.preview_stack.last() {
            Some(view) => view.select_mode,
            None => return Vec::new(),
        };
        if select_mode {
            self.handle_preview_select_key(key_event, now, width, viewport)
        } else {
            self.handle_preview_scroll_key(key_event, now, width, viewport)
        }
    }

    /// A preview's default sub-mode: Normal-like line/page scrolling with no
    /// selection highlight. `Ctrl-O` pops the preview (one `ThreadUnwatch` per
    /// pop — the server refcounts watchers); `Esc Esc` enters the select sub-mode.
    pub(in crate::app) fn handle_preview_scroll_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
        width: u16,
        viewport: u16,
    ) -> Vec<UiEffect> {
        if Self::is_ctrl_o(key_event) {
            return self.pop_preview_back();
        }
        let Some(view) = self.preview_stack.last_mut() else {
            return Vec::new();
        };
        match key_event.code {
            KeyCode::Esc => {
                let chord = view
                    .pending_esc
                    .is_some_and(|t| now.duration_since(t) <= DOUBLE_ESC_WINDOW);
                if chord {
                    view.pending_esc = None;
                    view.pending_g = None;
                    view.select_mode = true;
                    view.selected = view.first_visible_item(width);
                    view.selected_tool_entry =
                        view.default_tool_entry_for_selection(view.selected, false);
                    view.ensure_selected_visible(width, viewport);
                    self.status_line = String::from(
                        "Subagent select — j/k move, Enter open, Esc scroll, Ctrl-O back",
                    );
                } else {
                    view.pending_esc = Some(now);
                }
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.pending_esc = None;
                view.pending_g = None;
                view.scroll = view.scroll.saturating_sub(1);
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.pending_esc = None;
                view.pending_g = None;
                let max = view.max_scroll(width, viewport);
                view.scroll = view.scroll.saturating_add(1).min(max);
                view.auto_scroll = view.scroll >= max;
                Vec::new()
            }
            KeyCode::Char('g') => {
                view.pending_esc = None;
                let chord = view
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                if chord {
                    view.pending_g = None;
                    view.scroll = 0;
                    view.auto_scroll = false;
                } else {
                    view.pending_g = Some(now);
                }
                Vec::new()
            }
            KeyCode::Enter => {
                view.pending_esc = None;
                view.pending_g = None;
                Vec::new()
            }
            KeyCode::Home => {
                view.pending_esc = None;
                view.pending_g = None;
                view.scroll = 0;
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::End | KeyCode::Char('G') => {
                view.pending_esc = None;
                view.pending_g = None;
                view.auto_scroll = true;
                view.scroll_to_bottom(width, viewport);
                Vec::new()
            }
            KeyCode::PageUp => {
                view.pending_esc = None;
                view.pending_g = None;
                view.scroll = view.scroll.saturating_sub(viewport);
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::PageDown => {
                view.pending_esc = None;
                view.pending_g = None;
                let max = view.max_scroll(width, viewport);
                view.scroll = view.scroll.saturating_add(viewport).min(max);
                view.auto_scroll = view.scroll >= max;
                Vec::new()
            }
            _ => {
                view.pending_esc = None;
                view.pending_g = None;
                Vec::new()
            }
        }
    }

    /// A preview's transcript-select sub-mode (entered with `Esc Esc`): a
    /// highlighted row, `j`/`k` move the selection, `gg`/`G`/Home/End move it,
    /// `Enter` nests a deeper preview, `y`/`yy` copy. `Esc` returns to scroll and
    /// `Ctrl-O` returns to the parent preview/session.
    pub(in crate::app) fn handle_preview_select_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
        width: u16,
        viewport: u16,
    ) -> Vec<UiEffect> {
        if Self::is_ctrl_o(key_event) {
            return self.pop_preview_back();
        }
        let Some(view) = self.preview_stack.last_mut() else {
            return Vec::new();
        };
        let last = view.item_count().saturating_sub(1);
        match key_event.code {
            KeyCode::Esc => {
                view.select_mode = false;
                view.pending_g = None;
                view.pending_esc = None;
                view.pending_args = None;
                self.status_line =
                    String::from("Subagent preview — Ctrl-O back, Ctrl-I forward, Esc Esc select");
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.pending_g = None;
                view.pending_args = None;
                view.move_selection_up();
                view.ensure_selected_visible(width, viewport);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.pending_g = None;
                view.pending_args = None;
                view.move_selection_down();
                view.ensure_selected_visible(width, viewport);
                Vec::new()
            }
            KeyCode::Char('g') => {
                view.pending_args = None;
                let chord = view
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                if chord {
                    view.pending_g = None;
                    view.selected = 0;
                    view.selected_tool_entry = view.default_tool_entry_for_selection(0, false);
                    view.ensure_selected_visible(width, viewport);
                } else {
                    view.pending_g = Some(now);
                }
                Vec::new()
            }
            KeyCode::Enter => {
                view.pending_g = None;
                view.pending_args = None;
                let target = view.selected_tool_group().map(|group| {
                    let thread_id = if group.is_batch() {
                        view.selected_tool_entry
                            .and_then(|entry_idx| group.entry_subagent_thread_id(entry_idx))
                    } else {
                        group.subagent_thread_id()
                    };
                    (group.is_spawn_agent(), thread_id)
                });
                match target {
                    Some((_, Some(thread_id))) => {
                        self.preview_forward_stack.clear();
                        self.status_line = String::from("Opening subagent…");
                        vec![self.effect(
                            EffectContext::ThreadPreview { thread_id },
                            UiEffectKind::ThreadPreview { thread_id },
                        )]
                    }
                    Some((true, None)) => {
                        self.status_line =
                            String::from("Subagent not started yet — no session to open");
                        Vec::new()
                    }
                    _ => {
                        self.status_line =
                            String::from("Not a subagent row (Enter opens spawn_agent sessions)");
                        Vec::new()
                    }
                }
            }
            KeyCode::Char('y') => self.copy_preview_selected_row(now),
            KeyCode::Home => {
                view.pending_g = None;
                view.pending_args = None;
                view.selected = 0;
                view.selected_tool_entry = view.default_tool_entry_for_selection(0, false);
                view.ensure_selected_visible(width, viewport);
                Vec::new()
            }
            KeyCode::End | KeyCode::Char('G') => {
                view.pending_g = None;
                view.pending_args = None;
                view.selected = last;
                view.selected_tool_entry = view.default_tool_entry_for_selection(last, true);
                view.ensure_selected_visible(width, viewport);
                view.auto_scroll = true;
                Vec::new()
            }
            KeyCode::PageUp => {
                view.pending_g = None;
                view.pending_args = None;
                view.scroll = view.scroll.saturating_sub(viewport);
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::PageDown => {
                view.pending_g = None;
                view.pending_args = None;
                let max = view.max_scroll(width, viewport);
                view.scroll = view.scroll.saturating_add(viewport).min(max);
                view.auto_scroll = view.scroll >= max;
                Vec::new()
            }
            _ => {
                view.pending_g = None;
                view.pending_args = None;
                Vec::new()
            }
        }
    }

    /// `y` in a preview's select sub-mode. A reduced mirror of
    /// `copy_selected_transcript_row` over the active view's own items: tool
    /// rows copy their result first, a second `y` upgrades to the arguments.
    pub(in crate::app) fn copy_preview_selected_row(&mut self, now: Instant) -> Vec<UiEffect> {
        // (payload, status message, next `pending_args`) resolved from the
        // selected item before the immutable borrow is released.
        type Resolved = (
            String,
            &'static str,
            Option<(usize, Option<usize>, Instant)>,
        );
        let Some(view) = self.preview_stack.last_mut() else {
            return Vec::new();
        };
        view.pending_g = None;
        let idx = view.selected;
        let prev_pending = view.pending_args;
        // Resolve the payload from an immutable borrow of the view's items,
        // then release it before mutating `view`/`self`.
        let resolved: Option<Resolved> = {
            let Some(item) = view.selected_item() else {
                view.pending_args = None;
                return Vec::new();
            };
            if let Some(group) = item.tool_group_cell() {
                let selected_entry = if group.is_batch() {
                    Some(
                        view.selected_tool_entry
                            .unwrap_or(0)
                            .min(group.entry_count().saturating_sub(1)),
                    )
                } else {
                    None
                };
                let chord = prev_pending.is_some_and(|(p, e, t)| {
                    p == idx && e == selected_entry && now.duration_since(t) <= COPY_CHORD_WINDOW
                });
                if let Some(entry_idx) = selected_entry {
                    Some(if chord {
                        match group.copy_entry_args(entry_idx) {
                            Some(args) => (args, "Copied tool arguments", None),
                            None => return Vec::new(),
                        }
                    } else if let Some(result) = group.copy_entry_result(entry_idx) {
                        (
                            result,
                            "Copied tool result — y again for arguments",
                            Some((idx, selected_entry, now)),
                        )
                    } else {
                        match group.copy_entry_args(entry_idx) {
                            Some(args) => (args, "No result yet — copied tool arguments", None),
                            None => return Vec::new(),
                        }
                    })
                } else {
                    Some(if chord {
                        (group.copy_args(), "Copied tool arguments", None)
                    } else if let Some(result) = group.copy_result() {
                        (
                            result,
                            "Copied tool result — y again for arguments",
                            Some((idx, None, now)),
                        )
                    } else {
                        (
                            group.copy_args(),
                            "No result yet — copied tool arguments",
                            None,
                        )
                    })
                }
            } else {
                item.copy_text().map(|text| (text, "Copied", None))
            }
        };
        let Some((payload, status, next_pending)) = resolved else {
            view.pending_args = None;
            self.status_line = String::from("Nothing to copy");
            return Vec::new();
        };
        view.pending_args = next_pending;
        let (payload, truncated) = clip_for_clipboard(payload);
        self.status_line = if truncated {
            format!("{status} ({} bytes, truncated)", payload.len())
        } else {
            format!("{status} ({} bytes)", payload.len())
        };
        vec![self.effect(
            EffectContext::Clipboard,
            UiEffectKind::CopyToClipboard { content: payload },
        )]
    }

    /// `y` in transcript-select mode. Tool rows copy their result first; a
    /// second `y` within the chord window upgrades the clipboard to the tool
    /// arguments. Rows with nothing finished yet fall back to the arguments.
    pub(in crate::app) fn copy_selected_transcript_row(
        &mut self,
        mut state: TranscriptSelectState,
        now: Instant,
    ) -> Vec<UiEffect> {
        let idx = state.selected;
        let Some(item) = self.transcript_items.get(idx) else {
            self.transcript_select = Some(state);
            return Vec::new();
        };
        let (payload, status) = if let Some(group) = item.tool_group_cell() {
            let selected_entry = if group.is_batch() {
                Some(
                    state
                        .selected_tool_entry
                        .unwrap_or(0)
                        .min(group.entry_count().saturating_sub(1)),
                )
            } else {
                None
            };
            let chord = state.pending_args.is_some_and(|(p, e, t)| {
                p == idx && e == selected_entry && now.duration_since(t) <= COPY_CHORD_WINDOW
            });
            if let Some(entry_idx) = selected_entry {
                if chord {
                    state.pending_args = None;
                    match group.copy_entry_args(entry_idx) {
                        Some(args) => (args, "Copied tool arguments"),
                        None => {
                            self.transcript_select = Some(state);
                            self.status_line = String::from("Nothing to copy");
                            return Vec::new();
                        }
                    }
                } else if let Some(result) = group.copy_entry_result(entry_idx) {
                    state.pending_args = Some((idx, selected_entry, now));
                    (result, "Copied tool result — y again for arguments")
                } else {
                    state.pending_args = None;
                    match group.copy_entry_args(entry_idx) {
                        Some(args) => (args, "No result yet — copied tool arguments"),
                        None => {
                            self.transcript_select = Some(state);
                            self.status_line = String::from("Nothing to copy");
                            return Vec::new();
                        }
                    }
                }
            } else if chord {
                state.pending_args = None;
                (group.copy_args(), "Copied tool arguments")
            } else if let Some(result) = group.copy_result() {
                state.pending_args = Some((idx, None, now));
                (result, "Copied tool result — y again for arguments")
            } else {
                state.pending_args = None;
                (group.copy_args(), "No result yet — copied tool arguments")
            }
        } else {
            state.pending_args = None;
            match item.copy_text() {
                Some(text) => (text, "Copied"),
                None => {
                    self.transcript_select = Some(state);
                    self.status_line = String::from("Nothing to copy");
                    return Vec::new();
                }
            }
        };
        self.transcript_select = Some(state);
        let (payload, truncated) = clip_for_clipboard(payload);
        self.status_line = if truncated {
            format!("{status} ({} bytes, truncated)", payload.len())
        } else {
            format!("{status} ({} bytes)", payload.len())
        };
        vec![self.effect(
            EffectContext::Clipboard,
            UiEffectKind::CopyToClipboard { content: payload },
        )]
    }

    /// A successful `threadPreview` response: validate it against the
    /// requested thread and push the view. Any mismatch fails defensively —
    /// the server holds a watcher that only an unwatch will release, and no
    /// view means no pop will ever send one.
    pub(in crate::app) fn apply_thread_preview(
        &mut self,
        context: Option<EffectContext>,
        response: ThreadPreviewResponse,
    ) -> Vec<UiEffect> {
        let requested = match context {
            Some(EffectContext::ThreadPreview { thread_id }) => Some(thread_id),
            _ => None,
        };
        let response_thread = response.thread_id.parse::<ThreadId>().ok();
        let valid = match (requested, response_thread) {
            (Some(requested), Some(actual)) => requested == actual,
            _ => false,
        };
        if !valid {
            self.status_line = String::from("Could not open subagent: unexpected response");
            // Unwatch whichever id the server may have taken a watcher for.
            return requested
                .or(response_thread)
                .map(|thread_id| {
                    vec![self.effect(
                        EffectContext::ThreadUnwatch,
                        UiEffectKind::ThreadUnwatch { thread_id },
                    )]
                })
                .unwrap_or_default();
        }

        let width = self.terminal_width.max(1);
        let mut view = SubagentPreviewView::from_preview_response(response, width);
        view.scroll_to_bottom(width, self.viewport_height);
        self.status_line = if self.preview_forward_stack.is_empty() {
            format!("{} — Ctrl-O back", view.header_label())
        } else {
            format!("{} — Ctrl-O back, Ctrl-I forward", view.header_label())
        };
        self.preview_stack.push(view);
        Vec::new()
    }

    /// Pop every preview view, emitting one `ThreadUnwatch` per view — the
    /// server refcounts watchers, so each successful preview needs exactly
    /// one release, duplicates included.
    pub(in crate::app) fn clear_preview_stack(&mut self) -> Vec<UiEffect> {
        self.preview_forward_stack.clear();
        let views = std::mem::take(&mut self.preview_stack);
        views
            .into_iter()
            .map(|view| {
                self.effect(
                    EffectContext::ThreadUnwatch,
                    UiEffectKind::ThreadUnwatch {
                        thread_id: view.thread_id,
                    },
                )
            })
            .collect()
    }

    /// Full-screen subagent preview: a header naming the agent and its live
    /// status, the read-only transcript, and a key-hint footer.
    pub(in crate::app) fn render_preview(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        let depth = self.preview_stack.len();
        let Some(view) = self.preview_stack.last_mut() else {
            return;
        };
        let width = chunks[1].width.max(1);
        let viewport = chunks[1].height.max(1);
        let select_mode = view.select_mode;

        let mut header = view.header_label();
        if depth > 1 {
            header = format!("{header} · nested ×{}", depth - 1);
        }
        frame.render_widget(
            Paragraph::new(separator_line(
                area.width,
                &header,
                Style::default().fg(Color::Cyan),
            )),
            chunks[0],
        );

        if view.auto_scroll {
            view.scroll_to_bottom(width, viewport);
        }
        let lines = view.visible_lines(width, viewport);
        frame.render_widget(Paragraph::new(Text::from(lines)), chunks[1]);

        // The status line stays visible inside the preview: in-preview
        // feedback ("Subagent not started yet", failed nested opens) would
        // otherwise be invisible until the user exits.
        let hint = if select_mode {
            "j/k move  Enter nested  y copy selected tool  yy args  Esc scroll  Ctrl-O back"
        } else {
            "j/k scroll  gg/G top/bottom  Esc Esc select  Ctrl-O back  Ctrl-I forward"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(self.status_line.clone()),
                Span::raw("  "),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
            ])),
            chunks[2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    fn tool_started(thread_id: ThreadId, call_id: &str, args: &str) -> EventMsg {
        EventMsg::ToolCallStarted(ToolCallStartedEvent {
            thread_id: thread_id.to_string(),
            turn_id: String::from("turn-1"),
            call_id: call_id.to_owned(),
            tool_name: String::from("run_command"),
            args_preview: args.to_owned(),
        })
    }

    fn tool_completed(thread_id: ThreadId, call_id: &str, output: &str) -> EventMsg {
        EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
            thread_id: thread_id.to_string(),
            turn_id: String::from("turn-1"),
            call_id: call_id.to_owned(),
            success: true,
            output_preview: Some(output.to_owned()),
            error: None,
            result_kind: ToolCallResultKind::Final,
            related_thread_id: None,
            file_changes: Vec::new(),
            todos: Vec::new(),
        })
    }

    #[test]
    fn preview_batch_selection_y_copies_one_entry() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                tool_started(child, "call-1", "{\"cmd\":\"one\"}"),
                tool_started(child, "call-2", "{\"cmd\":\"two\"}"),
                tool_completed(child, "call-1", "output one"),
                tool_completed(child, "call-2", "output two"),
            ],
        );

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let view = model
            .preview_stack
            .last()
            .unwrap_or_else(|| panic!("preview view"));
        assert!(view.select_mode);
        assert_eq!(view.selected_tool_entry, Some(0));

        let effects =
            model.handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(100));
        assert_eq!(clipboard_content(&effects), Some("output one"));
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(150));
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some("output two"));
    }

    #[test]
    fn final_spawn_completion_records_related_thread_on_entry() {
        let mut app = App::new();
        let child_thread_id = ThreadId::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"inspect\",\"prompt\":\"inspect\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("{\"status\":\"completed\"}")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: Some(child_thread_id),
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let group = app
            .model
            .transcript_items
            .iter()
            .find_map(|item| item.tool_group_cell())
            .expect("spawn tool row");
        assert_eq!(group.subagent_thread_id(), Some(child_thread_id));
        assert!(group.is_spawn_agent());

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("↳ subagent"), "{joined}");
    }

    #[test]
    fn enter_on_subagent_row_emits_one_preview_effect_and_preserves_selection() {
        let mut app = App::new();
        let child_thread_id = ThreadId::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"inspect\",\"prompt\":\"inspect\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("{\"status\":\"running\"}")),
                    error: None,
                    result_kind: ToolCallResultKind::StatusUpdate,
                    related_thread_id: Some(child_thread_id),
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        assert_eq!(app.model.mode, UiMode::TranscriptSelect);
        let tool_row = app
            .model
            .transcript_items
            .iter()
            .position(|item| item.tool_group_cell().is_some())
            .expect("spawn tool row index");
        if let Some(state) = app.model.transcript_select.as_mut() {
            state.selected = tool_row;
        }
        app.model.preview_forward_stack.push(ThreadId::new());

        let t = t0 + Duration::from_millis(300);
        let effects = app.model.handle_key_event_at(key(KeyCode::Char('g')), t);
        assert!(effects.is_empty(), "g alone arms only the gg chord");
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('d')), t + Duration::from_millis(100));
        assert!(
            preview_targets(&effects).is_empty(),
            "gd no longer opens a subagent preview"
        );

        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Enter), t + Duration::from_millis(200));

        let preview_targets: Vec<ThreadId> = effects
            .iter()
            .filter_map(|effect| match effect.kind {
                UiEffectKind::ThreadPreview { thread_id } => Some(thread_id),
                _ => None,
            })
            .collect();
        assert_eq!(preview_targets, vec![child_thread_id]);
        assert!(app.model.preview_forward_stack.is_empty());
        assert_eq!(app.model.mode, UiMode::TranscriptSelect);
        assert_eq!(
            app.model.transcript_select.map(|state| state.selected),
            Some(tool_row),
            "Enter leaves the parent selection untouched"
        );
    }

    #[test]
    fn enter_on_plain_row_is_noop_with_status() {
        let mut model = select_model_with_items(3);
        let t0 = Instant::now();
        enter_select(&mut model, t0);

        let t = t0 + Duration::from_millis(300);
        let effects = model.handle_key_event_at(key(KeyCode::Enter), t);

        assert!(effects.is_empty());
        assert_eq!(
            model.status_line,
            "Not a subagent row (Enter opens spawn_agent sessions)"
        );
        assert_eq!(model.mode, UiMode::TranscriptSelect);
    }

    #[test]
    fn enter_on_spawn_row_without_child_id_reports_pending() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"inspect\",\"prompt\":\"inspect\"}",
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let tool_row = app
            .model
            .transcript_items
            .iter()
            .position(|item| item.tool_group_cell().is_some())
            .expect("spawn tool row index");
        if let Some(state) = app.model.transcript_select.as_mut() {
            state.selected = tool_row;
        }

        let t = t0 + Duration::from_millis(300);
        let effects = app.model.handle_key_event_at(key(KeyCode::Enter), t);

        assert!(effects.is_empty());
        assert_eq!(
            app.model.status_line,
            "Subagent not started yet — no session to open"
        );
    }

    #[test]
    fn enter_on_spawn_row_after_spawn_end_opens_before_tool_result() {
        let mut app = App::new();
        let child_thread_id = ThreadId::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"inspect\",\"prompt\":\"inspect\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                    call_id: String::from("c1"),
                    sender_thread_id: ThreadId::new(),
                    new_thread_id: Some(child_thread_id),
                    new_agent_nickname: Some(String::from("child")),
                    prompt: String::from("inspect"),
                    model: None,
                    status: AgentStatus::Running,
                }),
            ),
            20,
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let Some(tool_row) = app
            .model
            .transcript_items
            .iter()
            .position(|item| item.tool_group_cell().is_some())
        else {
            panic!("spawn tool row index");
        };
        if let Some(state) = app.model.transcript_select.as_mut() {
            state.selected = tool_row;
        }

        let t = t0 + Duration::from_millis(300);
        let effects = app.model.handle_key_event_at(key(KeyCode::Enter), t);

        let preview_targets: Vec<ThreadId> = effects
            .iter()
            .filter_map(|effect| match effect.kind {
                UiEffectKind::ThreadPreview { thread_id } => Some(thread_id),
                _ => None,
            })
            .collect();
        assert_eq!(preview_targets, vec![child_thread_id]);
        assert_eq!(app.model.status_line, "Opening subagent…");
    }

    #[test]
    fn preview_effect_result_pushes_view_and_replays_initial_messages()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                EventMsg::UserMessage {
                    text: String::from("inspect the rollout"),
                },
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: child.to_string(),
                    turn_id: String::from("0"),
                }),
            ],
        );

        assert_eq!(model.preview_stack.len(), 1);
        let view = model.preview_stack.last().ok_or("view")?;
        assert_eq!(view.thread_id, child);
        assert_eq!(view.item_count(), 1, "the user message becomes one item");
        // The response status wins over replayed turn-lifecycle events.
        assert_eq!(view.status, AgentStatus::Running);

        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("subagent worker"), "{rendered}");
        assert!(rendered.contains("running"), "{rendered}");
        assert!(rendered.contains("inspect the rollout"), "{rendered}");
        assert!(rendered.contains("Ctrl-O back"), "{rendered}");
        Ok(())
    }

    #[test]
    fn preview_renders_status_line_for_in_preview_enter_failures()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![EventMsg::UserMessage {
                text: String::from("not a tool row"),
            }],
        );

        // `Enter` on a non-subagent row inside the preview: the feedback must be
        // visible without leaving the preview. `Enter` opens only in the select
        // sub-mode, reached with `Esc Esc`.
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let effects =
            model.handle_key_event_at(key(KeyCode::Enter), t0 + Duration::from_millis(100));
        assert!(effects.is_empty());

        let mut terminal = Terminal::new(TestBackend::new(80, 12))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("Not a subagent row"), "{rendered}");
        Ok(())
    }

    #[test]
    fn child_events_route_to_every_matching_preview_not_main_transcript() {
        let mut model = workspace_normal_model();
        let parent = ThreadId::new();
        let child = ThreadId::new();
        model.current_thread_id = Some(parent);
        let main_items_before = model.transcript_items.len();

        // The same thread pushed twice: both views must receive the event.
        open_preview(&mut model, child, Vec::new());
        open_preview(&mut model, child, Vec::new());

        let effects = model.update(UiEvent::Protocol {
            source_thread_id: Some(child),
            event: Box::new(child_event(
                "c1",
                EventMsg::UserMessage {
                    text: String::from("child input"),
                },
            )),
            viewport_height: 20,
        });
        assert!(effects.is_empty());
        for view in &model.preview_stack {
            assert_eq!(view.item_count(), 1, "both duplicate views update");
        }
        assert_eq!(
            model.transcript_items.len(),
            main_items_before,
            "child events must not leak into the main transcript"
        );

        // Current-thread events still reach the main transcript.
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(parent),
            event: Box::new(child_event(
                "p1",
                EventMsg::UserMessage {
                    text: String::from("parent input"),
                },
            )),
            viewport_height: 20,
        });
        assert_eq!(model.transcript_items.len(), main_items_before + 1);
    }

    #[test]
    fn nested_parent_completion_patches_child_view_status() {
        let mut model = workspace_normal_model();
        let parent = ThreadId::new();
        let view_a = ThreadId::new();
        let view_b = ThreadId::new();
        model.current_thread_id = Some(parent);
        open_preview(&mut model, view_a, Vec::new());
        open_preview(&mut model, view_b, Vec::new());

        // B's completion arrives on A's channel (A spawned B): view B's
        // status must be patched even though source routing feeds view A.
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(view_a),
            event: Box::new(child_event(
                "done",
                EventMsg::CollabAgentCompleted(cazean_protocol::CollabAgentCompletedEvent {
                    parent_thread_id: view_a,
                    child_thread_id: view_b,
                    agent_path: cazean_protocol::AgentPath::try_from("/root/a/b")
                        .unwrap_or_else(|_| panic!("agent path")),
                    agent_nickname: Some(String::from("b")),
                    status: AgentStatus::Completed(Some(String::from("done"))),
                    last_assistant_message: Some(String::from("done")),
                }),
            )),
            viewport_height: 20,
        });

        let b = model
            .preview_stack
            .iter()
            .find(|view| view.thread_id == view_b)
            .unwrap_or_else(|| panic!("view B"));
        assert_eq!(b.status, AgentStatus::Completed(Some(String::from("done"))));
        assert!(!b.is_live);
        let a = model
            .preview_stack
            .iter()
            .find(|view| view.thread_id == view_a)
            .unwrap_or_else(|| panic!("view A"));
        assert_eq!(a.item_count(), 1, "view A renders the completion info row");
    }

    #[test]
    fn preview_ctrl_o_pops_one_view_and_ctrl_i_reopens_forward() {
        let mut model = select_model_with_items(4);
        let t0 = Instant::now();
        enter_select(&mut model, t0);
        if let Some(state) = model.transcript_select.as_mut() {
            state.selected = 2;
        }
        let parent_scroll = model.scroll;

        let child = ThreadId::new();
        open_preview(&mut model, child, Vec::new());
        open_preview(&mut model, child, Vec::new());

        let t = t0 + Duration::from_millis(300);
        let ctrl_o = modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let ctrl_i = modified_key(KeyCode::Char('i'), KeyModifiers::CONTROL);
        let effects = model.handle_key_event_at(ctrl_o, t);
        assert_eq!(
            unwatch_targets(&effects),
            vec![child],
            "first back unwatches"
        );
        assert_eq!(model.preview_stack.len(), 1);
        assert_eq!(model.preview_forward_stack, vec![child]);

        let effects = model.handle_key_event_at(ctrl_i, t + Duration::from_millis(50));
        assert_eq!(preview_targets(&effects), vec![child]);
        assert!(model.preview_forward_stack.is_empty());
        open_preview(&mut model, child, Vec::new());
        assert_eq!(model.preview_stack.len(), 2);

        let effects = model.handle_key_event_at(ctrl_o, t + Duration::from_millis(100));
        assert_eq!(
            unwatch_targets(&effects),
            vec![child],
            "second back unwatches"
        );
        assert_eq!(model.preview_stack.len(), 1);

        let effects = model.handle_key_event_at(ctrl_o, t + Duration::from_millis(150));
        assert_eq!(
            unwatch_targets(&effects),
            vec![child],
            "third back unwatches again — the server refcount drains to zero"
        );
        assert!(model.preview_stack.is_empty());

        assert_eq!(model.mode, UiMode::TranscriptSelect);
        assert_eq!(model.transcript_select.map(|state| state.selected), Some(2));
        assert_eq!(model.scroll, parent_scroll);
        assert_eq!(model.preview_forward_stack, vec![child, child]);

        let effects = model.handle_key_event_at(ctrl_i, t + Duration::from_millis(200));
        assert_eq!(preview_targets(&effects), vec![child]);
        assert_eq!(model.preview_forward_stack, vec![child]);
    }

    #[test]
    fn nested_enter_pushes_second_view_and_duplicates_are_dropped() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        let grandchild = ThreadId::new();
        let started = EventMsg::ToolCallStarted(ToolCallStartedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            tool_name: String::from("spawn_agent"),
            args_preview: String::from("{\"prompt\":\"dig deeper\"}"),
        });
        let completed = EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            success: true,
            output_preview: Some(String::from("{\"status\":\"completed\"}")),
            error: None,
            result_kind: ToolCallResultKind::Final,
            related_thread_id: Some(grandchild),
            file_changes: Vec::new(),
            todos: Vec::new(),
        });
        open_preview(&mut model, child, vec![started.clone(), completed.clone()]);
        let items_after_replay = model
            .preview_stack
            .last()
            .map(SubagentPreviewView::item_count)
            .unwrap_or_default();

        // The subscribe-then-snapshot overlap can replay both events again.
        for (id, msg) in [("dup-1", started), ("dup-2", completed)] {
            let _ = model.update(UiEvent::Protocol {
                source_thread_id: Some(child),
                event: Box::new(child_event(id, msg)),
                viewport_height: 20,
            });
        }
        let view = model
            .preview_stack
            .last()
            .unwrap_or_else(|| panic!("preview view"));
        assert_eq!(
            view.item_count(),
            items_after_replay,
            "duplicate started/completed events must not add rows"
        );

        // `Enter` on the spawn row nests a second preview. `Enter` lives in the
        // select sub-mode, reached with `Esc Esc`. A fresh `Enter` branch clears
        // any Ctrl-I forward history left by prior back navigation.
        model.preview_forward_stack.push(ThreadId::new());
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let effects =
            model.handle_key_event_at(key(KeyCode::Enter), t0 + Duration::from_millis(100));
        let targets = preview_targets(&effects);
        assert_eq!(targets, vec![grandchild]);
        assert!(model.preview_forward_stack.is_empty());
        open_preview(&mut model, grandchild, Vec::new());
        assert_eq!(model.preview_stack.len(), 2);
    }

    #[test]
    fn preview_opens_in_scroll_mode_and_jk_keeps_selection() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                EventMsg::UserMessage {
                    text: String::from("first"),
                },
                EventMsg::AgentMessage {
                    text: String::from("second"),
                },
                EventMsg::UserMessage {
                    text: String::from("third"),
                },
            ],
        );
        let view = model.preview_stack.last().expect("view");
        assert!(!view.select_mode, "preview opens in the scroll sub-mode");
        let selected_before = view.selected;

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Char('k')), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(10));
        let view = model.preview_stack.last().expect("view");
        assert!(!view.select_mode);
        assert_eq!(
            view.selected, selected_before,
            "scroll-mode j/k must not move the selection cursor"
        );
    }

    #[test]
    fn preview_double_esc_enters_select_mode_and_jk_moves_selection() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                EventMsg::UserMessage {
                    text: String::from("first"),
                },
                EventMsg::AgentMessage {
                    text: String::from("second"),
                },
                EventMsg::UserMessage {
                    text: String::from("third"),
                },
            ],
        );

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let view = model.preview_stack.last().expect("view");
        assert!(view.select_mode, "double-Esc enters the select sub-mode");
        let selected_after_enter = view.selected;

        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(100));
        let view = model.preview_stack.last().expect("view");
        assert!(view.select_mode);
        assert_eq!(
            view.selected,
            selected_after_enter + 1,
            "select-mode j moves the selection down"
        );
    }

    #[test]
    fn preview_enter_in_scroll_mode_does_not_open_nested() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        let grandchild = ThreadId::new();
        let started = EventMsg::ToolCallStarted(ToolCallStartedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            tool_name: String::from("spawn_agent"),
            args_preview: String::from("{\"prompt\":\"dig\"}"),
        });
        let completed = EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            success: true,
            output_preview: Some(String::from("{\"status\":\"completed\"}")),
            error: None,
            result_kind: ToolCallResultKind::Final,
            related_thread_id: Some(grandchild),
            file_changes: Vec::new(),
            todos: Vec::new(),
        });
        open_preview(&mut model, child, vec![started, completed]);

        // In scroll mode `Enter` must not open the nested subagent (it lives in
        // the select sub-mode); the preview stack is unchanged.
        let t0 = Instant::now();
        let effects = model.handle_key_event_at(key(KeyCode::Enter), t0);
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect.kind, UiEffectKind::ThreadPreview { .. })),
            "Enter in scroll mode must not emit a ThreadPreview effect"
        );
        assert_eq!(model.preview_stack.len(), 1);
    }

    #[test]
    fn preview_esc_exits_select_to_scroll_while_ctrl_o_pops_and_q_is_noop() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![EventMsg::UserMessage {
                text: String::from("hi"),
            }],
        );

        let t0 = Instant::now();
        // Enter select, then Esc back to scroll — the preview stays open.
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        assert!(model.preview_stack.last().expect("view").select_mode);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));
        let view = model.preview_stack.last().expect("view");
        assert!(!view.select_mode, "Esc in select returns to scroll");
        assert_eq!(
            model.preview_stack.len(),
            1,
            "Esc in select must not pop the preview"
        );

        // `q` no longer pops the preview.
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('q')), t0 + Duration::from_millis(150));
        assert!(effects.is_empty());
        assert_eq!(model.preview_stack.len(), 1);

        // `Ctrl-O` in scroll mode pops the preview with exactly one unwatch.
        let effects = model.handle_key_event_at(
            modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL),
            t0 + Duration::from_millis(200),
        );
        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![child]);
    }

    #[test]
    fn server_request_for_previewed_child_is_accepted_and_clears_stack() {
        let mut model = workspace_normal_model();
        let parent = ThreadId::new();
        let child = ThreadId::new();
        model.current_thread_id = Some(parent);
        open_preview(&mut model, child, Vec::new());

        let request = ServerRequest::AskUserQuestion {
            request_id: RequestId(7),
            params: AskUserQuestionParams {
                thread_id: child.to_string(),
                turn_id: String::from("0"),
                questions: vec![AskUserQuestion {
                    question: String::from("Proceed?"),
                    header: String::from("Confirm"),
                    multi_select: false,
                    options: vec![
                        AskUserQuestionOption {
                            label: String::from("Yes"),
                            description: String::from("go"),
                            preview: None,
                        },
                        AskUserQuestionOption {
                            label: String::from("No"),
                            description: String::from("stop"),
                            preview: None,
                        },
                    ],
                }],
            },
        };
        let effects = model.update(UiEvent::ServerRequest(request));
        assert!(model.question_picker.is_some(), "the picker is shown");
        assert!(model.preview_stack.is_empty(), "the stack is closed first");
        assert_eq!(unwatch_targets(&effects), vec![child]);

        // A request for an unrelated thread is still rejected.
        let unrelated = ServerRequest::AskUserQuestion {
            request_id: RequestId(8),
            params: AskUserQuestionParams {
                thread_id: ThreadId::new().to_string(),
                turn_id: String::from("0"),
                questions: Vec::new(),
            },
        };
        model.question_picker = None;
        let effects = model.update(UiEvent::ServerRequest(unrelated));
        assert!(model.question_picker.is_none());
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::FailServerRequest { .. }))
        );
    }

    #[test]
    fn preview_mid_stream_completion_uses_full_text_not_tail() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(&mut model, child, Vec::new());

        // The preview joined mid-stream: only the tail delta arrives.
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(child),
            event: Box::new(child_event(
                "d1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: child.to_string(),
                    turn_id: String::from("0"),
                    item_id: String::from("m1"),
                    delta: String::from("tail of the message"),
                }),
            )),
            viewport_height: 20,
        });
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(child),
            event: Box::new(child_event(
                "c1",
                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                    thread_id: child.to_string(),
                    turn_id: String::from("0"),
                    item_id: String::from("m1"),
                    text: String::from("the full message including the tail of the message"),
                }),
            )),
            viewport_height: 20,
        });

        let view = model
            .preview_stack
            .last()
            .unwrap_or_else(|| panic!("preview view"));
        assert_eq!(view.item_count(), 1, "one assistant item, not tail + full");
        let raw = view.items()[0]
            .copy_text()
            .unwrap_or_else(|| panic!("assistant raw text"));
        assert_eq!(
            raw, "the full message including the tail of the message",
            "the completed full text wins over the streamed tail"
        );
    }

    #[test]
    fn mismatched_preview_response_pushes_no_view_and_unwatches() {
        let mut model = workspace_normal_model();
        let requested = ThreadId::new();
        let other = ThreadId::new();

        let effect = model.effect(
            EffectContext::ThreadPreview {
                thread_id: requested,
            },
            UiEffectKind::ThreadPreview {
                thread_id: requested,
            },
        );
        let effects = model.update(UiEvent::EffectCompleted {
            effect_id: effect.effect_id,
            result: UiEffectResult::ThreadPreview(Box::new(preview_response(other, Vec::new()))),
            viewport_height: 20,
        });

        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![requested]);
        assert!(model.status_line.contains("Could not open subagent"));
    }

    #[test]
    fn preview_effect_failure_defensively_unwatches() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        let effect = model.effect(
            EffectContext::ThreadPreview { thread_id: child },
            UiEffectKind::ThreadPreview { thread_id: child },
        );
        let effects = model.update(UiEvent::EffectFailed {
            effect_id: effect.effect_id,
            error: String::from("rollout missing"),
            viewport_height: 20,
        });
        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![child]);
    }

    #[test]
    fn thread_switch_clears_preview_stack_with_unwatches() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(&mut model, child, Vec::new());

        let effect = model.effect(EffectContext::ThreadStart, UiEffectKind::ThreadStart);
        let effects = model.update(UiEvent::EffectCompleted {
            effect_id: effect.effect_id,
            result: UiEffectResult::ThreadStart(ThreadStartResponse {
                thread_id: ThreadId::new().to_string(),
                rollout_path: String::from("rollout.jsonl"),
            }),
            viewport_height: 20,
        });
        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![child]);
    }
}
