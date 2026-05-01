use std::collections::HashMap;

use anyhow::Result;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use smooth_protocol::{AgentStatus, Event, EventMsg, ThreadId};

use crate::{
    AppTerminal,
    app_server_session::AppServerSession,
    history_cell::{
        AgentMessageCell, HistoryCell, PlainHistoryCell, ToolCallCell, ToolCallState,
        UserHistoryCell,
    },
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
    active_cell: Option<AgentMessageCell>,
    stream_controller: Option<StreamController>,
    tool_call_rows: HashMap<String, (usize, String, String)>,
    current_turn_id: Option<String>,
    committed_assistant_item_id: Option<String>,
    committed_assistant_for_current_turn: bool,
    composer: String,
    status_line: String,
    scroll: u16,
    auto_scroll: bool,
    is_turn_running: bool,
    terminal_width: u16,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            current_thread_id: None,
            transcript_cells: Vec::new(),
            active_cell: None,
            stream_controller: None,
            tool_call_rows: HashMap::new(),
            current_turn_id: None,
            committed_assistant_item_id: None,
            committed_assistant_for_current_turn: false,
            composer: String::new(),
            status_line: String::from("Idle"),
            scroll: 0,
            auto_scroll: true,
            is_turn_running: false,
            terminal_width: 80,
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

        match key_event.code {
            KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(AppRunControl::Exit);
            }
            KeyCode::Esc => return Ok(AppRunControl::Exit),
            KeyCode::Char('q') if self.composer.is_empty() => return Ok(AppRunControl::Exit),
            KeyCode::Enter => {
                if !self.is_turn_running {
                    let message = std::mem::take(&mut self.composer);
                    if !message.trim().is_empty() {
                        self.submit_user_input(app_server, message).await?;
                    }
                }
            }
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Char(ch) => {
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT {
                    self.composer.push(ch);
                }
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

    pub(crate) fn handle_session_event(&mut self, event: Event, viewport_height: u16) {
        match event.msg {
            EventMsg::SessionConfigured(configured) => {
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
                self.current_turn_id = Some(turn.turn_id.clone());
                self.committed_assistant_item_id = None;
                self.committed_assistant_for_current_turn = false;
                self.status_line = format!("Running turn {}", turn.turn_id);
            }
            EventMsg::TurnCompleted(turn) => {
                let committed_from_stream = self.finalize_stream(None);
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
                self.finalize_stream(None);
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
                self.push_history(Box::new(UserHistoryCell::new(message)));
            }
            EventMsg::AgentMessageDelta(delta) => {
                self.handle_streaming_delta(delta.delta);
            }
            EventMsg::AgentMessageCompleted(completed) => {
                let committed_from_stream = self.finalize_stream(Some(completed.item_id.as_str()));
                if !committed_from_stream
                    && self.committed_assistant_item_id.as_deref()
                        != Some(completed.item_id.as_str())
                {
                    self.push_rendered_assistant_message(&completed.text);
                    self.committed_assistant_item_id = Some(completed.item_id);
                    self.committed_assistant_for_current_turn = true;
                }
            }
            EventMsg::ToolCallStarted(tool) => {
                self.finalize_stream(None);
                let idx = self.transcript_cells.len();
                self.tool_call_rows.insert(
                    tool.call_id,
                    (idx, tool.tool_name.clone(), tool.args_preview.clone()),
                );
                let cell = ToolCallCell::running(tool.tool_name, tool.args_preview);
                self.push_history(Box::new(cell));
            }
            EventMsg::ToolCallCompleted(tool) => {
                self.finalize_stream(None);
                let new_state = if tool.success {
                    ToolCallState::Success
                } else {
                    ToolCallState::Failure
                };

                if let Some((idx, tool_name, args_preview)) =
                    self.tool_call_rows.remove(&tool.call_id)
                {
                    self.transcript_cells[idx] = Box::new(
                        ToolCallCell::running(tool_name, args_preview).with_state(new_state),
                    );
                } else {
                    self.push_history(Box::new(
                        ToolCallCell::running(String::new(), String::new()).with_state(new_state),
                    ));
                }
            }
            EventMsg::Error(error) => {
                self.finalize_stream(None);
                self.push_history(Box::new(PlainHistoryCell::error(error.message)));
                self.is_turn_running = false;
                self.status_line = String::from("Error");
            }
            EventMsg::AgentMessage(_) => {}
        }

        if self.auto_scroll {
            self.scroll_to_bottom(viewport_height);
        }
    }

    fn handle_streaming_delta(&mut self, delta: String) {
        if self.stream_controller.is_none() {
            self.stream_controller = Some(StreamController::new(Some(usize::from(
                self.terminal_width.saturating_sub(6).max(20),
            ))));
        }
        if let Some(controller) = self.stream_controller.as_mut() {
            let _ = controller.push(&delta);
            self.active_cell = controller.snapshot_cell();
        }
    }

    fn finalize_stream(&mut self, item_id: Option<&str>) -> bool {
        if let Some(controller) = self.stream_controller.take()
            && let Some(cell) = controller.finalize()
        {
            self.push_history(Box::new(cell));
            self.committed_assistant_item_id = item_id.map(ToOwned::to_owned);
            self.committed_assistant_for_current_turn = true;
            self.active_cell = None;
            return true;
        }
        self.active_cell = None;
        false
    }

    fn push_rendered_assistant_message(&mut self, message: &str) {
        let mut lines = Vec::new();
        crate::markdown::append_markdown(
            message,
            Some(usize::from(self.terminal_width.saturating_sub(6))),
            &mut lines,
        );
        self.push_history(Box::new(AgentMessageCell::new(lines, true)));
    }

    fn push_history(&mut self, cell: Box<dyn HistoryCell>) {
        self.transcript_cells.push(cell);
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (idx, cell) in self.transcript_cells.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            lines.extend(cell.display_lines(width));
        }
        if let Some(active_cell) = self.active_cell.as_ref() {
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(frame.area());

        self.render_transcript(frame, chunks[0]);
        self.render_status(frame, chunks[1]);
        self.render_composer(frame, chunks[2]);
    }

    fn render_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner_width = area.width.saturating_sub(2).max(1);
        let lines = self.transcript_lines(inner_width);
        let paragraph = Paragraph::new(Text::from(lines.clone()))
            .block(Block::default().title("Transcript").borders(Borders::ALL))
            .wrap(Wrap { trim: false });

        if self.auto_scroll {
            let total_rows = lines.len();
            let max_scroll = total_rows.saturating_sub(usize::from(area.height.saturating_sub(2)));
            self.scroll = u16::try_from(max_scroll).unwrap_or(u16::MAX);
        }

        frame.render_widget(paragraph.scroll((self.scroll, 0)), area);
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = Line::from(vec![
            Span::styled("Status ", Style::default().fg(Color::Yellow).bold()),
            Span::raw(self.status_line.clone()),
            Span::raw("  "),
            Span::styled(
                if self.is_turn_running {
                    "agent running"
                } else {
                    "agent idle"
                },
                Style::default().dim(),
            ),
        ]);
        frame.render_widget(Paragraph::new(text), area);
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        let paragraph = Paragraph::new(self.composer.as_str())
            .block(
                Block::default()
                    .title("Input")
                    .borders(Borders::ALL)
                    .border_style(if self.is_turn_running {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default().fg(Color::Cyan)
                    }),
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
        let width = self.terminal_width.saturating_sub(4).max(1);
        let lines = self.transcript_lines(width);
        let total_rows = lines.len();
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
mod tests {
    use super::*;
    use smooth_protocol::{
        AgentMessageCompletedEvent, AgentMessageDeltaEvent, EventMsg, ToolCallCompletedEvent,
        ToolCallStartedEvent, TurnCompletedEvent, TurnStartedEvent,
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
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("✓ read foo.rs"));
        assert!(!joined.contains("⠋ read foo.rs"));
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
