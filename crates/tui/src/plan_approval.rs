use std::cell::Cell;

use app_server_protocol::{
    PlanApprovalDecision, RequestId, RequestPlanApprovalParams, RequestPlanApprovalResponse,
};
use cazean_protocol::ThreadId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::composer::ComposerState;
use crate::{markdown_render, wrap};

pub(crate) enum PlanApprovalOutcome {
    None,
    Respond(RequestPlanApprovalResponse),
    /// Hide the overlay without deciding; the approval request stays pending so
    /// the user can return to the transcript and resume review later.
    Defer,
    /// Open the plan file in the user's `$EDITOR` (handled up in the run loop,
    /// which owns the terminal).
    OpenEditor,
}

enum Stage {
    Reviewing,
    EditingFeedback,
}

/// Whether the overlay currently owns the screen (`Active`) or has been parked
/// with Esc (`Deferred`) while its approval request stays pending.
#[derive(PartialEq, Eq)]
pub(crate) enum Presentation {
    Active,
    Deferred,
}

/// Overlay presenting a plan submitted via `exit_plan_mode` for the user to
/// approve, reject (with feedback), open in `$EDITOR`, or defer. Rendered
/// full-screen: the header and footer stay pinned while the plan body scrolls.
pub(crate) struct PlanApprovalOverlay {
    pub(crate) request_id: RequestId,
    /// Parsed from the request so `Ctrl+G` can locate the plan file on disk.
    thread_id: Option<ThreadId>,
    plan: String,
    /// The model's optional `exit_plan_mode` note, shown above the plan.
    reason: Option<String>,
    scroll: usize,
    stage: Stage,
    presentation: Presentation,
    feedback: ComposerState,
    /// Max scroll offset from the last render, so key handling (which has no
    /// dimensions) can clamp `scroll` to the body height.
    max_scroll: Cell<usize>,
    /// Width of the last render, so feedback Up/Down can move by visual row.
    feedback_width: Cell<u16>,
}

impl PlanApprovalOverlay {
    pub(crate) fn new(request_id: RequestId, params: RequestPlanApprovalParams) -> Self {
        Self {
            request_id,
            thread_id: params.thread_id.parse().ok(),
            plan: params.plan,
            reason: params.reason,
            scroll: 0,
            stage: Stage::Reviewing,
            presentation: Presentation::Active,
            feedback: ComposerState::default(),
            max_scroll: Cell::new(0),
            feedback_width: Cell::new(80),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.presentation == Presentation::Active
    }

    /// Park the overlay (Esc): keep the pending approval request alive but stop
    /// owning the screen so the user can read the transcript.
    pub(crate) fn defer(&mut self) {
        self.presentation = Presentation::Deferred;
        self.stage = Stage::Reviewing;
    }

    pub(crate) fn resume(&mut self) {
        self.presentation = Presentation::Active;
    }

    pub(crate) fn thread_id(&self) -> Option<ThreadId> {
        self.thread_id
    }

    /// Replace the displayed plan after the user edits it in `$EDITOR`.
    pub(crate) fn set_plan(&mut self, plan: String) {
        self.plan = plan;
        // The refreshed plan may be shorter than what was scrolled through;
        // reset scroll so a stale offset doesn't leave Up/PageUp stuck.
        self.scroll = 0;
    }

    /// Insert pasted text into the feedback editor (only while editing).
    pub(crate) fn handle_paste(&mut self, text: &str) {
        if matches!(self.stage, Stage::EditingFeedback) {
            self.feedback.insert_paste(text);
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PlanApprovalOutcome {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return PlanApprovalOutcome::None;
        }

        match self.stage {
            Stage::Reviewing => self.handle_review_key(key),
            Stage::EditingFeedback => self.handle_feedback_key(key),
        }
    }

    fn handle_review_key(&mut self, key: KeyEvent) -> PlanApprovalOutcome {
        // Ctrl+G opens the plan in the external editor (distinguishable from a
        // bare `g` thanks to the kitty keyboard flags pushed at startup).
        if matches!(key.code, KeyCode::Char('g')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            return PlanApprovalOutcome::OpenEditor;
        }

        let max = self.max_scroll.get();
        match key.code {
            // Approve / reject are deliberate; Esc never decides (it defers).
            KeyCode::Char('a') if key.modifiers.is_empty() => {
                PlanApprovalOutcome::Respond(RequestPlanApprovalResponse {
                    decision: PlanApprovalDecision::Approved,
                    feedback: None,
                })
            }
            KeyCode::Char('r') if key.modifiers.is_empty() => {
                self.stage = Stage::EditingFeedback;
                PlanApprovalOutcome::None
            }
            KeyCode::Esc => PlanApprovalOutcome::Defer,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                PlanApprovalOutcome::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(max);
                PlanApprovalOutcome::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                PlanApprovalOutcome::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10).min(max);
                PlanApprovalOutcome::None
            }
            KeyCode::Home => {
                self.scroll = 0;
                PlanApprovalOutcome::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = max;
                PlanApprovalOutcome::None
            }
            _ => PlanApprovalOutcome::None,
        }
    }

    fn handle_feedback_key(&mut self, key: KeyEvent) -> PlanApprovalOutcome {
        // Ctrl+Enter submits the rejection (so bare Enter can add newlines),
        // mirroring the main composer's submit chord.
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
            let feedback = self.feedback.as_str().trim();
            return PlanApprovalOutcome::Respond(RequestPlanApprovalResponse {
                decision: PlanApprovalDecision::Rejected,
                feedback: (!feedback.is_empty()).then(|| feedback.to_string()),
            });
        }

        match key.code {
            KeyCode::Esc => self.stage = Stage::Reviewing,
            KeyCode::Enter => self.feedback.insert_char('\n'),
            KeyCode::Backspace => self.feedback.backspace(),
            KeyCode::Delete => self.feedback.delete(),
            KeyCode::Left => self.feedback.move_left(),
            KeyCode::Right => self.feedback.move_right(),
            KeyCode::Home => self.feedback.move_line_start(),
            KeyCode::End => self.feedback.move_line_end(),
            KeyCode::Up => {
                let width = usize::from(self.feedback_width.get().max(1));
                self.feedback.move_visual_up(width);
            }
            KeyCode::Down => {
                let width = usize::from(self.feedback_width.get().max(1));
                self.feedback.move_visual_down(width);
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.feedback.insert_char(ch)
            }
            _ => {}
        }
        PlanApprovalOutcome::None
    }

    fn footer_lines(&self, wrap_width: usize) -> Vec<Line<'static>> {
        let mut footer = vec![Line::from("")];
        match self.stage {
            Stage::Reviewing => footer.extend(wrap::wrap_line_hanging(
                Line::from(Span::styled(
                    "a approve   r reject (feedback)   Ctrl+G edit in $EDITOR   \
                     Esc defer (R resume)   ↑/↓ PgUp/PgDn Home/End scroll",
                    Style::default().fg(Color::DarkGray),
                )),
                wrap_width,
                0,
            )),
            Stage::EditingFeedback => footer.extend(self.feedback_editor_lines()),
        }
        footer
    }

    /// Render the feedback editor as styled lines with a reversed cursor cell,
    /// so the multi-line `ComposerState` shows the caret without a hardware
    /// cursor (the overlay is one `Paragraph`).
    fn feedback_editor_lines(&self) -> Vec<Line<'static>> {
        let text = self.feedback.as_str();
        let cursor = self.feedback.cursor().min(text.len());
        let normal = Style::default();
        let cursor_style = normal.add_modifier(Modifier::REVERSED);

        let mut out = vec![Line::from(Span::styled(
            "Feedback (type · Ctrl+Enter submit rejection · Esc back to review):",
            Style::default().fg(Color::Yellow),
        ))];
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut placed_cursor = false;
        for (byte, ch) in text.char_indices() {
            if byte == cursor {
                placed_cursor = true;
                if ch == '\n' {
                    spans.push(Span::styled(" ", cursor_style));
                    out.push(Line::from(std::mem::take(&mut spans)));
                    continue;
                }
                spans.push(Span::styled(ch.to_string(), cursor_style));
                continue;
            }
            if ch == '\n' {
                out.push(Line::from(std::mem::take(&mut spans)));
            } else {
                spans.push(Span::styled(ch.to_string(), normal));
            }
        }
        if !placed_cursor {
            spans.push(Span::styled(" ", cursor_style));
        }
        out.push(Line::from(spans));
        out
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let width = area.width;
        let wrap_width = usize::from(width.max(1));
        self.feedback_width.set(width.max(1));

        let header = separator_line(width, "Plan approval", Style::default().fg(Color::Magenta));
        let footer = self.footer_lines(wrap_width);

        let mut body: Vec<Line<'static>> = Vec::new();
        if let Some(reason) = &self.reason {
            body.extend(wrap::wrap_line_hanging(
                Line::from(vec![
                    Span::styled(
                        "Model's note: ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(reason.clone()),
                ]),
                wrap_width,
                wrap::display_width("Model's note: "),
            ));
            body.push(separator_line(
                width,
                "plan",
                Style::default().fg(Color::DarkGray),
            ));
        }
        body.extend(wrap::wrap_lines(
            markdown_render::render_markdown_text(&self.plan).lines,
            wrap_width,
        ));

        let total_height = usize::from(area.height.max(1));
        // The header is one line; the footer height varies (feedback editor).
        let budget = total_height.saturating_sub(1 + footer.len());
        let max_start = body.len().saturating_sub(budget);
        self.max_scroll.set(max_start);
        let start = self.scroll.min(max_start);
        let end = start.saturating_add(budget).min(body.len());

        let mut lines = Vec::with_capacity(total_height);
        lines.push(header);
        if start < end {
            lines.extend(body[start..end].iter().cloned());
        }
        lines.extend(footer);

        frame.render_widget(Paragraph::new(lines), area);
    }
}

fn separator_line(width: u16, label: &str, style: Style) -> Line<'static> {
    let width = usize::from(width);
    if width == 0 {
        return Line::default();
    }

    let prefix = format!("─ {label} ");
    let prefix_len = prefix.chars().count();
    let text = if prefix_len >= width {
        prefix.chars().take(width).collect()
    } else {
        format!("{prefix}{}", "─".repeat(width - prefix_len))
    };
    Line::from(Span::styled(text, style))
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn sample_params() -> RequestPlanApprovalParams {
        RequestPlanApprovalParams {
            thread_id: "t".into(),
            turn_id: "u".into(),
            call_id: "c".into(),
            plan: "# Refactor plan\n\n1. Move the parser.\n2. Add tests.".into(),
            reason: None,
        }
    }

    fn render_rows(
        overlay: &PlanApprovalOverlay,
        width: u16,
        height: u16,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| overlay.render(frame, frame.area()))?;
        Ok(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .chunks(usize::from(width))
            .map(|row| row.concat())
            .collect())
    }

    #[test]
    fn a_approves_without_feedback() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        match overlay.handle_key(key(KeyCode::Char('a'))) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.decision, PlanApprovalDecision::Approved);
                assert_eq!(response.feedback, None);
            }
            _ => panic!("expected approval response"),
        }
    }

    #[test]
    fn esc_defers_without_deciding() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Esc)),
            PlanApprovalOutcome::Defer
        ));
        // Defer parks the overlay but keeps it for resume.
        overlay.defer();
        assert!(!overlay.is_active());
        overlay.resume();
        assert!(overlay.is_active());
    }

    #[test]
    fn ctrl_g_requests_editor() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        assert!(matches!(
            overlay.handle_key(ctrl(KeyCode::Char('g'))),
            PlanApprovalOutcome::OpenEditor
        ));
    }

    #[test]
    fn r_collects_feedback_then_ctrl_enter_rejects() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Char('r'))),
            PlanApprovalOutcome::None
        ));
        for ch in "use sqlite".chars() {
            overlay.handle_key(key(KeyCode::Char(ch)));
        }
        match overlay.handle_key(ctrl(KeyCode::Enter)) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.decision, PlanApprovalDecision::Rejected);
                assert_eq!(response.feedback.as_deref(), Some("use sqlite"));
            }
            _ => panic!("expected rejection with feedback"),
        }
    }

    #[test]
    fn bare_enter_in_feedback_adds_newline_not_submit() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        overlay.handle_key(key(KeyCode::Char('r')));
        overlay.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Enter)),
            PlanApprovalOutcome::None
        ));
        overlay.handle_key(key(KeyCode::Char('b')));
        match overlay.handle_key(ctrl(KeyCode::Enter)) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.feedback.as_deref(), Some("a\nb"));
            }
            _ => panic!("expected rejection with multi-line feedback"),
        }
    }

    #[test]
    fn esc_while_editing_returns_to_review() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        overlay.handle_key(key(KeyCode::Char('r')));
        // While editing, 'a' is text input, not approval.
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Char('a'))),
            PlanApprovalOutcome::None
        ));
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Esc)),
            PlanApprovalOutcome::None
        ));
        match overlay.handle_key(key(KeyCode::Char('a'))) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.decision, PlanApprovalDecision::Approved);
            }
            _ => panic!("expected approval after returning to review"),
        }
    }

    #[test]
    fn paste_into_feedback_appends_text() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        // Paste is ignored while reviewing.
        overlay.handle_paste("ignored");
        overlay.handle_key(key(KeyCode::Char('r')));
        overlay.handle_paste("pasted\nfeedback");
        match overlay.handle_key(ctrl(KeyCode::Enter)) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.feedback.as_deref(), Some("pasted\nfeedback"));
            }
            _ => panic!("expected rejection with pasted feedback"),
        }
    }

    #[test]
    fn feedback_up_moves_cursor_to_previous_line() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        overlay.handle_key(key(KeyCode::Char('r')));
        for ch in "ab".chars() {
            overlay.handle_key(key(KeyCode::Char(ch)));
        }
        overlay.handle_key(key(KeyCode::Enter));
        for ch in "cd".chars() {
            overlay.handle_key(key(KeyCode::Char(ch)));
        }
        // feedback is "ab\ncd" with the cursor at the end; a render primes the
        // editor width that Up/Down navigation needs.
        let _ = render_rows(&overlay, 40, 8);
        overlay.handle_key(key(KeyCode::Up));
        overlay.handle_key(key(KeyCode::End));
        overlay.handle_key(key(KeyCode::Char('X')));
        match overlay.handle_key(ctrl(KeyCode::Enter)) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.feedback.as_deref(), Some("abX\ncd"));
            }
            _ => panic!("expected rejection with edited feedback"),
        }
    }

    #[test]
    fn reason_is_displayed() -> Result<(), Box<dyn std::error::Error>> {
        let mut params = sample_params();
        params.reason = Some("ready for review".into());
        let overlay = PlanApprovalOverlay::new(RequestId(1), params);
        let rows = render_rows(&overlay, 60, 12)?;
        assert!(
            rows.iter().any(|row| row.contains("Model's note")),
            "reason callout missing: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("ready for review")),
            "reason text missing: {rows:?}"
        );
        Ok(())
    }

    #[test]
    fn set_plan_refreshes_body() -> Result<(), Box<dyn std::error::Error>> {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        overlay.set_plan("# Edited plan\n\nNew content here.".into());
        let rows = render_rows(&overlay, 60, 12)?;
        assert!(
            rows.iter().any(|row| row.contains("New content here")),
            "edited plan body missing: {rows:?}"
        );
        Ok(())
    }

    #[test]
    fn renders_plan_markdown_with_pinned_header_and_footer()
    -> Result<(), Box<dyn std::error::Error>> {
        let overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        let rows = render_rows(&overlay, 60, 12)?;
        assert!(
            rows.iter().any(|row| row.contains("Plan approval")),
            "header missing: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Refactor plan")),
            "plan body missing: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("a approve")),
            "footer missing: {rows:?}"
        );
        Ok(())
    }

    #[test]
    fn scrolling_keeps_tail_reachable() -> Result<(), Box<dyn std::error::Error>> {
        let mut params = sample_params();
        params.plan = (0..40)
            .map(|i| format!("Step number {i} of the plan."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), params);
        // Prime max_scroll with a render, then jump to the bottom.
        let _ = render_rows(&overlay, 60, 10)?;
        overlay.handle_key(key(KeyCode::End));

        let rows = render_rows(&overlay, 60, 10)?;
        assert!(
            rows.iter().any(|row| row.contains("Step number 39")),
            "tail not reachable after scrolling: {rows:?}"
        );
        Ok(())
    }
}
