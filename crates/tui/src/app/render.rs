use super::*;

impl UiModel {
    /// Set the active assistant lines and invalidate the active-wrap cache by
    /// bumping the version. All writes to the active lines go through here so the
    /// memo in `refresh_active_wrap` can never go stale.
    pub(in crate::app) fn set_active_assistant_lines(&mut self, lines: Option<Vec<Line<'static>>>) {
        self.active_assistant_lines = lines;
        self.active_version = self.active_version.wrapping_add(1);
    }

    pub(in crate::app) fn set_active_reasoning_lines(&mut self, lines: Option<Vec<Line<'static>>>) {
        self.active_reasoning_lines = lines;
        self.active_version = self.active_version.wrapping_add(1);
    }

    /// Ensure `active_wrap` holds the active streams wrapped at `width` for the
    /// current `active_version`, recomputing only on a miss. The active streams
    /// mutate every delta so they stay out of `render_cache`; this memo keeps
    /// them from being re-wrapped twice per frame (row count + visible lines)
    /// and on idle frames where nothing streamed.
    pub(in crate::app) fn refresh_active_wrap(&mut self, width: u16) {
        if self
            .active_wrap
            .as_ref()
            .is_some_and(|cache| cache.width == width && cache.version == self.active_version)
        {
            return;
        }
        #[cfg(test)]
        {
            self.active_wrap_computes += 1;
        }
        let reasoning = self
            .active_reasoning_lines
            .as_ref()
            .map(|lines| {
                TranscriptItem::reasoning(0, lines.clone(), true, String::new())
                    .display_lines(width)
            })
            .unwrap_or_default();
        let assistant = self
            .active_assistant_lines
            .as_ref()
            .map(|lines| {
                TranscriptItem::assistant(0, lines.clone(), true, String::new())
                    .display_lines(width)
            })
            .unwrap_or_default();
        self.active_wrap = Some(ActiveWrap {
            width,
            version: self.active_version,
            reasoning,
            assistant,
        });
    }

    #[cfg(test)]
    pub(in crate::app) fn transcript_lines_uncached(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let item_count = self.transcript_items.len();
        let has_active_below = self.has_active_stream_lines();
        for (idx, item) in self.transcript_items.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            let mut item_lines = item.display_lines(width);
            if idx + 1 == item_count && !has_active_below && item.is_user() {
                item_lines.pop();
            }
            lines.extend(item_lines);
        }
        self.append_active_lines(&mut lines, width);
        if lines.is_empty() {
            lines.push(
                Line::from("No transcript yet. Type a message and use :send.")
                    .style(Style::default().dim()),
            );
        }
        lines
    }

    /// Whether an active reasoning/assistant stream is currently rendered below
    /// the committed transcript items. Used to decide whether the last user
    /// message has anything following it: when nothing does, its bottom
    /// separator is dropped so the transcript doesn't end on a dangling rule.
    pub(in crate::app) fn has_active_stream_lines(&self) -> bool {
        self.active_reasoning_lines
            .as_ref()
            .is_some_and(|lines| !lines.is_empty())
            || self
                .active_assistant_lines
                .as_ref()
                .is_some_and(|lines| !lines.is_empty())
    }

    pub(in crate::app) fn visible_transcript_lines(
        &mut self,
        width: u16,
        viewport_height: u16,
    ) -> Vec<Line<'static>> {
        let start = usize::from(self.scroll);
        let end = usize::from(self.scroll).saturating_add(usize::from(viewport_height));
        let mut all_rows = 0usize;
        let mut lines = Vec::new();

        let item_count = self.transcript_items.len();
        let has_active_below = self.has_active_stream_lines();
        let (selected_idx, selected_tool_entry) = if self.mode == UiMode::TranscriptSelect {
            self.transcript_select
                .map(|state| (Some(state.selected), state.selected_tool_entry))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        self.render_cache.evict_stale_widths(width);
        for (idx, item) in self.transcript_items.iter().enumerate() {
            if idx > 0 {
                append_visible_line(&mut lines, Line::default(), all_rows, start, end);
                all_rows += 1;
            }
            let omit_bottom = idx + 1 == item_count && !has_active_below && item.is_user();
            let mut item_height = self.render_cache.item_height(item, width);
            if omit_bottom {
                item_height = item_height.saturating_sub(1);
            }
            if all_rows >= end || all_rows.saturating_add(item_height) <= start {
                all_rows = all_rows.saturating_add(item_height);
                continue;
            }

            let mut item_lines = if selected_idx == Some(idx)
                && let Some(entry_idx) = selected_tool_entry
                && let Some(group) = item.tool_group_cell()
                && group.is_batch()
            {
                group.display_lines_with_selected_entry(
                    usize::from(width.max(1)),
                    Some(entry_idx.min(group.entry_count().saturating_sub(1))),
                )
            } else {
                self.render_cache.item_lines(item, width)
            };
            if omit_bottom {
                item_lines.pop();
            }
            // Highlight the selected row by patching the per-frame clone of
            // the cached lines; the cache itself stays untouched.
            if selected_idx == Some(idx) && selected_tool_entry.is_none() {
                for line in &mut item_lines {
                    line.style = line.style.patch(Style::default().bg(Color::DarkGray));
                }
            }
            for line in item_lines {
                append_visible_line(&mut lines, line, all_rows, start, end);
                all_rows += 1;
            }
        }

        self.refresh_active_wrap(width);
        let Some(active) = self.active_wrap.as_ref() else {
            return lines;
        };
        let has_reasoning = !active.reasoning.is_empty();
        let has_assistant = !active.assistant.is_empty();
        if (has_reasoning || has_assistant) && all_rows > 0 {
            append_visible_line(&mut lines, Line::default(), all_rows, start, end);
            all_rows += 1;
        }
        for line in &active.reasoning {
            append_visible_line(&mut lines, line.clone(), all_rows, start, end);
            all_rows += 1;
        }
        if has_reasoning && has_assistant {
            append_visible_line(&mut lines, Line::default(), all_rows, start, end);
            all_rows += 1;
        }
        for line in &active.assistant {
            append_visible_line(&mut lines, line.clone(), all_rows, start, end);
            all_rows += 1;
        }

        if all_rows == 0 {
            lines.push(
                Line::from("No transcript yet. Type a message and use :send.")
                    .style(Style::default().dim()),
            );
        }

        lines
    }

    #[cfg(test)]
    pub(in crate::app) fn append_active_lines(&self, lines: &mut Vec<Line<'static>>, width: u16) {
        if let Some(active_lines) = self.active_reasoning_lines.as_ref() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let item = TranscriptItem::reasoning(0, active_lines.clone(), true, String::new());
            lines.extend(item.display_lines(width));
        }
        if let Some(active_lines) = self.active_assistant_lines.as_ref() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let item = TranscriptItem::assistant(0, active_lines.clone(), true, String::new());
            lines.extend(item.display_lines(width));
        }
    }

    pub(in crate::app) fn total_transcript_rows(&mut self, width: u16) -> usize {
        if self.transcript_items.is_empty()
            && self.active_assistant_lines.is_none()
            && self.active_reasoning_lines.is_none()
        {
            return 1;
        }

        let item_count = self.transcript_items.len();
        let has_active_below = self.has_active_stream_lines();
        self.render_cache.evict_stale_widths(width);
        let mut rows = 0usize;
        for (idx, item) in self.transcript_items.iter().enumerate() {
            if idx > 0 {
                rows += 1;
            }
            let omit_bottom = idx + 1 == item_count && !has_active_below && item.is_user();
            let mut height = self.render_cache.item_height(item, width);
            if omit_bottom {
                height = height.saturating_sub(1);
            }
            rows += height;
        }

        self.refresh_active_wrap(width);
        let Some(active) = self.active_wrap.as_ref() else {
            return rows;
        };
        if !active.reasoning.is_empty() {
            if rows > 0 {
                rows += 1;
            }
            rows += active.reasoning.len();
        }
        if !active.assistant.is_empty() {
            if rows > 0 {
                rows += 1;
            }
            rows += active.assistant.len();
        }
        rows
    }

    pub(in crate::app) fn render(&mut self, frame: &mut Frame<'_>) {
        self.terminal_width = frame.area().width;
        match self.screen {
            Screen::Dashboard => self.render_dashboard(frame, frame.area()),
            Screen::Workspace if !self.preview_stack.is_empty() => {
                self.render_preview(frame, frame.area());
            }
            // An active plan-approval overlay owns the whole screen.
            Screen::Workspace
                if self
                    .plan_approval
                    .as_ref()
                    .is_some_and(PlanApprovalOverlay::is_active) =>
            {
                if let Some(overlay) = self.plan_approval.as_ref() {
                    overlay.render(frame, frame.area());
                }
            }
            Screen::Workspace => self.render_workspace(frame, frame.area()),
        }
    }

    pub(in crate::app) fn render_dashboard(&self, frame: &mut Frame<'_>, area: Rect) {
        self.render_dashboard_body(frame, area);
    }

    pub(in crate::app) fn render_dashboard_body(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("cazean", Style::default().bold().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("sessions", Style::default().dim()),
        ]));
        lines.push(Line::default());
        if self.dashboard.loading {
            lines.push(Line::from(Span::styled(
                "Loading recent sessions...",
                Style::default().dim(),
            )));
        } else if let Some(error) = &self.dashboard.error {
            lines.extend(wrap::wrap_line_hanging(
                Line::from(vec![
                    Span::styled("! ", Style::default().fg(Color::Red).bold()),
                    Span::styled(error.clone(), Style::default().fg(Color::Red)),
                ]),
                usize::from(area.width.max(1)),
                2,
            ));
        } else if self.dashboard.items.is_empty() {
            lines.push(Line::from("No saved sessions."));
        } else {
            let visible_count = self.dashboard_visible_item_count(area.height);
            let max_offset = self.dashboard_max_scroll_offset(visible_count);
            let start = self.dashboard.scroll_offset.min(max_offset);
            let end = start
                .saturating_add(visible_count)
                .min(self.dashboard.items.len());
            for (idx, item) in self.dashboard.items[start..end].iter().enumerate() {
                let item_idx = start + idx;
                let selected = item_idx == self.dashboard.selected;
                let prefix = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let preview = item
                    .last_user_message
                    .as_deref()
                    .or(item.last_assistant_message.as_deref())
                    .unwrap_or("(no messages)");
                // Keep each session two rows tall: truncate the preview to the
                // space left after the selection prefix, date, and gap so it
                // never wraps and breaks the dashboard scroll math.
                let used = wrap::display_width(prefix) + wrap::display_width(&item.updated_at) + 2;
                let preview = wrap::truncate_display(
                    preview,
                    usize::from(area.width.max(1)).saturating_sub(used),
                );
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(item.updated_at.clone(), Style::default().fg(Color::Yellow)),
                    Span::raw("  "),
                    Span::styled(preview, style),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(item.thread_id.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "n new  Enter resume  j/k move  PgUp/PgDn scroll  : command  Ctrl-C quit",
            Style::default().fg(Color::DarkGray),
        )));

        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(in crate::app) fn render_workspace(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let picker_height = self
            .question_picker
            .as_ref()
            .map(|picker| picker.desired_height(area.width).min(20))
            .unwrap_or(0);
        let command_height = if self.mode == UiMode::Command { 1 } else { 0 };
        let composer_height = self.composer_height();
        let skill_popup_height = self
            .skill_popup
            .as_ref()
            .map(|popup| popup.desired_height().min(8))
            .unwrap_or(0);

        let mut constraints = Vec::new();
        constraints.push(Constraint::Min(5));
        if picker_height > 0 {
            constraints.push(Constraint::Length(picker_height));
        }
        if skill_popup_height > 0 {
            constraints.push(Constraint::Length(skill_popup_height));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(composer_height));
        if command_height > 0 {
            constraints.push(Constraint::Length(command_height));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        self.render_workspace_body(frame, chunks[idx]);
        idx += 1;
        if picker_height > 0 {
            if let Some(picker) = &self.question_picker {
                picker.render(frame, chunks[idx]);
            }
            idx += 1;
        }
        if skill_popup_height > 0 {
            if let Some(popup) = &self.skill_popup {
                popup.render(frame, chunks[idx]);
            }
            idx += 1;
        }
        render_horizontal_separator(
            frame,
            chunks[idx],
            self.composer_title(),
            self.composer_accent_style(),
        );
        idx += 1;
        self.render_composer(frame, chunks[idx]);
        idx += 1;
        if command_height > 0 {
            self.render_command(frame, chunks[idx]);
            idx += 1;
        }
        render_horizontal_separator(frame, chunks[idx], "Status", muted_separator_style());
        idx += 1;
        self.render_status(frame, chunks[idx]);
    }

    pub(in crate::app) fn render_workspace_body(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let show_split = area.width >= 110 && self.inspector_visible;
        if show_split {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(70),
                    Constraint::Length(1),
                    Constraint::Percentage(30),
                ])
                .split(area);
            self.render_transcript(frame, chunks[0]);
            render_vertical_separator(frame, chunks[1]);
            self.render_inspector(frame, chunks[2]);
        } else if self.inspector_visible && self.focus == FocusTarget::Inspector {
            self.render_inspector(frame, area);
        } else {
            self.render_transcript(frame, area);
        }
    }

    pub(in crate::app) fn render_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner_width = area.width.max(1);
        let viewport_height = area.height.max(1);
        // Record the width we actually wrap/draw at so `max_scroll` (which also
        // runs from key handlers, before the next draw) counts the same rows.
        self.transcript_inner_width = inner_width;
        if self.auto_scroll {
            self.scroll_to_bottom(viewport_height);
        }
        let lines = self.visible_transcript_lines(inner_width, viewport_height);
        let paragraph = Paragraph::new(Text::from(lines));
        frame.render_widget(paragraph, area);
    }

    pub(in crate::app) fn render_inspector(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let wrap_width = usize::from(area.width.max(1));
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            "Inspector",
            Style::default().fg(Color::Cyan).bold(),
        )));
        lines.push(Line::default());
        lines.extend(wrap::wrap_line_hanging(
            Line::from(vec![
                Span::styled("Turn ", Style::default().fg(Color::Yellow).bold()),
                Span::raw(
                    self.current_turn_id
                        .as_deref()
                        .unwrap_or(if self.is_turn_running {
                            "running"
                        } else {
                            "idle"
                        })
                        .to_string(),
                ),
            ]),
            wrap_width,
            wrap::display_width("Turn "),
        ));
        lines.extend(wrap::wrap_line_hanging(
            Line::from(vec![
                Span::styled("Editor ", Style::default().fg(Color::Yellow).bold()),
                Span::raw(format!("{:?}", self.mode)),
            ]),
            wrap_width,
            wrap::display_width("Editor "),
        ));
        lines.extend(wrap::wrap_line_hanging(
            Line::from(vec![
                Span::styled("Agent ", Style::default().fg(Color::Yellow).bold()),
                Span::styled(
                    if self.plan_mode { "PLAN" } else { "FULL" },
                    if self.plan_mode {
                        Style::default().fg(Color::Magenta).bold()
                    } else {
                        Style::default().dim()
                    },
                ),
                Span::styled("  Shift+Tab", Style::default().dim()),
            ]),
            wrap_width,
            wrap::display_width("Agent "),
        ));
        if let Some(thread_id) = self.current_thread_id {
            lines.extend(wrap::wrap_line_hanging(
                Line::from(vec![
                    Span::styled("Thread ", Style::default().fg(Color::Yellow).bold()),
                    Span::styled(thread_id.to_string(), Style::default().fg(Color::DarkGray)),
                ]),
                wrap_width,
                wrap::display_width("Thread "),
            ));
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Running Tools",
            Style::default().bold(),
        )));
        if self.running_tools.is_empty() {
            lines.push(Line::from(Span::styled("none", Style::default().dim())));
        } else {
            for tool in self.running_tools.values() {
                // Align continuation rows under the args text (glyph + name + space).
                let indent = 2 + wrap::display_width(&tool.tool_name) + 1;
                lines.extend(wrap::wrap_line_hanging(
                    Line::from(vec![
                        Span::styled("⠋ ", Style::default().fg(Color::Yellow).bold()),
                        Span::raw(tool.tool_name.clone()),
                        Span::raw(" "),
                        Span::styled(tool.args_preview.clone(), Style::default().dim()),
                    ]),
                    wrap_width,
                    indent,
                ));
            }
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Recent File Changes",
            Style::default().bold(),
        )));
        if self.recent_file_changes.is_empty() {
            lines.push(Line::from(Span::styled("none", Style::default().dim())));
        } else {
            for change in self.recent_file_changes.iter().rev().take(4) {
                lines.extend(wrap::wrap_line_hanging(
                    Line::from(vec![
                        Span::raw("• "),
                        Span::raw(file_change_path_label(change)),
                    ]),
                    wrap_width,
                    2,
                ));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(in crate::app) fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = Vec::with_capacity(10);
        spans.push(Span::styled(
            "Status ",
            Style::default().fg(Color::Yellow).bold(),
        ));
        spans.push(Span::raw(self.status_line.clone()));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            if self.is_turn_cancelling {
                "agent cancelling"
            } else if self.is_turn_running {
                "agent running"
            } else {
                "agent idle"
            },
            Style::default().dim(),
        ));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{:?}", self.mode),
            Style::default().fg(Color::Cyan),
        ));
        if self.mode == UiMode::TranscriptSelect {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "j/k move  y copy selected tool  yy copy selected args  gg top  G bottom  Enter subagent  Esc exit",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if self.plan_mode {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "⏸ PLAN MODE",
                Style::default().fg(Color::Magenta).bold(),
            ));
        }
        if matches!(self.mode, UiMode::Normal | UiMode::Insert) {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "Shift+Tab plan/full",
                Style::default().fg(Color::DarkGray),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    pub(in crate::app) fn render_command(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(":", Style::default().fg(Color::Cyan).bold()),
                Span::raw(self.command.clone()),
            ])),
            area,
        );
    }

    pub(in crate::app) fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        let composer_width = area.width.max(1);
        let visible_height = area.height.max(1);
        let rows = self.composer.visual_rows(usize::from(composer_width));
        let (cursor_row, cursor_col) = self.composer.cursor_visual_position_in_rows(&rows);
        let scroll_row = cursor_row
            .saturating_add(1)
            .saturating_sub(usize::from(visible_height));
        let visible_lines: Vec<Line<'static>> = rows
            .iter()
            .skip(scroll_row)
            .take(usize::from(visible_height))
            .map(|row| Line::raw(self.composer.as_str()[row.start..row.end].to_owned()))
            .collect();

        frame.render_widget(Paragraph::new(Text::from(visible_lines)), area);

        if self.mode == UiMode::Insert && !self.is_turn_running {
            let inner_width = usize::from(composer_width);
            let x_offset = cursor_col.min(inner_width.saturating_sub(1));
            let y_offset = cursor_row
                .saturating_sub(scroll_row)
                .min(usize::from(visible_height.saturating_sub(1)));
            let x = area
                .x
                .saturating_add(u16::try_from(x_offset).unwrap_or(u16::MAX));
            let y = area
                .y
                .saturating_add(u16::try_from(y_offset).unwrap_or(u16::MAX));
            frame.set_cursor_position((x, y.min(area.y.saturating_add(area.height - 1))));
        }
    }

    pub(in crate::app) fn composer_title(&self) -> &'static str {
        if self.plan_mode {
            "Input (plan)"
        } else {
            "Input"
        }
    }

    pub(in crate::app) fn composer_accent_style(&self) -> Style {
        if self.plan_mode {
            Style::default().fg(Color::Magenta)
        } else if self.is_turn_running {
            Style::default().fg(Color::DarkGray)
        } else if self.mode == UiMode::Insert {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    pub(in crate::app) fn composer_height(&self) -> u16 {
        let rows = self
            .composer
            .visual_rows(self.composer_inner_width())
            .len()
            .max(1);
        u16::try_from(rows).unwrap_or(4).clamp(1, 4)
    }

    pub(in crate::app) fn composer_inner_width(&self) -> usize {
        usize::from(self.terminal_width.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn assistant_message_delta_complete_and_turn_complete_render_once() {
        let mut app = App::new();

        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("Hi! What can I help with?\n"),
                }),
            ),
            20,
        );
        complete_agent_message(&mut app, "2", "assistant-1", "Hi! What can I help with?");
        app.handle_session_event(
            event(
                "3",
                EventMsg::TurnCompleted(TurnCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    last_assistant_message: Some(String::from("Hi! What can I help with?")),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Hi! What can I help with?").count(), 1);
    }

    #[test]
    fn completed_assistant_message_followed_by_agent_message_renders_once() {
        let mut app = App::new();

        start_turn(&mut app);
        complete_agent_message(&mut app, "2", "assistant-1", "Done.");
        app.handle_session_event(
            event(
                "3",
                EventMsg::AgentMessage {
                    text: "Done.".to_string(),
                },
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Done.").count(), 1);
    }

    #[test]
    fn assistant_delta_without_newline_renders_while_streaming() {
        let mut app = App::new();

        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("hello"),
                }),
            ),
            20,
        );

        assert_eq!(transcript_strings(&app), vec![String::from("• hello")]);
    }

    #[test]
    fn last_user_message_drops_its_bottom_separator() {
        let mut app = App::new();
        start_turn(&mut app);
        app.handle_session_event(
            event(
                "u1",
                EventMsg::UserMessage {
                    text: "hello".to_string(),
                },
            ),
            20,
        );

        let texts = transcript_strings(&app);
        let rule = "─".repeat(80);
        // Only the top rule is drawn; the bottom rule is dropped because the
        // user message is the last thing in the transcript.
        assert_eq!(texts.iter().filter(|t| **t == rule).count(), 1, "{texts:?}");
        assert_eq!(
            texts.last().map(String::as_str),
            Some("▌ hello"),
            "{texts:?}"
        );
    }

    #[test]
    fn user_message_keeps_bottom_separator_when_followed_by_reply() {
        let mut app = App::new();
        start_turn(&mut app);
        app.handle_session_event(
            event(
                "u1",
                EventMsg::UserMessage {
                    text: "hello".to_string(),
                },
            ),
            20,
        );
        complete_agent_message(&mut app, "a1", "assistant-1", "world");

        let texts = transcript_strings(&app);
        let rule = "─".repeat(80);
        // The user message now has a reply after it, so both rules are drawn.
        assert_eq!(texts.iter().filter(|t| **t == rule).count(), 2, "{texts:?}");
    }

    #[test]
    fn reasoning_delta_then_completed_renders_once() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "reasoning-1", "Thinking through this...\n");
        complete_reasoning(&mut app, "3", "reasoning-1", "Thinking through this...");

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert_eq!(joined.matches("Thinking through this...").count(), 1);
        assert!(
            transcript
                .iter()
                .any(|line| line == "… Thinking through this...")
        );
    }

    #[test]
    fn late_reasoning_completion_after_stream_finalized_via_assistant_does_not_duplicate() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "r-1", "Planning\n");
        app.handle_session_event(
            event(
                "3",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("a-1"),
                    delta: String::from("Done.\n"),
                }),
            ),
            20,
        );
        complete_agent_message(&mut app, "4", "a-1", "Done.");
        complete_reasoning(&mut app, "5", "r-1", "Planning");

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Planning").count(), 1);
    }

    #[test]
    fn tool_call_start_then_complete_renders_single_row() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert_eq!(
            transcript
                .iter()
                .filter(|line| line.contains("read foo.rs"))
                .count(),
            1
        );
        assert!(joined.contains("✓ read foo.rs"));
        assert!(!joined.contains("BIG CONTENT"));
    }

    #[test]
    fn consecutive_same_tool_calls_render_as_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        start_tool_call(&mut app, "3", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "4", "c1", true, None);
        complete_tool_call(&mut app, "5", "c2", true, None);

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("✓ read\n   1. ✓ foo.rs\n   2. ✓ bar.rs"));
        assert!(!joined.contains("✓ read foo.rs"));
    }

    #[test]
    fn reasoning_breaks_tool_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);
        complete_reasoning(&mut app, "4", "reasoning-1", "Checking context");
        start_tool_call(&mut app, "5", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "6", "c2", true, None);

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✓ read foo.rs"),
                String::new(),
                String::from("… Checking context"),
                String::new(),
                String::from("✓ read bar.rs"),
            ]
        );
    }

    #[test]
    fn phantom_stream_breaks_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);

        app.model.assistant_stream = Some(StreamController::new(Some(20)));

        start_tool_call(&mut app, "4", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "5", "c2", true, None);

        let transcript = transcript_strings(&app);
        assert!(transcript.iter().any(|line| line == "✓ read foo.rs"));
        assert!(transcript.iter().any(|line| line == "✓ read bar.rs"));
        assert!(!transcript.iter().any(|line| line == "✓ read"));
    }

    #[test]
    fn failed_entry_inside_group_shows_error_reason() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        start_tool_call(&mut app, "3", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "4", "c1", true, None);
        complete_tool_call(&mut app, "5", "c2", false, Some("permission denied"));

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✗ read"),
                String::from("   1. ✓ foo.rs"),
                String::from("   2. ✗ bar.rs"),
                String::from("        ! permission denied"),
            ]
        );
    }

    #[test]
    fn file_change_completion_renders_diff_in_transcript() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "edit", "src/lib.rs");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("edited src/lib.rs (1 replacement)")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_changes: vec![cazean_protocol::FileChangeOutput {
                        path: "src/lib.rs".into(),
                        change: cazean_protocol::FileChange::Update {
                            unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                            move_path: None,
                        },
                    }],
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("• Edited 1 file (+1 -1)"));
        assert!(joined.contains("src/lib.rs (+1 -1)"));
        assert!(joined.contains("1 - old"));
        assert!(joined.contains("1 + new"));
        assert!(!joined.contains("✓ edit src/lib.rs"));
        assert_eq!(app.model.recent_file_changes.len(), 1);

        app.model.screen = Screen::Workspace;
        app.model.focus = FocusTarget::Transcript;
        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| app.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("1 - old"), "{rendered}");
        assert!(rendered.contains("1 + new"), "{rendered}");
        assert!(!rendered.contains("Selected Diff"), "{rendered}");
        Ok(())
    }

    #[test]
    fn todo_write_completion_renders_checklist_in_transcript() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "todo_write", "{\"todos\":[...]}");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("Todo list updated: 2 items")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_changes: Vec::new(),
                    todos: vec![
                        TodoItem {
                            content: String::from("add module"),
                            status: cazean_protocol::TodoStatus::Completed,
                        },
                        TodoItem {
                            content: String::from("register tool"),
                            status: cazean_protocol::TodoStatus::InProgress,
                        },
                    ],
                }),
            ),
            80,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("☑ add module"), "{joined}");
        assert!(joined.contains("◐ register tool"), "{joined}");
        assert!(!joined.contains("todo_write"), "{joined}");
    }

    #[test]
    fn move_file_change_renders_destination() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "edit", "src/old.rs");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("applied edits (1 file changed)")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_changes: vec![cazean_protocol::FileChangeOutput {
                        path: "src/old.rs".into(),
                        change: cazean_protocol::FileChange::Update {
                            unified_diff: String::new(),
                            move_path: Some("src/new.rs".into()),
                        },
                    }],
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("• Moved 1 file (+0 -0)"));
        assert!(joined.contains("src/old.rs -> src/new.rs (+0 -0)"));

        app.model.screen = Screen::Workspace;
        app.model.focus = FocusTarget::Transcript;
        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| app.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("src/old.rs -> src/new.rs"), "{rendered}");
        Ok(())
    }

    #[test]
    fn multi_file_change_completion_renders_multiple_patch_items() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "edit",
            "{\"updates\":[{\"file_path\":\"one.txt\",\"hunks\":[]}]}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("applied edits (2 files changed)")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_changes: vec![
                        cazean_protocol::FileChangeOutput {
                            path: "one.txt".into(),
                            change: cazean_protocol::FileChange::Update {
                                unified_diff: diffy::create_patch("one\n", "uno\n").to_string(),
                                move_path: None,
                            },
                        },
                        cazean_protocol::FileChangeOutput {
                            path: "two.txt".into(),
                            change: cazean_protocol::FileChange::Add {
                                content: "dos\n".to_string(),
                            },
                        },
                    ],
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("one.txt (+1 -1)"));
        assert!(joined.contains("two.txt (+1 -0)"));
        assert!(!joined.contains("✓ edit"));
        assert_eq!(app.model.recent_file_changes.len(), 2);
    }

    #[test]
    fn file_change_completion_auto_scrolls_after_summary_expands() {
        let mut app = App::new();
        let viewport_height = 1;

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "write", "large.txt");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("wrote bytes to large.txt")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_changes: vec![cazean_protocol::FileChangeOutput {
                        path: "large.txt".into(),
                        change: cazean_protocol::FileChange::Add {
                            content: (0..40)
                                .map(|idx| format!("line {idx}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        },
                    }],
                    todos: Vec::new(),
                }),
            ),
            viewport_height,
        );

        assert!(app.model.scroll > 0);
        let max_scroll = app.max_scroll(viewport_height);
        assert_eq!(app.model.scroll, max_scroll);
    }

    #[test]
    fn long_edit_diff_rows_do_not_hide_bottom_marker() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        app.model.screen = Screen::Workspace;
        app.model.focus = FocusTarget::Transcript;
        app.model.inspector_visible = false;
        let old = (0..20)
            .map(|_| "a".repeat(80))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..20)
            .map(|_| "b".repeat(80))
            .collect::<Vec<_>>()
            .join("\n");

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "edit", "src/lib.rs");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("edited src/lib.rs")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_changes: vec![cazean_protocol::FileChangeOutput {
                        path: "src/lib.rs".into(),
                        change: cazean_protocol::FileChange::Update {
                            unified_diff: diffy::create_patch(
                                &format!("{old}\n"),
                                &format!("{new}\n"),
                            )
                            .to_string(),
                            move_path: None,
                        },
                    }],
                    todos: Vec::new(),
                }),
            ),
            8,
        );
        app.model.push_info("bottom marker");
        let viewport_height = app.model.transcript_viewport_height(40, 12);
        app.model.transcript_inner_width = 40;
        app.model.scroll_to_bottom(viewport_height);
        app.model.auto_scroll = false;

        let mut terminal = Terminal::new(TestBackend::new(40, 12))?;
        terminal.draw(|frame| app.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(rendered.contains("bottom marker"), "{rendered}");
        Ok(())
    }

    #[test]
    fn skill_popup_renders_above_composer() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;
        let _ = model.handle_key_event(key(KeyCode::Char('/')));

        let mut terminal = Terminal::new(TestBackend::new(80, 18))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        let Some(deploy_idx) = rendered.find("/deploy  Deploy the app") else {
            panic!("popup row missing:\n{rendered}");
        };
        let Some(review_idx) = rendered.find("/review  Review a PR") else {
            panic!("popup row missing:\n{rendered}");
        };
        let Some(status_idx) = rendered.find("─ Status ") else {
            panic!("status separator missing:\n{rendered}");
        };
        assert!(deploy_idx < review_idx, "{rendered}");
        assert!(review_idx < status_idx, "{rendered}");
        Ok(())
    }

    #[test]
    fn visible_transcript_lines_at_bottom_are_exact_viewport_rows() {
        let mut model = UiModel::new();
        for idx in 1..=5 {
            model.push_info(format!("line {idx}"));
        }
        model.terminal_width = 80;
        model.scroll_to_bottom(3);

        let lines = model.visible_transcript_lines(78, 3);
        let rendered = lines
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered.len(), 3);
        assert_eq!(rendered.last().map(String::as_str), Some("i line 5"));
    }

    #[test]
    fn rendered_transcript_bottom_includes_newest_row() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        model.screen = Screen::Workspace;
        model.focus = FocusTarget::Transcript;
        for idx in 1..=12 {
            model.push_info(format!("line {idx}"));
        }
        model.terminal_width = 50;
        let viewport_height = model.transcript_viewport_height(50, 10);
        model.scroll = model.max_scroll(viewport_height);
        model.auto_scroll = false;

        let mut terminal = Terminal::new(TestBackend::new(50, 10))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("i line 12"), "{rendered}");
        Ok(())
    }

    #[test]
    fn split_view_auto_scroll_reaches_wrapping_bottom() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        model.screen = Screen::Workspace;
        model.focus = FocusTarget::Transcript;
        // A wide terminal (>= 110) enables the split workspace, so the transcript
        // pane is only ~70% of the width. These lines wrap at the pane width but
        // would not wrap at the full terminal width — the case where row counting
        // at the wrong width left the newest content unreachable.
        for idx in 1..=20 {
            model.push_info(format!("{} marker{idx}", "x".repeat(90)));
        }
        model.auto_scroll = true;

        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(
            rendered.contains("marker20"),
            "newest content missing:\n{rendered}"
        );
        Ok(())
    }

    #[test]
    fn active_wrap_is_cached_by_width_and_version() {
        let mut model = UiModel::new();
        model.set_active_assistant_lines(Some(vec![Line::raw("streaming text here")]));

        model.refresh_active_wrap(40);
        assert_eq!(model.active_wrap_computes, 1);

        // Same width and unchanged version: cache hit, no recompute.
        model.refresh_active_wrap(40);
        assert_eq!(model.active_wrap_computes, 1);

        // A different width is a miss.
        model.refresh_active_wrap(20);
        assert_eq!(model.active_wrap_computes, 2);

        // A new delta bumps the version, so even the same width recomputes.
        model.set_active_assistant_lines(Some(vec![Line::raw("streaming text and more")]));
        model.refresh_active_wrap(20);
        assert_eq!(model.active_wrap_computes, 3);
    }

    #[test]
    fn visible_transcript_lines_counts_active_separator_above_viewport() {
        let mut model = UiModel::new();
        model.push_info("history");
        model.set_active_assistant_lines(Some(vec![
            Line::raw("active one"),
            Line::raw("active two"),
            Line::raw("active three"),
        ]));
        model.terminal_width = 80;
        model.scroll_to_bottom(3);

        let rendered = model
            .visible_transcript_lines(78, 3)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "• active one".to_string(),
                "  active two".to_string(),
                "  active three".to_string(),
            ]
        );
    }

    #[test]
    fn long_tool_call_rows_are_counted_after_wrapping() {
        let mut model = UiModel::new();
        let item_id = model.next_item_id();
        model.push_history(TranscriptItem::tool_group(
            item_id,
            ToolCallGroupCell::new("read".to_string(), "x".repeat(80)),
        ));

        let rendered = model.visible_transcript_lines(20, 20);
        let total_rows = model.total_transcript_rows(20);

        assert!(rendered.len() > 1);
        assert_eq!(total_rows, rendered.len());
    }

    #[test]
    fn workspace_renders_input_above_status_footer() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_insert_model();
        model.composer.set_text("draft".to_string());

        let mut terminal = Terminal::new(TestBackend::new(80, 16))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        let Some(input_idx) = rendered.find("─ Input ") else {
            panic!("input separator missing:\n{rendered}");
        };
        let Some(status_idx) = rendered.find("─ Status ") else {
            panic!("status separator missing:\n{rendered}");
        };
        assert!(input_idx < status_idx, "{rendered}");
        Ok(())
    }

    #[test]
    fn command_line_renders_above_status_footer() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        model.mode = UiMode::Command;
        model.command = "help".to_string();
        model
            .composer
            .set_text("first input row\nsecond input row".to_string());

        let mut terminal = Terminal::new(TestBackend::new(80, 18))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        let Some(first_input_idx) = rendered.find("first input row") else {
            panic!("composer text missing:\n{rendered}");
        };
        let Some(second_input_idx) = rendered.find("second input row") else {
            panic!("multi-line composer text missing:\n{rendered}");
        };
        let Some(command_idx) = rendered.find(":help") else {
            panic!("command line missing:\n{rendered}");
        };
        let Some(status_idx) = rendered.find("─ Status ") else {
            panic!("status separator missing:\n{rendered}");
        };

        assert!(first_input_idx < second_input_idx, "{rendered}");
        assert!(second_input_idx < command_idx, "{rendered}");
        assert!(command_idx < status_idx, "{rendered}");
        Ok(())
    }

    #[test]
    fn hidden_inspector_renders_transcript_across_full_workspace_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        model.inspector_visible = false;
        model.push_info("body marker");

        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(!rendered.contains("Inspector"), "{rendered}");
        assert!(rendered.contains("body marker"), "{rendered}");
        assert_eq!(model.transcript_inner_width, 120);
        Ok(())
    }

    #[test]
    fn dashboard_render_shows_only_visible_scrolled_sessions()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        model.viewport_height = 8;
        model.dashboard.items = (0..8).map(dashboard_thread).collect();
        model.dashboard.selected = 3;
        model.dashboard_ensure_selected_visible(8);

        let mut terminal = Terminal::new(TestBackend::new(80, 8))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(!rendered.contains("message-0"), "{rendered}");
        assert!(rendered.contains("message-2"), "{rendered}");
        assert!(
            rendered.contains("› 2026-05-31T00:03:00Z  message-3"),
            "{rendered}"
        );
        assert!(!rendered.contains("message-4"), "{rendered}");
        Ok(())
    }

    #[test]
    fn dashboard_truncates_long_previews_and_keeps_footer_visible()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let items: Vec<ThreadListItem> = (0..3)
            .map(|idx| {
                let mut item = dashboard_thread(idx);
                item.last_user_message = Some(format!("preview-{idx} ").repeat(40));
                item
            })
            .collect();
        model.dashboard.items = items;

        // header (2) + 3 sessions * 2 + footer (2) == 10 rows: only fits if the
        // long previews are truncated to one row each rather than wrapped.
        let width = 50usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 10))?;
        terminal.draw(|frame| model.render(frame))?;
        let rows = buffer_rows(&terminal, width);

        assert!(
            rows.iter().any(|r| r.contains("n new")),
            "footer clipped by wrapped previews: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains('…')),
            "preview was not truncated: {rows:?}"
        );
        let id_rows = rows.iter().filter(|r| r.contains("thread-")).count();
        assert_eq!(id_rows, 3, "each session should stay two rows: {rows:?}");
        Ok(())
    }

    #[test]
    fn inspector_wraps_long_running_tool_args_with_hanging_indent()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        model.inspector_visible = true;
        model.focus = FocusTarget::Inspector;
        model.running_tools.insert(
            "call-1".to_string(),
            RunningToolInfo {
                tool_name: "run_command".to_string(),
                args_preview: "echo this is a really long command preview that must wrap"
                    .to_string(),
            },
        );

        let width = 40usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 20))?;
        terminal.draw(|frame| model.render(frame))?;
        let rows = buffer_rows(&terminal, width);

        assert!(
            rows.iter().any(|r| r.contains("run_command")),
            "tool name missing: {rows:?}"
        );
        // glyph (2) + "run_command" (11) + space (1) = 14-column hanging indent.
        let indent = " ".repeat(14);
        assert!(
            rows.iter()
                .any(|r| r.starts_with(&indent) && !r.trim().is_empty()),
            "args did not hang-indent on continuation: {rows:?}"
        );
        Ok(())
    }

    #[test]
    fn selected_item_lines_get_background_highlight() {
        let mut model = select_model_with_items(2);
        model.transcript_inner_width = 40;
        enter_select(&mut model, Instant::now());

        let lines = model.visible_transcript_lines(40, 10);
        // Layout: item0 row, separator, item1 row (selected).
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].style.bg, None);
        assert_eq!(lines[2].style.bg, Some(Color::DarkGray));
    }
}
