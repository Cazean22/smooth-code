use std::any::Any;

use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

pub(crate) trait HistoryCell: std::fmt::Debug + Send + Sync {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

#[derive(Debug, Clone)]
pub(crate) struct UserHistoryCell {
    message: String,
}

impl UserHistoryCell {
    pub(crate) fn new(message: String) -> Self {
        Self { message }
    }
}

impl HistoryCell for UserHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        prefix_multiline_text(&self.message, "› ".cyan().bold(), "  ".cyan())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl AgentMessageCell {
    pub(crate) fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }
}

impl HistoryCell for AgentMessageCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let first_prefix = if self.is_first_line {
            "• ".green().bold()
        } else {
            "  ".green()
        };
        prefix_lines(self.lines.clone(), first_prefix, "  ".green())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReasoningCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl ReasoningCell {
    pub(crate) fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl HistoryCell for ReasoningCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let first_prefix = if self.is_first_line {
            Span::styled("… ", Style::default().fg(Color::Magenta).dim().bold())
        } else {
            Span::styled("  ", Style::default().fg(Color::Magenta).dim())
        };
        prefix_lines(
            self.lines
                .clone()
                .into_iter()
                .map(|line| line.style(Style::default().dim()))
                .collect(),
            first_prefix,
            Span::styled("  ", Style::default().fg(Color::Magenta).dim()),
        )
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PlainHistoryCell {
    lines: Vec<Line<'static>>,
}

impl PlainHistoryCell {
    pub(crate) fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }

    pub(crate) fn info(message: impl Into<String>) -> Self {
        Self::new(vec![Line::from(vec![
            Span::styled("i ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(message.into(), Style::default().dim()),
        ])])
    }

    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self::new(vec![Line::from(vec![
            Span::styled("! ", Style::default().fg(Color::Red).bold()),
            Span::styled(message.into(), Style::default().fg(Color::Red)),
        ])])
    }
}

impl HistoryCell for PlainHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ToolCallState {
    Running,
    Success,
    Failure,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallEntry {
    args_preview: String,
    state: ToolCallState,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallGroupCell {
    tool_name: String,
    entries: Vec<ToolCallEntry>,
}

impl ToolCallGroupCell {
    pub(crate) fn new(tool_name: String, args_preview: String) -> Self {
        Self {
            tool_name,
            entries: vec![ToolCallEntry {
                args_preview,
                state: ToolCallState::Running,
                error: None,
            }],
        }
    }

    pub(crate) fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub(crate) fn push_entry(&mut self, args_preview: String) -> usize {
        let entry_idx = self.entries.len();
        self.entries.push(ToolCallEntry {
            args_preview,
            state: ToolCallState::Running,
            error: None,
        });
        entry_idx
    }

    pub(crate) fn set_entry_outcome(
        &mut self,
        entry_idx: usize,
        state: ToolCallState,
        error: Option<String>,
    ) {
        let entry = self
            .entries
            .get_mut(entry_idx)
            .expect("tool call entry index should be valid");
        entry.state = state;
        entry.error = error;
    }
}

impl HistoryCell for ToolCallGroupCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let header_state = if self
            .entries
            .iter()
            .any(|entry| matches!(entry.state, ToolCallState::Failure))
        {
            ToolCallState::Failure
        } else if self
            .entries
            .iter()
            .any(|entry| matches!(entry.state, ToolCallState::Running))
        {
            ToolCallState::Running
        } else {
            ToolCallState::Success
        };

        if self.entries.len() == 1 {
            let entry = &self.entries[0];
            let mut lines = vec![tool_call_line(
                "",
                entry.state,
                Some(self.tool_name()),
                &entry.args_preview,
            )];
            if matches!(entry.state, ToolCallState::Failure)
                && let Some(error) = entry.error.as_deref()
            {
                lines.push(tool_error_line("      ", error));
            }
            return lines;
        }

        let mut lines = vec![tool_call_line("", header_state, Some(self.tool_name()), "")];
        for entry in &self.entries {
            lines.push(tool_call_line(
                "      ",
                entry.state,
                None,
                &entry.args_preview,
            ));
            if matches!(entry.state, ToolCallState::Failure)
                && let Some(error) = entry.error.as_deref()
            {
                lines.push(tool_error_line("        ", error));
            }
        }
        lines
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn tool_call_line(
    indent: &'static str,
    state: ToolCallState,
    label: Option<&str>,
    args_preview: &str,
) -> Line<'static> {
    let mut spans = Vec::new();
    if !indent.is_empty() {
        spans.push(Span::raw(indent));
    }
    spans.push(tool_call_glyph(state));
    if let Some(label) = label {
        spans.push(Span::raw(label.to_owned()));
    }
    if !args_preview.is_empty() {
        if label.is_some() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            args_preview.to_owned(),
            Style::default().dim(),
        ));
    }
    Line::from(spans)
}

fn tool_call_glyph(state: ToolCallState) -> Span<'static> {
    let (glyph, glyph_style) = match state {
        ToolCallState::Running => ("⠋ ", Style::default().fg(Color::Yellow).bold()),
        ToolCallState::Success => ("✓ ", Style::default().fg(Color::Green).bold()),
        ToolCallState::Failure => ("✗ ", Style::default().fg(Color::Red).bold()),
    };
    Span::styled(glyph, glyph_style)
}

fn tool_error_line(indent: &'static str, error: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(indent),
        Span::styled("! ", Style::default().fg(Color::Red).bold()),
        Span::styled(error.to_owned(), Style::default().fg(Color::Red).dim()),
    ])
}

pub(crate) fn prefix_lines(
    lines: Vec<Line<'static>>,
    first_prefix: Span<'static>,
    rest_prefix: Span<'static>,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for (idx, line) in lines.into_iter().enumerate() {
        let prefix = if idx == 0 {
            first_prefix.clone()
        } else {
            rest_prefix.clone()
        };
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        spans.push(prefix);
        spans.extend(line.spans);
        out.push(Line::from(spans).style(line.style));
    }
    out
}

fn prefix_multiline_text(
    text: &str,
    first_prefix: Span<'static>,
    rest_prefix: Span<'static>,
) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::from(vec![first_prefix])];
    }

    let lines = text
        .split('\n')
        .map(|line| Line::from(line.to_owned()))
        .collect::<Vec<_>>();
    prefix_lines(lines, first_prefix, rest_prefix)
}
