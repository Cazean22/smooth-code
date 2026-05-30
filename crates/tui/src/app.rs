use std::collections::HashMap;

use anyhow::Result;
use app_server_protocol::{AskUserQuestionParams, JSONRPCErrorError, RequestId};
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use smooth_protocol::{AgentStatus, Event, EventMsg, ThreadId, ToolCallResultKind};

use crate::{
    AppTerminal,
    app_server_session::AppServerSession,
    history_cell::{
        AgentMessageCell, HistoryCell, PlainHistoryCell, ReasoningCell, ToolCallGroupCell,
        ToolCallState, UserHistoryCell,
    },
    question_picker::{PickerOutcome, QuestionPicker},
    streaming::StreamController,
};

#[derive(Debug)]
pub(crate) enum AppRunControl {
    Continue,
    Exit,
}

pub(crate) struct App {
    pub(crate) current_thread_id: Option<ThreadId>,
    transcript_cells: Vec<Box<dyn HistoryCell>>,
    active_assistant_cell: Option<AgentMessageCell>,
    active_reasoning_cell: Option<ReasoningCell>,
    assistant_stream: Option<StreamController>,
    reasoning_stream: Option<StreamController>,
    tool_call_rows: HashMap<String, (usize, usize)>,
    subagent_tool_calls: HashMap<ThreadId, String>,
    pending_tool_group: Option<(usize, String)>,
    current_turn_id: Option<String>,
    committed_assistant_item_id: Option<String>,
    committed_assistant_for_current_turn: bool,
    committed_reasoning_item_id: Option<String>,
    in_flight_reasoning_item_id: Option<String>,
    composer: String,
    status_line: String,
    scroll: u16,
    auto_scroll: bool,
    is_turn_running: bool,
    plan_mode: bool,
    terminal_width: u16,
    question_picker: Option<QuestionPicker>,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            current_thread_id: None,
            transcript_cells: Vec::new(),
            active_assistant_cell: None,
            active_reasoning_cell: None,
            assistant_stream: None,
            reasoning_stream: None,
            tool_call_rows: HashMap::new(),
            subagent_tool_calls: HashMap::new(),
            pending_tool_group: None,
            current_turn_id: None,
            committed_assistant_item_id: None,
            committed_assistant_for_current_turn: false,
            committed_reasoning_item_id: None,
            in_flight_reasoning_item_id: None,
            composer: String::new(),
            status_line: String::from("Idle"),
            scroll: 0,
            auto_scroll: true,
            is_turn_running: false,
            plan_mode: false,
            terminal_width: 80,
            question_picker: None,
        }
    }

    pub(crate) fn begin_question_picker(
        &mut self,
        request_id: RequestId,
        params: AskUserQuestionParams,
    ) {
        self.question_picker = Some(QuestionPicker::new(request_id, params));
    }

    async fn dispatch_picker_key(
        &mut self,
        app_server: &mut AppServerSession,
        key_event: KeyEvent,
    ) -> Result<AppRunControl> {
        let outcome = self
            .question_picker
            .as_mut()
            .map(|picker| picker.handle_key(key_event))
            .unwrap_or(PickerOutcome::None);
        match outcome {
            PickerOutcome::None => Ok(AppRunControl::Continue),
            PickerOutcome::Confirm(response) => {
                if let Some(picker) = self.question_picker.take() {
                    let value = serde_json::to_value(response)?;
                    app_server
                        .respond_to_server_request(picker.request_id, value)
                        .await?;
                }
                Ok(AppRunControl::Continue)
            }
            PickerOutcome::Cancel => {
                if let Some(picker) = self.question_picker.take() {
                    app_server
                        .fail_server_request(
                            picker.request_id,
                            JSONRPCErrorError {
                                code: -32001,
                                data: None,
                                message: "user declined to answer".to_string(),
                            },
                        )
                        .await?;
                }
                Ok(AppRunControl::Continue)
            }
        }
    }

    pub(crate) async fn handle_key_event(
        &mut self,
        app_server: &mut AppServerSession,
        key_event: KeyEvent,
        viewport_height: u16,
    ) -> Result<AppRunControl> {
        if key_event.kind != crossterm::event::KeyEventKind::Press {
            return Ok(AppRunControl::Continue);
        }

        // Ctrl-C always exits, even while the picker is open.
        if matches!(key_event.code, KeyCode::Char('c'))
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            return Ok(AppRunControl::Exit);
        }

        if self.question_picker.is_some() {
            return self.dispatch_picker_key(app_server, key_event).await;
        }

        match key_event.code {
            KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(AppRunControl::Exit);
            }
            KeyCode::Esc => return Ok(AppRunControl::Exit),
            KeyCode::Char('q') if self.composer.is_empty() => return Ok(AppRunControl::Exit),
            KeyCode::BackTab if !self.is_turn_running => {
                self.toggle_plan_mode(app_server).await;
            }
            KeyCode::Enter if !self.is_turn_running => {
                let message = std::mem::take(&mut self.composer);
                if !message.trim().is_empty() {
                    self.submit_user_input(app_server, message).await?;
                }
            }
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Char(ch)
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.composer.push(ch);
            }
            KeyCode::Tab => self.composer.push_str("    "),
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1, viewport_height),
            KeyCode::PageUp => self.scroll_up(viewport_height.saturating_sub(1).max(1)),
            KeyCode::PageDown => {
                self.scroll_down(viewport_height.saturating_sub(1).max(1), viewport_height)
            }
            KeyCode::Home => {
                self.scroll = 0;
                self.auto_scroll = false;
            }
            KeyCode::End => {
                self.auto_scroll = true;
                self.scroll_to_bottom(viewport_height);
            }
            _ => {}
        }
        Ok(AppRunControl::Continue)
    }

    async fn submit_user_input(
        &mut self,
        app_server: &mut AppServerSession,
        input: String,
    ) -> Result<()> {
        let thread_id = self
            .current_thread_id
            .ok_or_else(|| anyhow::anyhow!("no started thread available for prompt submission"))?;
        let response = app_server.turn_start(thread_id, input).await?;
        self.status_line = format!("Turn {}", response.turn_id);
        Ok(())
    }

    async fn toggle_plan_mode(&mut self, app_server: &mut AppServerSession) {
        let Some(thread_id) = self.current_thread_id else {
            self.push_history(Box::new(PlainHistoryCell::info(
                "no active thread; start a session before toggling plan mode",
            )));
            return;
        };
        let desired = !self.plan_mode;
        match app_server.set_plan_mode(thread_id, desired).await {
            Ok(response) => {
                // The authoritative state will also arrive via PlanModeChanged
                // event, but apply it now so the indicator updates immediately.
                self.plan_mode = response.enabled;
            }
            Err(err) => {
                self.push_history(Box::new(PlainHistoryCell::error(format!(
                    "could not toggle plan mode: {err}"
                ))));
            }
        }
    }

    pub(crate) fn handle_session_event(&mut self, event: Event, viewport_height: u16) {
        match event.msg {
            EventMsg::SessionConfigured(configured) => {
                self.finalize_reasoning_stream();
                match configured.thread_id.parse::<ThreadId>() {
                    Ok(thread_id) => {
                        self.current_thread_id = Some(thread_id);
                        self.push_history(Box::new(PlainHistoryCell::info(format!(
                            "Session configured for thread {}",
                            configured.thread_id
                        ))));
                    }
                    Err(err) => {
                        self.push_history(Box::new(PlainHistoryCell::error(format!(
                            "Invalid configured thread id: {err}"
                        ))));
                    }
                }
            }
            EventMsg::TurnStarted(turn) => {
                self.is_turn_running = true;
                self.tool_call_rows.clear();
                self.subagent_tool_calls.clear();
                self.pending_tool_group = None;
                self.current_turn_id = Some(turn.turn_id.clone());
                self.committed_assistant_item_id = None;
                self.committed_assistant_for_current_turn = false;
                self.committed_reasoning_item_id = None;
                self.in_flight_reasoning_item_id = None;
                self.status_line = format!("Running turn {}", turn.turn_id);
            }
            EventMsg::TurnCompleted(turn) => {
                let committed_from_stream = self.finalize_assistant_stream(None);
                self.is_turn_running = false;
                self.status_line = format!("Completed turn {}", turn.turn_id);
                if let Some(message) = turn.last_assistant_message
                    && !committed_from_stream
                    && !self.committed_assistant_for_current_turn
                {
                    self.push_rendered_assistant_message(&message);
                    self.committed_assistant_for_current_turn = true;
                }
            }
            EventMsg::TurnInterrupted(turn) => {
                self.finalize_assistant_stream(None);
                self.is_turn_running = false;
                self.push_history(Box::new(PlainHistoryCell::info(format!(
                    "Turn {} interrupted: {}",
                    turn.turn_id, turn.reason
                ))));
                self.status_line = String::from("Interrupted");
            }
            EventMsg::AgentStatusChanged(status) => {
                self.status_line = format!("Status: {}", agent_status_label(&status.status));
            }
            EventMsg::UserMessage(message) => {
                self.finalize_reasoning_stream();
                self.push_history(Box::new(UserHistoryCell::new(message)));
            }
            EventMsg::AgentMessageDelta(delta) => {
                self.handle_assistant_delta(delta.delta);
            }
            EventMsg::AgentMessageCompleted(completed) => {
                let committed_from_stream =
                    self.finalize_assistant_stream(Some(completed.item_id.as_str()));
                if !committed_from_stream
                    && self.committed_assistant_item_id.as_deref()
                        != Some(completed.item_id.as_str())
                {
                    self.push_rendered_assistant_message(&completed.text);
                    self.committed_assistant_item_id = Some(completed.item_id);
                    self.committed_assistant_for_current_turn = true;
                }
            }
            EventMsg::AgentReasoningDelta(delta) => {
                self.handle_reasoning_delta(delta.item_id, delta.delta);
            }
            EventMsg::AgentReasoningCompleted(completed) => {
                self.pending_tool_group = None;
                if self.committed_reasoning_item_id.as_deref() != Some(completed.item_id.as_str()) {
                    let committed_from_stream = self.finalize_reasoning_stream();
                    if !committed_from_stream {
                        self.push_rendered_reasoning_message(&completed.text);
                        self.committed_reasoning_item_id = Some(completed.item_id);
                    }
                }
            }
            EventMsg::ToolCallStarted(tool) => {
                self.finalize_assistant_stream(None);

                let call_id = tool.call_id;
                let tool_name = tool.tool_name;
                let args_preview = tool.args_preview;

                if let Some(cell_idx) = self
                    .pending_tool_group
                    .as_ref()
                    .and_then(|(idx, name)| (name == &tool_name).then_some(*idx))
                {
                    let entry_idx = self.tool_group_mut(cell_idx).push_entry(args_preview);
                    self.tool_call_rows.insert(call_id, (cell_idx, entry_idx));
                } else {
                    let cell_idx = self.push_tool_group_cell(ToolCallGroupCell::new(
                        tool_name.clone(),
                        args_preview,
                    ));
                    self.tool_call_rows.insert(call_id, (cell_idx, 0));
                }
            }
            EventMsg::ToolCallCompleted(tool) => {
                self.finalize_assistant_stream(None);
                if tool.result_kind == ToolCallResultKind::StatusUpdate && tool.success {
                    if let Some(thread_id) = tool.related_thread_id {
                        self.subagent_tool_calls
                            .insert(thread_id, tool.call_id.clone());
                    }
                    if let Some((cell_idx, entry_idx)) =
                        self.tool_call_rows.get(&tool.call_id).copied()
                    {
                        self.tool_group_mut(cell_idx).set_entry_outcome(
                            entry_idx,
                            ToolCallState::Running,
                            None,
                        );
                    }
                    return;
                }

                let new_state = if tool.success {
                    ToolCallState::Success
                } else {
                    ToolCallState::Failure
                };
                let error = if tool.success {
                    None
                } else {
                    Some(tool.error.unwrap_or_else(|| String::from("tool failed")))
                };

                if let Some((cell_idx, entry_idx)) = self.tool_call_rows.remove(&tool.call_id) {
                    self.tool_group_mut(cell_idx)
                        .set_entry_outcome(entry_idx, new_state, error);
                } else if let Some(error) = error {
                    self.push_history(Box::new(PlainHistoryCell::error(format!(
                        "tool {} failed: {}",
                        tool.call_id, error
                    ))));
                } else {
                    self.push_history(Box::new(PlainHistoryCell::info(format!(
                        "tool {} completed",
                        tool.call_id
                    ))));
                }
            }
            EventMsg::Error(error) => {
                self.finalize_assistant_stream(None);
                self.push_history(Box::new(PlainHistoryCell::error(error.message)));
                self.is_turn_running = false;
                self.status_line = String::from("Error");
            }
            EventMsg::CollabAgentSpawnBegin(_event) => {}
            EventMsg::CollabAgentSpawnEnd(_event) => {}
            EventMsg::CollabAgentCompleted(event) => {
                self.complete_subagent_tool_call(event.child_thread_id, &event.status);
            }
            EventMsg::CollabResumeBegin(event) => {
                self.push_history(Box::new(PlainHistoryCell::info(format!(
                    "Resuming agent {}",
                    event.receiver_thread_id
                ))));
            }
            EventMsg::CollabResumeEnd(event) => {
                self.push_history(Box::new(PlainHistoryCell::info(format!(
                    "Resume finished with status {}",
                    agent_status_label(&event.status)
                ))));
            }
            EventMsg::PlanModeChanged(event) => {
                self.plan_mode = event.enabled;
                let message = if event.enabled {
                    "plan mode enabled — agent restricted to read/list/spawn/plan_write/exit_plan_mode"
                } else {
                    "plan mode disabled — agent back to the full tool set"
                };
                self.push_history(Box::new(PlainHistoryCell::info(message)));
            }
            EventMsg::AgentMessage(_) => {}
        }

        if self.auto_scroll {
            self.scroll_to_bottom(viewport_height);
        }
    }

    fn handle_assistant_delta(&mut self, delta: String) {
        if self.assistant_stream.is_none() {
            self.assistant_stream = Some(StreamController::new(Some(usize::from(
                self.terminal_width.saturating_sub(6).max(20),
            ))));
        }
        if let Some(controller) = self.assistant_stream.as_mut() {
            let _ = controller.push(&delta);
            self.active_assistant_cell = controller
                .snapshot_lines()
                .map(|lines| AgentMessageCell::new(lines, true));
        }
    }

    fn handle_reasoning_delta(&mut self, item_id: String, delta: String) {
        self.pending_tool_group = None;
        self.in_flight_reasoning_item_id = Some(item_id);
        if self.reasoning_stream.is_none() {
            self.reasoning_stream = Some(StreamController::new(Some(usize::from(
                self.terminal_width.saturating_sub(6).max(20),
            ))));
        }
        if let Some(controller) = self.reasoning_stream.as_mut() {
            let _ = controller.push(&delta);
            self.active_reasoning_cell = controller
                .snapshot_lines()
                .map(|lines| ReasoningCell::new(lines, true));
        }
    }

    fn finalize_assistant_stream(&mut self, item_id: Option<&str>) -> bool {
        self.finalize_reasoning_stream();
        self.commit_assistant_stream(item_id)
    }

    fn commit_assistant_stream(&mut self, item_id: Option<&str>) -> bool {
        if let Some(controller) = self.assistant_stream.take() {
            self.pending_tool_group = None;
            if let Some(lines) = controller.finalize_lines() {
                self.push_history(Box::new(AgentMessageCell::new(lines, true)));
                self.committed_assistant_item_id = item_id.map(ToOwned::to_owned);
                self.committed_assistant_for_current_turn = true;
                self.active_assistant_cell = None;
                return true;
            }
        }
        self.active_assistant_cell = None;
        false
    }

    fn finalize_reasoning_stream(&mut self) -> bool {
        let had_reasoning = self.reasoning_stream.is_some() || self.active_reasoning_cell.is_some();
        let in_flight_id = self.in_flight_reasoning_item_id.take();
        if let Some(controller) = self.reasoning_stream.take()
            && let Some(lines) = controller.finalize_lines()
        {
            self.push_history(Box::new(ReasoningCell::new(lines, true)));
            self.active_reasoning_cell = None;
            if in_flight_id.is_some() {
                self.committed_reasoning_item_id = in_flight_id;
            }
            return true;
        }
        self.active_reasoning_cell = None;
        if had_reasoning {
            self.pending_tool_group = None;
        }
        false
    }

    fn push_rendered_assistant_message(&mut self, message: &str) {
        let lines = self.render_markdown_lines(message);
        self.push_history(Box::new(AgentMessageCell::new(lines, true)));
    }

    fn push_rendered_reasoning_message(&mut self, message: &str) {
        let lines = self.render_markdown_lines(message);
        self.push_history(Box::new(ReasoningCell::new(lines, true)));
    }

    fn render_markdown_lines(&self, message: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        crate::markdown::append_markdown(
            message,
            Some(usize::from(self.terminal_width.saturating_sub(6))),
            &mut lines,
        );
        lines
    }

    fn push_history(&mut self, cell: Box<dyn HistoryCell>) {
        self.pending_tool_group = None;
        self.transcript_cells.push(cell);
    }

    fn push_tool_group_cell(&mut self, cell: ToolCallGroupCell) -> usize {
        let cell_idx = self.transcript_cells.len();
        let tool_name = cell.tool_name().to_owned();
        self.transcript_cells.push(Box::new(cell));
        self.pending_tool_group = Some((cell_idx, tool_name));
        cell_idx
    }

    fn tool_group_mut(&mut self, cell_idx: usize) -> &mut ToolCallGroupCell {
        self.transcript_cells[cell_idx]
            .as_any_mut()
            .downcast_mut::<ToolCallGroupCell>()
            .expect("tracked tool row should be a ToolCallGroupCell")
    }

    fn complete_subagent_tool_call(&mut self, child_thread_id: ThreadId, status: &AgentStatus) {
        let Some(call_id) = self.subagent_tool_calls.remove(&child_thread_id) else {
            return;
        };
        let Some((cell_idx, entry_idx)) = self.tool_call_rows.remove(&call_id) else {
            return;
        };

        let (state, error) = match status {
            AgentStatus::Completed(_) => (ToolCallState::Success, None),
            AgentStatus::Errored(message) => (ToolCallState::Failure, Some(message.clone())),
            AgentStatus::Interrupted => (ToolCallState::Failure, Some(String::from("interrupted"))),
            AgentStatus::Shutdown => (ToolCallState::Failure, Some(String::from("shutdown"))),
            AgentStatus::NotFound => (ToolCallState::Failure, Some(String::from("not found"))),
            AgentStatus::PendingInit | AgentStatus::Running => (ToolCallState::Running, None),
        };
        self.tool_group_mut(cell_idx)
            .set_entry_outcome(entry_idx, state, error);
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (idx, cell) in self.transcript_cells.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            lines.extend(cell.display_lines(width));
        }
        if let Some(active_cell) = self.active_reasoning_cell.as_ref() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(active_cell.display_lines(width));
        }
        if let Some(active_cell) = self.active_assistant_cell.as_ref() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(active_cell.display_lines(width));
        }
        if lines.is_empty() {
            lines.push(
                Line::from("No transcript yet. Type a message and press Enter.")
                    .style(Style::default().dim()),
            );
        }
        lines
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>) {
        self.terminal_width = frame.area().width;
        let picker_height = self
            .question_picker
            .as_ref()
            .map(|picker| picker.desired_height(frame.area().width).min(20))
            .unwrap_or(0);
        let constraints: Vec<Constraint> = if picker_height > 0 {
            vec![
                Constraint::Min(5),
                Constraint::Length(picker_height),
                Constraint::Length(1),
                Constraint::Length(3),
            ]
        } else {
            vec![
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(3),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        self.render_transcript(frame, chunks[0]);
        if picker_height > 0 {
            if let Some(picker) = &self.question_picker {
                picker.render(frame, chunks[1]);
            }
            self.render_status(frame, chunks[2]);
            self.render_composer(frame, chunks[3]);
        } else {
            self.render_status(frame, chunks[1]);
            self.render_composer(frame, chunks[2]);
        }
    }

    fn render_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner_width = area.width.saturating_sub(2).max(1);
        let lines = self.transcript_lines(inner_width);
        let paragraph = Paragraph::new(Text::from(lines))
            .block(Block::default().title("Transcript").borders(Borders::ALL))
            .wrap(Wrap { trim: false });

        if self.auto_scroll {
            // `lines.len()` counts logical lines; `Paragraph` with `Wrap` lays
            // out wrapped rows. Using the logical count here makes the bottom
            // unreachable on narrow terminals because `scroll((y, _))` is in
            // post-wrap rows, so we'd cap `self.scroll` below the true bottom.
            let total_rows = paragraph.line_count(inner_width);
            let max_scroll = total_rows.saturating_sub(usize::from(area.height.saturating_sub(2)));
            self.scroll = u16::try_from(max_scroll).unwrap_or(u16::MAX);
        }

        frame.render_widget(paragraph.scroll((self.scroll, 0)), area);
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = Vec::with_capacity(6);
        spans.push(Span::styled(
            "Status ",
            Style::default().fg(Color::Yellow).bold(),
        ));
        spans.push(Span::raw(self.status_line.clone()));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            if self.is_turn_running {
                "agent running"
            } else {
                "agent idle"
            },
            Style::default().dim(),
        ));
        if self.plan_mode {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "⏸ PLAN MODE",
                Style::default().fg(Color::Magenta).bold(),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = if self.plan_mode {
            "Input (plan)"
        } else {
            "Input"
        };
        let border_style = if self.plan_mode {
            Style::default().fg(Color::Magenta)
        } else if self.is_turn_running {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let paragraph = Paragraph::new(self.composer.as_str())
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);

        if !self.is_turn_running {
            let composer_width = area.width.saturating_sub(2);
            let visible_len = self
                .composer
                .chars()
                .count()
                .min(usize::from(composer_width));
            let x = area
                .x
                .saturating_add(1 + u16::try_from(visible_len).unwrap_or(u16::MAX));
            let y = area.y.saturating_add(1);
            frame.set_cursor_position((x, y));
        }
    }

    pub(crate) fn viewport_height_for(&self, terminal: &AppTerminal) -> Result<u16> {
        let size = terminal.size()?;
        Ok(size.height.saturating_sub(4).max(1))
    }

    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, amount: u16, viewport_height: u16) {
        let max_scroll = self.max_scroll(viewport_height);
        self.scroll = self.scroll.saturating_add(amount).min(max_scroll);
        self.auto_scroll = self.scroll >= max_scroll;
    }

    fn scroll_to_bottom(&mut self, viewport_height: u16) {
        self.scroll = self.max_scroll(viewport_height);
    }

    fn max_scroll(&self, viewport_height: u16) -> u16 {
        // Mirror `render_transcript`: `inner_width = area.width - 2` for the
        // block's borders. Use `Paragraph::line_count(inner_width)` so the
        // scroll cap reflects post-wrap rows, not logical line count — without
        // this, narrow terminals leave the last `wrapped - logical` rows
        // unreachable even with PageDown/End.
        let inner_width = self.terminal_width.saturating_sub(2).max(1);
        let lines = self.transcript_lines(inner_width);
        let paragraph = Paragraph::new(Text::from(lines))
            .block(Block::default().title("Transcript").borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        let total_rows = paragraph.line_count(inner_width);
        let max_scroll = total_rows.saturating_sub(usize::from(viewport_height));
        u16::try_from(max_scroll).unwrap_or(u16::MAX)
    }

    pub(crate) async fn handle_terminal_event(
        &mut self,
        app_server: &mut AppServerSession,
        event: CrosstermEvent,
        viewport_height: u16,
    ) -> Result<AppRunControl> {
        match event {
            CrosstermEvent::Key(key_event) => {
                self.handle_key_event(app_server, key_event, viewport_height)
                    .await
            }
            CrosstermEvent::Resize(width, _) => {
                self.terminal_width = width;
                Ok(AppRunControl::Continue)
            }
            _ => Ok(AppRunControl::Continue),
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::streaming::StreamController;
    use smooth_protocol::{
        AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
        AgentReasoningDeltaEvent, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent, EventMsg,
        ToolCallCompletedEvent, ToolCallStartedEvent, TurnCompletedEvent, TurnStartedEvent,
    };

    fn event(id: &str, msg: EventMsg) -> Event {
        Event {
            id: id.to_owned(),
            msg,
        }
    }

    fn transcript_strings(app: &App) -> Vec<String> {
        app.transcript_lines(80)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn start_turn(app: &mut App) {
        app.handle_session_event(
            event(
                "turn-start",
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                }),
            ),
            20,
        );
    }

    fn start_tool_call(app: &mut App, event_id: &str, call_id: &str, tool_name: &str, args: &str) {
        app.handle_session_event(
            event(
                event_id,
                EventMsg::ToolCallStarted(ToolCallStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: call_id.to_owned(),
                    tool_name: tool_name.to_owned(),
                    args_preview: args.to_owned(),
                }),
            ),
            20,
        );
    }

    fn complete_tool_call(
        app: &mut App,
        event_id: &str,
        call_id: &str,
        success: bool,
        error: Option<&str>,
    ) {
        app.handle_session_event(
            event(
                event_id,
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: call_id.to_owned(),
                    success,
                    output_preview: Some(String::from("BIG CONTENT")),
                    error: error.map(str::to_owned),
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                }),
            ),
            20,
        );
    }

    fn complete_agent_message(app: &mut App, event_id: &str, item_id: &str, text: &str) {
        app.handle_session_event(
            event(
                event_id,
                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: item_id.to_owned(),
                    text: text.to_owned(),
                }),
            ),
            20,
        );
    }

    fn reasoning_delta(app: &mut App, event_id: &str, item_id: &str, delta: &str) {
        app.handle_session_event(
            event(
                event_id,
                EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: item_id.to_owned(),
                    delta: delta.to_owned(),
                }),
            ),
            20,
        );
    }

    fn complete_reasoning(app: &mut App, event_id: &str, item_id: &str, text: &str) {
        app.handle_session_event(
            event(
                event_id,
                EventMsg::AgentReasoningCompleted(AgentReasoningCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: item_id.to_owned(),
                    text: text.to_owned(),
                }),
            ),
            20,
        );
    }

    #[test]
    fn assistant_message_delta_complete_and_turn_complete_render_once() {
        let mut app = App::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                }),
            ),
            20,
        );
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
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    text: String::from("Hi! What can I help with?"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "1",
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
    fn reasoning_completed_without_deltas_renders_once() {
        let mut app = App::new();

        start_turn(&mut app);
        complete_reasoning(
            &mut app,
            "2",
            "reasoning-1",
            "Silent chain of thought summary",
        );

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert_eq!(joined.matches("Silent chain of thought summary").count(), 1);
        assert!(
            transcript
                .iter()
                .any(|line| line == "… Silent chain of thought summary")
        );
    }

    #[test]
    fn reasoning_then_assistant_message_render_in_order() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "reasoning-1", "Planning steps\n");
        complete_reasoning(&mut app, "3", "reasoning-1", "Planning steps");
        app.handle_session_event(
            event(
                "4",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("Done.\n"),
                }),
            ),
            20,
        );
        complete_agent_message(&mut app, "5", "assistant-1", "Done.");
        app.handle_session_event(
            event(
                "6",
                EventMsg::TurnCompleted(TurnCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    last_assistant_message: Some(String::from("Done.")),
                }),
            ),
            20,
        );

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("… Planning steps"),
                String::new(),
                String::from("• Done."),
            ]
        );
    }

    #[test]
    fn interleaved_reasoning_completion_does_not_duplicate_assistant() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "reasoning-1", "Planning\n");
        app.handle_session_event(
            event(
                "3",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("Started answer\n"),
                }),
            ),
            20,
        );
        complete_reasoning(&mut app, "4", "reasoning-1", "Planning");
        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("… Planning"),
                String::new(),
                String::from("• Started answer"),
            ]
        );
        complete_agent_message(&mut app, "5", "assistant-1", "Started answer");

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Started answer").count(), 1);
        assert!(joined.contains("… Planning\n\n• Started answer"));
    }

    #[test]
    fn duplicate_reasoning_completion_does_not_duplicate_cell() {
        let mut app = App::new();

        start_turn(&mut app);
        complete_reasoning(&mut app, "2", "r-1", "Planning");
        complete_reasoning(&mut app, "3", "r-1", "Planning");

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Planning").count(), 1);
    }

    #[test]
    fn late_reasoning_completion_after_stream_finalized_via_assistant_does_not_duplicate() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "r-1", "Planning\n");
        // Reasoning stream gets flushed by an assistant-side finalization
        // (e.g., AgentMessageCompleted) before the matching ReasoningCompleted event arrives.
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
        // Late completion for the reasoning we already streamed.
        complete_reasoning(&mut app, "5", "r-1", "Planning");

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Planning").count(), 1);
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
    fn completed_message_without_stream_renders_once() {
        let mut app = App::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    text: String::from("Hi! What can I help with?"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "1",
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
    fn turn_completed_fallback_renders_once_when_no_completed_message_exists() {
        let mut app = App::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "1",
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
    fn collab_agent_completed_does_not_render_transcript_entry() {
        let mut app = App::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::CollabAgentCompleted(smooth_protocol::CollabAgentCompletedEvent {
                    parent_thread_id: ThreadId::new(),
                    child_thread_id: ThreadId::new(),
                    agent_path: smooth_protocol::AgentPath::try_from("/root/child").expect("path"),
                    agent_nickname: Some("child".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: AgentStatus::Completed(Some("Done".to_string())),
                    last_assistant_message: Some("Done".to_string()),
                }),
            ),
            20,
        );

        assert!(app.transcript_cells.is_empty());
        let joined = transcript_strings(&app).join("\n");
        assert!(!joined.contains("Sub-agent"));
        assert!(!joined.contains("Done"));
    }

    #[test]
    fn spawn_begin_does_not_duplicate_tool_prompt() {
        let mut app = App::new();
        let prompt = "inspect protocol";

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"message\":\"inspect protocol\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                    call_id: String::from("call"),
                    sender_thread_id: ThreadId::new(),
                    prompt: prompt.to_string(),
                    model: None,
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("spawn_agent {\"message\":\"inspect protocol\"}"));
        assert_eq!(joined.matches(prompt).count(), 1);
        assert!(!joined.contains("Spawning sub-agent"));
    }

    #[test]
    fn spawn_end_does_not_duplicate_tool_prompt_or_status() {
        let mut app = App::new();
        let prompt = "inspect protocol";

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"message\":\"inspect protocol\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                    call_id: String::from("call"),
                    sender_thread_id: ThreadId::new(),
                    new_thread_id: Some(ThreadId::new()),
                    new_agent_nickname: Some(String::from("child")),
                    new_agent_role: Some(String::from("explorer")),
                    prompt: prompt.to_string(),
                    model: None,
                    status: AgentStatus::Running,
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("spawn_agent {\"message\":\"inspect protocol\"}"));
        assert_eq!(joined.matches(prompt).count(), 1);
        assert!(!joined.contains("Sub-agent started"));
        assert!(!joined.contains("Spawn ended"));
    }

    #[test]
    fn tool_call_start_then_complete_renders_single_row() {
        let mut app = App::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "2",
                EventMsg::ToolCallStarted(ToolCallStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    tool_name: String::from("read"),
                    args_preview: String::from("foo.rs"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("BIG CONTENT")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                }),
            ),
            20,
        );

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert_eq!(
            transcript
                .iter()
                .filter(|line| line.contains("read foo.rs"))
                .count(),
            1
        );
        assert!(!joined.contains("BIG CONTENT"));
    }

    #[test]
    fn tool_call_completed_changes_glyph_in_place() {
        let mut app = App::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "2",
                EventMsg::ToolCallStarted(ToolCallStartedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    tool_name: String::from("read"),
                    args_preview: String::from("foo.rs"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("BIG CONTENT")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("✓ read foo.rs"));
        assert!(!joined.contains("⠋ read foo.rs"));
    }

    #[test]
    fn spawn_agent_status_update_keeps_tool_row_running() {
        let mut app = App::new();
        let child_thread_id = ThreadId::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"message\":\"inspect\"}",
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
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("⠋ spawn_agent {\"message\":\"inspect\"}"));
        assert!(!joined.contains("✓ spawn_agent {\"message\":\"inspect\"}"));
    }

    #[test]
    fn subagent_completion_finishes_status_update_tool_row() {
        let mut app = App::new();
        let child_thread_id = ThreadId::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"message\":\"inspect\"}",
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
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "4",
                EventMsg::CollabAgentCompleted(smooth_protocol::CollabAgentCompletedEvent {
                    parent_thread_id: ThreadId::new(),
                    child_thread_id,
                    agent_path: smooth_protocol::AgentPath::try_from("/root/child").expect("path"),
                    agent_nickname: Some("child".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: AgentStatus::Completed(Some("Done".to_string())),
                    last_assistant_message: Some("Done".to_string()),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("✓ spawn_agent {\"message\":\"inspect\"}"));
        assert!(!joined.contains("⠋ spawn_agent {\"message\":\"inspect\"}"));
        assert!(!joined.contains("Sub-agent"));
        assert!(!joined.contains("Done"));
    }

    #[test]
    fn consecutive_same_tool_calls_render_as_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        start_tool_call(&mut app, "3", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "4", "c1", true, None);
        complete_tool_call(&mut app, "5", "c2", true, None);

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert!(joined.contains("✓ read\n      ✓ foo.rs\n      ✓ bar.rs"));
        assert!(!joined.contains("✓ read foo.rs"));
    }

    #[test]
    fn different_tool_names_do_not_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "list", "dir1");
        complete_tool_call(&mut app, "3", "c1", true, None);
        start_tool_call(&mut app, "4", "c2", "read", "foo.rs");
        complete_tool_call(&mut app, "5", "c2", true, None);
        start_tool_call(&mut app, "6", "c3", "list", "dir2");
        complete_tool_call(&mut app, "7", "c3", true, None);

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✓ list dir1"),
                String::new(),
                String::from("✓ read foo.rs"),
                String::new(),
                String::from("✓ list dir2"),
            ]
        );
    }

    #[test]
    fn non_tool_event_breaks_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);
        complete_agent_message(&mut app, "4", "assistant-1", "Between calls");
        start_tool_call(&mut app, "5", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "6", "c2", true, None);

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert!(joined.contains("✓ read foo.rs"));
        assert!(joined.contains("✓ read bar.rs"));
        assert!(!transcript.iter().any(|line| line == "✓ read"));
    }

    #[test]
    fn phantom_stream_breaks_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);

        app.assistant_stream = Some(StreamController::new(Some(20)));

        start_tool_call(&mut app, "4", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "5", "c2", true, None);

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert!(joined.contains("✓ read foo.rs"));
        assert!(joined.contains("✓ read bar.rs"));
        assert!(!transcript.iter().any(|line| line == "✓ read"));
    }

    #[test]
    fn mixed_sequence_matches_user_example() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "list", "dir1");
        complete_tool_call(&mut app, "3", "c1", true, None);
        start_tool_call(&mut app, "4", "c2", "read", "file a");
        start_tool_call(&mut app, "5", "c3", "read", "file b");
        complete_tool_call(&mut app, "6", "c2", true, None);
        complete_tool_call(&mut app, "7", "c3", true, None);
        start_tool_call(&mut app, "8", "c4", "list", "dir2");
        complete_tool_call(&mut app, "9", "c4", true, None);
        start_tool_call(&mut app, "10", "c5", "read", "file c");

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✓ list dir1"),
                String::new(),
                String::from("✓ read"),
                String::from("      ✓ file a"),
                String::from("      ✓ file b"),
                String::new(),
                String::from("✓ list dir2"),
                String::new(),
                String::from("⠋ read file c"),
            ]
        );
    }

    #[test]
    fn failed_tool_call_shows_error_reason() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "missing.rs");
        complete_tool_call(&mut app, "3", "c1", false, Some("file not found"));

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✗ read missing.rs"),
                String::from("      ! file not found"),
            ]
        );
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
                String::from("      ✓ foo.rs"),
                String::from("      ✗ bar.rs"),
                String::from("        ! permission denied"),
            ]
        );
    }
}

fn agent_status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::PendingInit => String::from("pending"),
        AgentStatus::Running => String::from("running"),
        AgentStatus::Interrupted => String::from("interrupted"),
        AgentStatus::Completed(Some(text)) => format!("completed ({text})"),
        AgentStatus::Completed(None) => String::from("completed"),
        AgentStatus::Errored(message) => format!("errored ({message})"),
        AgentStatus::Shutdown => String::from("shutdown"),
        AgentStatus::NotFound => String::from("not_found"),
    }
}
