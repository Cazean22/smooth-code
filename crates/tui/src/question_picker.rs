use std::collections::HashSet;

use app_server_protocol::{
    AskUserQuestion, AskUserQuestionAnswer, AskUserQuestionParams, AskUserQuestionResponse,
    RequestId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

const OTHER_LABEL: &str = "Other (type your own answer)";

pub(crate) enum PickerOutcome {
    None,
    Confirm(AskUserQuestionResponse),
    Cancel,
}

struct QuestionState {
    cursor: usize,
    multi_selected: HashSet<usize>,
    other_text: String,
    other_editing: bool,
}

impl QuestionState {
    fn new() -> Self {
        Self {
            cursor: 0,
            multi_selected: HashSet::new(),
            other_text: String::new(),
            other_editing: false,
        }
    }
}

pub(crate) struct QuestionPicker {
    pub(crate) request_id: RequestId,
    questions: Vec<AskUserQuestion>,
    current: usize,
    states: Vec<QuestionState>,
    hint: Option<String>,
}

impl QuestionPicker {
    pub(crate) fn new(request_id: RequestId, params: AskUserQuestionParams) -> Self {
        let states = (0..params.questions.len())
            .map(|_| QuestionState::new())
            .collect();
        Self {
            request_id,
            questions: params.questions,
            current: 0,
            states,
            hint: None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return PickerOutcome::None;
        }

        self.hint = None;
        let question = match self.questions.get(self.current) {
            Some(q) => q.clone(),
            None => return PickerOutcome::None,
        };
        let multi = question.multi_select;
        let other_row = question.options.len();
        let state = match self.states.get_mut(self.current) {
            Some(s) => s,
            None => return PickerOutcome::None,
        };

        if state.other_editing {
            match key.code {
                KeyCode::Esc => {
                    state.other_editing = false;
                }
                KeyCode::Enter => {
                    if state.other_text.trim().is_empty() {
                        self.hint = Some("Type an answer or press Esc to cancel".to_string());
                    } else {
                        state.other_editing = false;
                        if multi {
                            state.multi_selected.insert(other_row);
                        }
                        return self.advance_or_confirm();
                    }
                }
                KeyCode::Backspace => {
                    state.other_text.pop();
                }
                KeyCode::Char(ch)
                    if key.modifiers.is_empty()
                        || key.modifiers == KeyModifiers::SHIFT
                        || key.modifiers == KeyModifiers::NONE =>
                {
                    state.other_text.push(ch);
                }
                _ => {}
            }
            return PickerOutcome::None;
        }

        match key.code {
            KeyCode::Esc => PickerOutcome::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                }
                PickerOutcome::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if state.cursor < other_row {
                    state.cursor += 1;
                }
                PickerOutcome::None
            }
            KeyCode::Tab => {
                if self.current + 1 < self.questions.len() {
                    self.current += 1;
                }
                PickerOutcome::None
            }
            KeyCode::BackTab => {
                if self.current > 0 {
                    self.current -= 1;
                }
                PickerOutcome::None
            }
            KeyCode::Char(' ') if multi => {
                if state.cursor == other_row {
                    state.other_editing = true;
                } else if state.multi_selected.contains(&state.cursor) {
                    state.multi_selected.remove(&state.cursor);
                } else {
                    state.multi_selected.insert(state.cursor);
                }
                PickerOutcome::None
            }
            KeyCode::Enter => {
                if state.cursor == other_row {
                    if state.other_text.trim().is_empty() {
                        state.other_editing = true;
                        return PickerOutcome::None;
                    }
                    if multi {
                        state.multi_selected.insert(other_row);
                    }
                }
                self.advance_or_confirm()
            }
            _ => PickerOutcome::None,
        }
    }

    fn advance_or_confirm(&mut self) -> PickerOutcome {
        if self.current + 1 < self.questions.len() {
            self.current += 1;
            PickerOutcome::None
        } else if let Some(response) = self.build_response() {
            PickerOutcome::Confirm(response)
        } else {
            self.hint = Some("Each question needs at least one selection".to_string());
            PickerOutcome::None
        }
    }

    fn build_response(&self) -> Option<AskUserQuestionResponse> {
        let mut answers = Vec::with_capacity(self.questions.len());
        for (idx, question) in self.questions.iter().enumerate() {
            let state = self.states.get(idx)?;
            let other_row = question.options.len();
            let (selected, preview) = if question.multi_select {
                let mut picks: Vec<(usize, String)> = state
                    .multi_selected
                    .iter()
                    .copied()
                    .filter_map(|i| {
                        if i == other_row {
                            let txt = state.other_text.trim();
                            if txt.is_empty() {
                                None
                            } else {
                                Some((i, txt.to_string()))
                            }
                        } else {
                            question.options.get(i).map(|opt| (i, opt.label.clone()))
                        }
                    })
                    .collect();
                if picks.is_empty() {
                    return None;
                }
                picks.sort_by_key(|(i, _)| *i);
                let labels: Vec<String> = picks.into_iter().map(|(_, l)| l).collect();
                (labels, None)
            } else if state.cursor == other_row {
                let txt = state.other_text.trim();
                if txt.is_empty() {
                    return None;
                }
                (vec![txt.to_string()], None)
            } else {
                let opt = question.options.get(state.cursor)?;
                (vec![opt.label.clone()], opt.preview.clone())
            };
            answers.push(AskUserQuestionAnswer {
                question: question.question.clone(),
                selected,
                preview,
            });
        }
        Some(AskUserQuestionResponse { answers })
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let question = match self.questions.get(self.current) {
            Some(q) => q,
            None => return,
        };
        let state = match self.states.get(self.current) {
            Some(s) => s,
            None => return,
        };
        let other_row = question.options.len();
        let multi = question.multi_select;
        let total = self.questions.len();
        let header = format!(
            "[{}] {} ({}/{}){}",
            question.header,
            question.question,
            self.current + 1,
            total,
            if multi { "  multi-select" } else { "" }
        );

        let mut lines: Vec<Line<'static>> = Vec::new();
        for (idx, option) in question.options.iter().enumerate() {
            let cursor_mark = if state.cursor == idx { "›" } else { " " };
            let select_mark = if multi && state.multi_selected.contains(&idx) {
                "[x]"
            } else if multi {
                "[ ]"
            } else if state.cursor == idx {
                "(•)"
            } else {
                "( )"
            };
            let mut style = Style::default();
            if state.cursor == idx {
                style = style.add_modifier(Modifier::BOLD).fg(Color::Cyan);
            }
            lines.push(Line::from(vec![
                Span::raw(format!("{cursor_mark} {select_mark} ")),
                Span::styled(option.label.clone(), style),
                Span::raw("  "),
                Span::styled(
                    option.description.clone(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            if let Some(preview) = &option.preview {
                lines.push(Line::from(Span::styled(
                    format!("       preview: {preview}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        // "Other" virtual row
        let other_cursor = if state.cursor == other_row {
            "›"
        } else {
            " "
        };
        let other_mark = if multi && state.multi_selected.contains(&other_row) {
            "[x]"
        } else if multi {
            "[ ]"
        } else if state.cursor == other_row {
            "(•)"
        } else {
            "( )"
        };
        let other_display = if state.other_text.is_empty() {
            OTHER_LABEL.to_string()
        } else {
            format!("Other: {}", state.other_text)
        };
        let mut other_style = Style::default();
        if state.cursor == other_row {
            other_style = other_style.add_modifier(Modifier::BOLD).fg(Color::Cyan);
        }
        if state.other_editing {
            other_style = other_style.add_modifier(Modifier::UNDERLINED);
        }
        lines.push(Line::from(vec![
            Span::raw(format!("{other_cursor} {other_mark} ")),
            Span::styled(other_display, other_style),
        ]));

        let footer = if state.other_editing {
            "Enter to confirm  Esc to cancel typing".to_string()
        } else if multi {
            "Space toggle  Enter next/submit  Tab switch question  Esc cancel".to_string()
        } else {
            "↑/↓ move  Enter next/submit  Tab switch question  Esc cancel".to_string()
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        )));
        if let Some(hint) = &self.hint {
            lines.push(Line::from(Span::styled(
                hint.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .title(header)
            .border_style(Style::default().fg(Color::Cyan));
        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    pub(crate) fn desired_height(&self, area_width: u16) -> u16 {
        let question = match self.questions.get(self.current) {
            Some(q) => q,
            None => return 3,
        };
        // header line + 2 borders + N option lines (some have preview) + Other + 1 blank + footer + optional hint
        let mut rows: u16 = 0;
        for opt in &question.options {
            rows += 1;
            if opt.preview.is_some() {
                rows += 1;
            }
        }
        rows += 1; // Other row
        rows += 2; // blank + footer
        if self.hint.is_some() {
            rows += 1;
        }
        rows += 2; // borders
        let _ = area_width;
        rows
    }
}

#[cfg(test)]
mod tests {
    use app_server_protocol::AskUserQuestionOption;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_params() -> AskUserQuestionParams {
        AskUserQuestionParams {
            thread_id: "t".into(),
            turn_id: "u".into(),
            call_id: "c".into(),
            questions: vec![AskUserQuestion {
                question: "Pick one".into(),
                header: "Pick".into(),
                multi_select: false,
                options: vec![
                    AskUserQuestionOption {
                        label: "First".into(),
                        description: "the first".into(),
                        preview: None,
                    },
                    AskUserQuestionOption {
                        label: "Second".into(),
                        description: "the second".into(),
                        preview: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn enter_on_single_select_confirms() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(resp.answers.len(), 1);
                assert_eq!(resp.answers[0].selected, vec!["First".to_string()]);
            }
            _ => panic!("expected confirm"),
        }
    }

    #[test]
    fn arrow_down_then_enter_selects_second() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        assert!(matches!(
            picker.handle_key(key(KeyCode::Down)),
            PickerOutcome::None
        ));
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(resp.answers[0].selected, vec!["Second".to_string()]);
            }
            _ => panic!("expected confirm"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            PickerOutcome::Cancel
        ));
    }

    #[test]
    fn other_free_text_flow() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        // Move to "Other" row (index 2).
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        // Enter starts editing.
        picker.handle_key(key(KeyCode::Enter));
        // Type "hi".
        picker.handle_key(key(KeyCode::Char('h')));
        picker.handle_key(key(KeyCode::Char('i')));
        // Confirm.
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(resp.answers[0].selected, vec!["hi".to_string()]);
            }
            _ => panic!("expected confirm with free-text"),
        }
    }

    #[test]
    fn multi_select_toggles_with_space() {
        let mut params = sample_params();
        params.questions[0].multi_select = true;
        let mut picker = QuestionPicker::new(RequestId(1), params);
        picker.handle_key(key(KeyCode::Char(' ')));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Char(' ')));
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(
                    resp.answers[0].selected,
                    vec!["First".to_string(), "Second".to_string()]
                );
            }
            _ => panic!("expected confirm"),
        }
    }
}
