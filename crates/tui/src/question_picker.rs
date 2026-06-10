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
    widgets::Paragraph,
};

use crate::composer::ComposerState;
use crate::wrap;

const OTHER_LABEL: &str = "Other (type your own answer)";

pub(crate) enum PickerOutcome {
    None,
    Confirm(AskUserQuestionResponse),
    Cancel,
}

/// Pre-wrapped picker rows, split so `render` can keep the header and footer
/// pinned while scrolling only the option list. `blocks` holds one wrapped block
/// per selectable row (the options followed by the virtual "Other" row), and
/// `active` indexes the block under the cursor.
struct PickerLayout {
    header: Vec<Line<'static>>,
    blocks: Vec<Vec<Line<'static>>>,
    footer: Vec<Line<'static>>,
    active: usize,
}

struct QuestionState {
    cursor: usize,
    multi_selected: HashSet<usize>,
    other_editor: ComposerState,
    other_editing: bool,
}

impl QuestionState {
    fn new() -> Self {
        Self {
            cursor: 0,
            multi_selected: HashSet::new(),
            other_editor: ComposerState::default(),
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
                    if state.other_editor.as_str().trim().is_empty() {
                        self.hint = Some("Type an answer or press Esc to cancel".to_string());
                    } else {
                        state.other_editing = false;
                        if multi {
                            state.multi_selected.insert(other_row);
                        }
                        return self.advance_or_confirm();
                    }
                }
                KeyCode::Backspace => state.other_editor.backspace(),
                KeyCode::Delete => state.other_editor.delete(),
                KeyCode::Left => state.other_editor.move_left(),
                KeyCode::Right => state.other_editor.move_right(),
                KeyCode::Home => state.other_editor.move_line_start(),
                KeyCode::End => state.other_editor.move_line_end(),
                KeyCode::Char(ch)
                    if key.modifiers.is_empty()
                        || key.modifiers == KeyModifiers::SHIFT
                        || key.modifiers == KeyModifiers::NONE =>
                {
                    state.other_editor.insert_char(ch);
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
                    if state.other_editor.as_str().trim().is_empty() {
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

    /// Insert pasted text into the "Other" field. The field is single-line, so
    /// line breaks and tabs are flattened to spaces.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        let Some(state) = self.states.get_mut(self.current) else {
            return;
        };
        if !state.other_editing {
            return;
        }
        let flat = text.replace("\r\n", " ").replace(['\r', '\n', '\t'], " ");
        state.other_editor.insert_str(&flat);
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
                            let txt = state.other_editor.as_str().trim();
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
                let txt = state.other_editor.as_str().trim();
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

    /// Build the pre-wrapped header/option/footer rows for the current question.
    /// `render` and `desired_height` both go through this so their row counts can
    /// never drift.
    fn layout(&self, width: u16) -> Option<PickerLayout> {
        let question = self.questions.get(self.current)?;
        let state = self.states.get(self.current)?;
        let wrap_width = usize::from(width.max(1));
        let other_row = question.options.len();
        let multi = question.multi_select;
        let total = self.questions.len();

        let header_text = format!(
            "[{}] {} ({}/{}){}",
            question.header,
            question.question,
            self.current + 1,
            total,
            if multi { "  multi-select" } else { "" }
        );
        let header = vec![separator_line(
            width,
            &header_text,
            Style::default().fg(Color::Cyan),
        )];

        let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
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
            let marks = format!("{cursor_mark} {select_mark} ");
            let indent = wrap::display_width(&marks);
            let mut block = wrap::wrap_line_hanging(
                Line::from(vec![
                    Span::raw(marks),
                    Span::styled(option.label.clone(), style),
                    Span::raw("  "),
                    Span::styled(
                        option.description.clone(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                wrap_width,
                indent,
            );
            if let Some(preview) = &option.preview {
                block.extend(wrap::wrap_line_hanging(
                    Line::from(Span::styled(
                        format!("       preview: {preview}"),
                        Style::default().fg(Color::DarkGray),
                    )),
                    wrap_width,
                    7,
                ));
            }
            blocks.push(block);
        }

        // "Other" virtual row.
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
        let mut other_style = Style::default();
        if state.cursor == other_row {
            other_style = other_style.add_modifier(Modifier::BOLD).fg(Color::Cyan);
        }
        if state.other_editing {
            other_style = other_style.add_modifier(Modifier::UNDERLINED);
        }
        let other_marks = format!("{other_cursor} {other_mark} ");
        let other_indent = wrap::display_width(&other_marks);
        let mut other_spans = vec![Span::raw(other_marks)];
        if state.other_editing {
            // Split the text at the editing cursor so a reversed cell marks it.
            let text = state.other_editor.as_str();
            let cursor = state.other_editor.cursor();
            let cursor_style = other_style.add_modifier(Modifier::REVERSED);
            other_spans.push(Span::styled("Other: ".to_string(), other_style));
            other_spans.push(Span::styled(text[..cursor].to_string(), other_style));
            match text[cursor..].chars().next() {
                Some(ch) => {
                    other_spans.push(Span::styled(ch.to_string(), cursor_style));
                    other_spans.push(Span::styled(
                        text[cursor + ch.len_utf8()..].to_string(),
                        other_style,
                    ));
                }
                None => other_spans.push(Span::styled(" ".to_string(), cursor_style)),
            }
        } else {
            let other_display = if state.other_editor.is_empty() {
                OTHER_LABEL.to_string()
            } else {
                format!("Other: {}", state.other_editor.as_str())
            };
            other_spans.push(Span::styled(other_display, other_style));
        }
        blocks.push(wrap::wrap_line_hanging(
            Line::from(other_spans),
            wrap_width,
            other_indent,
        ));

        let footer_text = if state.other_editing {
            "Enter to confirm  ←/→ Home End move  Esc to cancel typing".to_string()
        } else if multi {
            "Space toggle  Enter next/submit  Tab switch question  Esc cancel".to_string()
        } else {
            "↑/↓ move  Enter next/submit  Tab switch question  Esc cancel".to_string()
        };
        let mut footer = vec![Line::from("")];
        footer.extend(wrap::wrap_line_hanging(
            Line::from(Span::styled(
                footer_text,
                Style::default().fg(Color::DarkGray),
            )),
            wrap_width,
            0,
        ));
        if let Some(hint) = &self.hint {
            footer.extend(wrap::wrap_line_hanging(
                Line::from(Span::styled(
                    hint.clone(),
                    Style::default().fg(Color::Yellow),
                )),
                wrap_width,
                0,
            ));
        }

        let active = state.cursor.min(blocks.len().saturating_sub(1));
        Some(PickerLayout {
            header,
            blocks,
            footer,
            active,
        })
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(layout) = self.layout(area.width) else {
            return;
        };
        let total_height = usize::from(area.height.max(1));
        let middle_budget = total_height.saturating_sub(layout.header.len() + layout.footer.len());

        // Flatten the option blocks, tracking the active block's row range so the
        // window can keep it fully visible.
        let mut option_lines: Vec<Line<'static>> = Vec::new();
        let mut active_start = 0usize;
        let mut active_end = 0usize;
        for (idx, block) in layout.blocks.iter().enumerate() {
            if idx == layout.active {
                active_start = option_lines.len();
            }
            option_lines.extend(block.iter().cloned());
            if idx == layout.active {
                active_end = option_lines.len();
            }
        }

        let start = window_start(option_lines.len(), active_start, active_end, middle_budget);
        let end = start.saturating_add(middle_budget).min(option_lines.len());

        let mut lines = Vec::with_capacity(total_height);
        lines.extend(layout.header);
        if start < end {
            lines.extend(option_lines[start..end].iter().cloned());
        }
        lines.extend(layout.footer);

        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(crate) fn desired_height(&self, area_width: u16) -> u16 {
        let Some(layout) = self.layout(area_width) else {
            return 1;
        };
        let total = layout.header.len()
            + layout.blocks.iter().map(Vec::len).sum::<usize>()
            + layout.footer.len();
        u16::try_from(total).unwrap_or(u16::MAX)
    }
}

/// Choose the first visible option-row index so a `budget`-tall window keeps the
/// active block (`active_start..active_end`) visible. When the active block alone
/// is taller than the window its top is pinned and the bottom is clipped.
fn window_start(total: usize, active_start: usize, active_end: usize, budget: usize) -> usize {
    if budget == 0 || total <= budget {
        return 0;
    }
    let block_height = active_end.saturating_sub(active_start);
    if block_height >= budget {
        active_start
    } else {
        active_end.saturating_sub(budget)
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
    use app_server_protocol::AskUserQuestionOption;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_params() -> AskUserQuestionParams {
        AskUserQuestionParams {
            thread_id: "t".into(),
            turn_id: "u".into(),
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
    fn other_editor_cursor_movement() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        // Type "ab", move left, type "c" -> "acb".
        picker.handle_key(key(KeyCode::Char('a')));
        picker.handle_key(key(KeyCode::Char('b')));
        picker.handle_key(key(KeyCode::Left));
        picker.handle_key(key(KeyCode::Char('c')));
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(resp.answers[0].selected, vec!["acb".to_string()]);
            }
            _ => panic!("expected confirm with edited free-text"),
        }
    }

    #[test]
    fn other_editor_home_end_delete() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        // Type "xab", Home, Delete -> "ab", End, type "c" -> "abc".
        picker.handle_key(key(KeyCode::Char('x')));
        picker.handle_key(key(KeyCode::Char('a')));
        picker.handle_key(key(KeyCode::Char('b')));
        picker.handle_key(key(KeyCode::Home));
        picker.handle_key(key(KeyCode::Delete));
        picker.handle_key(key(KeyCode::End));
        picker.handle_key(key(KeyCode::Char('c')));
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(resp.answers[0].selected, vec!["abc".to_string()]);
            }
            _ => panic!("expected confirm with edited free-text"),
        }
    }

    #[test]
    fn handle_paste_flattens_newlines() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_paste("multi\r\nline\tpaste");
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(
                    resp.answers[0].selected,
                    vec!["multi line paste".to_string()]
                );
            }
            _ => panic!("expected confirm with pasted free-text"),
        }
    }

    #[test]
    fn handle_paste_ignored_when_not_editing() {
        let mut picker = QuestionPicker::new(RequestId(1), sample_params());
        picker.handle_paste("ignored");
        match picker.handle_key(key(KeyCode::Enter)) {
            PickerOutcome::Confirm(resp) => {
                assert_eq!(resp.answers[0].selected, vec!["First".to_string()]);
            }
            _ => panic!("expected confirm of the first option"),
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

    fn buffer_rows(terminal: &Terminal<TestBackend>, width: usize) -> Vec<String> {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .chunks(width)
            .map(|row| row.concat())
            .collect()
    }

    #[test]
    fn window_start_keeps_active_block_visible() {
        // Everything fits: no scrolling.
        assert_eq!(window_start(5, 0, 1, 10), 0);
        // Active block near the end anchors to the window bottom.
        assert_eq!(window_start(20, 18, 20, 5), 15);
        // Active block taller than the window pins its top.
        assert_eq!(window_start(20, 2, 12, 5), 2);
        // Active block early stays at the top.
        assert_eq!(window_start(20, 0, 2, 5), 0);
    }

    #[test]
    fn desired_height_counts_wrapped_rows() {
        let mut params = sample_params();
        params.questions[0].options[0].description =
            "a very long description that will certainly wrap across several lines at a narrow width"
                .into();
        let picker = QuestionPicker::new(RequestId(1), params);

        let wide = picker.desired_height(200);
        let narrow = picker.desired_height(24);
        assert!(narrow > wide, "narrow={narrow} wide={wide}");

        // desired_height must equal the flattened layout that render draws.
        let Some(layout) = picker.layout(24) else {
            panic!("expected a layout");
        };
        let sum = layout.header.len()
            + layout.blocks.iter().map(Vec::len).sum::<usize>()
            + layout.footer.len();
        assert_eq!(usize::from(narrow), sum);
    }

    #[test]
    fn active_option_stays_visible_under_height_cap() -> Result<(), Box<dyn std::error::Error>> {
        let mut params = sample_params();
        for i in 0..30 {
            params.questions[0].options.push(AskUserQuestionOption {
                label: format!("Option{i:02}"),
                description: format!("description for option {i}"),
                preview: None,
            });
        }
        let mut picker = QuestionPicker::new(RequestId(1), params);
        // Move the cursor to the last real option (First, Second, Option00..29).
        for _ in 0..31 {
            picker.handle_key(key(KeyCode::Down));
        }

        let width = 40usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 20))?;
        terminal.draw(|frame| picker.render(frame, frame.area()))?;
        let rows = buffer_rows(&terminal, width);

        // Header and footer stay pinned, the active option stays visible.
        assert!(
            rows.iter().any(|r| r.contains("Pick")),
            "header missing: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("Esc cancel")),
            "footer missing: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("Option29")),
            "active option not visible: {rows:?}"
        );
        Ok(())
    }
}
