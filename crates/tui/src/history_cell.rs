use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

pub(crate) trait HistoryCell: std::fmt::Debug + Send + Sync {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;
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
