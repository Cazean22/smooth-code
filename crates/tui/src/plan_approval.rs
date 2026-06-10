use app_server_protocol::{
    PlanApprovalDecision, RequestId, RequestPlanApprovalParams, RequestPlanApprovalResponse,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{markdown_render, wrap};

pub(crate) enum PlanApprovalOutcome {
    None,
    Respond(RequestPlanApprovalResponse),
}

enum Stage {
    Reviewing,
    EditingFeedback,
}

/// Overlay presenting a plan submitted via `exit_plan_mode` for the user to
/// approve or reject. Modeled on `QuestionPicker`: the header and footer stay
/// pinned while the plan body scrolls.
pub(crate) struct PlanApprovalOverlay {
    pub(crate) request_id: RequestId,
    plan: String,
    scroll: usize,
    stage: Stage,
    feedback: String,
}

struct OverlayLayout {
    header: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    footer: Vec<Line<'static>>,
}

impl PlanApprovalOverlay {
    pub(crate) fn new(request_id: RequestId, params: RequestPlanApprovalParams) -> Self {
        Self {
            request_id,
            plan: params.plan,
            scroll: 0,
            stage: Stage::Reviewing,
            feedback: String::new(),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PlanApprovalOutcome {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return PlanApprovalOutcome::None;
        }

        match self.stage {
            Stage::Reviewing => match key.code {
                KeyCode::Char('a') | KeyCode::Char('y') => {
                    PlanApprovalOutcome::Respond(RequestPlanApprovalResponse {
                        decision: PlanApprovalDecision::Approved,
                        feedback: None,
                    })
                }
                KeyCode::Char('r') => {
                    self.stage = Stage::EditingFeedback;
                    PlanApprovalOutcome::None
                }
                KeyCode::Esc => PlanApprovalOutcome::Respond(RequestPlanApprovalResponse {
                    decision: PlanApprovalDecision::Rejected,
                    feedback: None,
                }),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll = self.scroll.saturating_sub(1);
                    PlanApprovalOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll = self.scroll.saturating_add(1);
                    PlanApprovalOutcome::None
                }
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(10);
                    PlanApprovalOutcome::None
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(10);
                    PlanApprovalOutcome::None
                }
                _ => PlanApprovalOutcome::None,
            },
            Stage::EditingFeedback => match key.code {
                KeyCode::Esc => {
                    self.stage = Stage::Reviewing;
                    PlanApprovalOutcome::None
                }
                KeyCode::Enter => {
                    let feedback = self.feedback.trim();
                    PlanApprovalOutcome::Respond(RequestPlanApprovalResponse {
                        decision: PlanApprovalDecision::Rejected,
                        feedback: (!feedback.is_empty()).then(|| feedback.to_string()),
                    })
                }
                KeyCode::Backspace => {
                    self.feedback.pop();
                    PlanApprovalOutcome::None
                }
                KeyCode::Char(ch)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.feedback.push(ch);
                    PlanApprovalOutcome::None
                }
                _ => PlanApprovalOutcome::None,
            },
        }
    }

    fn layout(&self, width: u16) -> OverlayLayout {
        let wrap_width = usize::from(width.max(1));

        let header = vec![separator_line(
            width,
            "Plan approval",
            Style::default().fg(Color::Magenta),
        )];

        let body = wrap::wrap_lines(
            markdown_render::render_markdown_text(&self.plan).lines,
            wrap_width,
        );

        let mut footer = vec![Line::from("")];
        match self.stage {
            Stage::Reviewing => {
                footer.extend(wrap::wrap_line_hanging(
                    Line::from(Span::styled(
                        "a approve  r reject with feedback  Esc reject  ↑/↓ scroll",
                        Style::default().fg(Color::DarkGray),
                    )),
                    wrap_width,
                    0,
                ));
            }
            Stage::EditingFeedback => {
                footer.extend(wrap::wrap_line_hanging(
                    Line::from(vec![
                        Span::raw("Feedback: "),
                        Span::styled(
                            self.feedback.clone(),
                            Style::default().add_modifier(Modifier::UNDERLINED),
                        ),
                    ]),
                    wrap_width,
                    "Feedback: ".len(),
                ));
                footer.extend(wrap::wrap_line_hanging(
                    Line::from(Span::styled(
                        "Enter submit rejection  Esc back to review",
                        Style::default().fg(Color::DarkGray),
                    )),
                    wrap_width,
                    0,
                ));
            }
        }

        OverlayLayout {
            header,
            body,
            footer,
        }
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let layout = self.layout(area.width);
        let total_height = usize::from(area.height.max(1));
        let budget = total_height.saturating_sub(layout.header.len() + layout.footer.len());

        let max_start = layout.body.len().saturating_sub(budget);
        let start = self.scroll.min(max_start);
        let end = start.saturating_add(budget).min(layout.body.len());

        let mut lines = Vec::with_capacity(total_height);
        lines.extend(layout.header);
        if start < end {
            lines.extend(layout.body[start..end].iter().cloned());
        }
        lines.extend(layout.footer);

        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(crate) fn desired_height(&self, area_width: u16) -> u16 {
        let layout = self.layout(area_width);
        let total = layout.header.len() + layout.body.len() + layout.footer.len();
        u16::try_from(total).unwrap_or(u16::MAX)
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

    fn sample_params() -> RequestPlanApprovalParams {
        RequestPlanApprovalParams {
            thread_id: "t".into(),
            turn_id: "u".into(),
            call_id: "c".into(),
            plan: "# Refactor plan\n\n1. Move the parser.\n2. Add tests.".into(),
        }
    }

    #[test]
    fn a_approves_without_feedback() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        match overlay.handle_key(key(KeyCode::Char('a'))) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.decision, PlanApprovalDecision::Approved);
                assert_eq!(response.feedback, None);
            }
            PlanApprovalOutcome::None => panic!("expected approval response"),
        }
    }

    #[test]
    fn esc_rejects_without_feedback() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        match overlay.handle_key(key(KeyCode::Esc)) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.decision, PlanApprovalDecision::Rejected);
                assert_eq!(response.feedback, None);
            }
            PlanApprovalOutcome::None => panic!("expected rejection response"),
        }
    }

    #[test]
    fn r_collects_feedback_then_enter_rejects() {
        let mut overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        assert!(matches!(
            overlay.handle_key(key(KeyCode::Char('r'))),
            PlanApprovalOutcome::None
        ));
        for ch in "use sqlite".chars() {
            overlay.handle_key(key(KeyCode::Char(ch)));
        }
        match overlay.handle_key(key(KeyCode::Enter)) {
            PlanApprovalOutcome::Respond(response) => {
                assert_eq!(response.decision, PlanApprovalDecision::Rejected);
                assert_eq!(response.feedback.as_deref(), Some("use sqlite"));
            }
            PlanApprovalOutcome::None => panic!("expected rejection with feedback"),
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
            PlanApprovalOutcome::None => panic!("expected approval after returning to review"),
        }
    }

    #[test]
    fn renders_plan_markdown_with_pinned_header_and_footer() -> Result<(), Box<dyn std::error::Error>>
    {
        let overlay = PlanApprovalOverlay::new(RequestId(1), sample_params());
        let width = 60usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 12))?;
        terminal.draw(|frame| overlay.render(frame, frame.area()))?;
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .chunks(width)
            .map(|row| row.concat())
            .collect();

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
        for _ in 0..200 {
            overlay.handle_key(key(KeyCode::Down));
        }

        let width = 60usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 10))?;
        terminal.draw(|frame| overlay.render(frame, frame.area()))?;
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .chunks(width)
            .map(|row| row.concat())
            .collect();
        assert!(
            rows.iter().any(|row| row.contains("Step number 39")),
            "tail not reachable after scrolling: {rows:?}"
        );
        Ok(())
    }
}
