use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use smooth_protocol::FileChangeOutput;

use crate::{diff_render::create_diff_summary, wrap};

pub(crate) type TranscriptItemId = u64;

#[derive(Debug, Clone)]
pub(crate) struct TranscriptItem {
    id: TranscriptItemId,
    version: u64,
    kind: TranscriptItemKind,
}

impl TranscriptItem {
    pub(crate) fn user(id: TranscriptItemId, message: String) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::User { message },
        }
    }

    pub(crate) fn assistant(
        id: TranscriptItemId,
        lines: Vec<Line<'static>>,
        is_first_line: bool,
    ) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Assistant {
                lines,
                is_first_line,
            },
        }
    }

    pub(crate) fn reasoning(
        id: TranscriptItemId,
        lines: Vec<Line<'static>>,
        is_first_line: bool,
    ) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Reasoning {
                lines,
                is_first_line,
            },
        }
    }

    pub(crate) fn plain(id: TranscriptItemId, lines: Vec<Line<'static>>) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Plain { lines },
        }
    }

    pub(crate) fn info(id: TranscriptItemId, message: impl Into<String>) -> Self {
        Self::plain(
            id,
            vec![Line::from(vec![
                Span::styled("i ", Style::default().fg(Color::Yellow).bold()),
                Span::styled(message.into(), Style::default().dim()),
            ])],
        )
    }

    pub(crate) fn error(id: TranscriptItemId, message: impl Into<String>) -> Self {
        Self::plain(
            id,
            vec![Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red).bold()),
                Span::styled(message.into(), Style::default().fg(Color::Red)),
            ])],
        )
    }

    pub(crate) fn patch(id: TranscriptItemId, file_change: FileChangeOutput) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Patch { file_change },
        }
    }

    pub(crate) fn tool_group(id: TranscriptItemId, cell: ToolCallGroupCell) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::ToolCallGroup(cell),
        }
    }

    pub(crate) fn id(&self) -> TranscriptItemId {
        self.id
    }

    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    pub(crate) fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        match &self.kind {
            TranscriptItemKind::User { message } => wrap_display(
                prefix_multiline_text(message, "› ".cyan().bold(), "  ".cyan()),
                width,
            ),
            TranscriptItemKind::Assistant {
                lines,
                is_first_line,
            } => {
                let first_prefix = if *is_first_line {
                    "• ".green().bold()
                } else {
                    "  ".green()
                };
                wrap_markdown_lines(lines, first_prefix, "  ".green(), false, width)
            }
            TranscriptItemKind::Reasoning {
                lines,
                is_first_line,
            } => {
                let first_prefix = if *is_first_line {
                    Span::styled("… ", Style::default().fg(Color::Magenta).dim().bold())
                } else {
                    Span::styled("  ", Style::default().fg(Color::Magenta).dim())
                };
                wrap_markdown_lines(
                    lines,
                    first_prefix,
                    Span::styled("  ", Style::default().fg(Color::Magenta).dim()),
                    true,
                    width,
                )
            }
            TranscriptItemKind::Plain { lines } => wrap_display(lines.clone(), width),
            TranscriptItemKind::Patch { file_change } => create_diff_summary(file_change, width),
            TranscriptItemKind::ToolCallGroup(cell) => {
                wrap::wrap_lines_char(cell.display_lines(), usize::from(width.max(1)))
            }
        }
    }

    pub(crate) fn tool_group_mut(&mut self) -> Option<&mut ToolCallGroupCell> {
        match &mut self.kind {
            TranscriptItemKind::ToolCallGroup(cell) => Some(cell),
            _ => None,
        }
    }

    pub(crate) fn replace_with_patch(&mut self, file_change: FileChangeOutput) {
        self.kind = TranscriptItemKind::Patch { file_change };
        self.version = self.version.saturating_add(1);
    }

    pub(crate) fn mark_mutated(&mut self) {
        self.version = self.version.saturating_add(1);
    }
}

#[derive(Debug, Clone)]
enum TranscriptItemKind {
    User {
        message: String,
    },
    Assistant {
        lines: Vec<Line<'static>>,
        is_first_line: bool,
    },
    Reasoning {
        lines: Vec<Line<'static>>,
        is_first_line: bool,
    },
    Plain {
        lines: Vec<Line<'static>>,
    },
    Patch {
        file_change: FileChangeOutput,
    },
    ToolCallGroup(ToolCallGroupCell),
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

    pub(crate) fn entry_count(&self) -> usize {
        self.entries.len()
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

    pub(crate) fn display_lines(&self) -> Vec<Line<'static>> {
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

fn wrap_display(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    wrap::wrap_lines(lines, usize::from(width.max(1)))
}

/// Wrap markdown-rendered content, choosing the wrap policy per logical line:
/// prose word-wraps, while fenced code lines (marked at the line level)
/// wrap column-faithfully so indentation and alignment survive. `dim` applies
/// the reasoning styling. The prefix is added to the first row of each logical
/// line, matching the earlier prefix-then-wrap behavior.
fn wrap_markdown_lines(
    lines: &[Line<'static>],
    first_prefix: Span<'static>,
    rest_prefix: Span<'static>,
    dim: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let wrap_width = usize::from(width.max(1));
    let mut out = Vec::new();
    for (idx, raw) in lines.iter().enumerate() {
        let preformatted = is_preformatted_line(raw);
        let line_style = if dim {
            Style::default().dim()
        } else {
            raw.style
        };
        let prefix = if idx == 0 {
            first_prefix.clone()
        } else {
            rest_prefix.clone()
        };
        let mut spans = Vec::with_capacity(raw.spans.len() + 1);
        spans.push(prefix);
        spans.extend(raw.spans.iter().cloned());
        let prefixed = Line::from(spans).style(line_style);
        if preformatted {
            out.extend(wrap::wrap_line_char(prefixed, wrap_width));
        } else {
            out.extend(wrap::wrap_line(prefixed, wrap_width));
        }
    }
    out
}

/// A markdown line is preformatted (column-faithful wrapping) when the markdown
/// renderer marked the whole line as fenced code. Inline code only colors spans,
/// so even an inline-code-only paragraph still word-wraps as prose.
fn is_preformatted_line(line: &Line<'static>) -> bool {
    !line.spans.is_empty() && line.style.fg == Some(crate::markdown_render::CODE_COLOR)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::CODE_COLOR;

    #[test]
    fn fenced_code_line_marker_is_preformatted() {
        let line = Line::from(vec![Span::styled(
            "    let x = 1;",
            Style::default().fg(CODE_COLOR),
        )])
        .style(Style::default().fg(CODE_COLOR));
        assert!(is_preformatted_line(&line));
    }

    #[test]
    fn prose_with_inline_code_is_not_preformatted() {
        let line = Line::from(vec![
            Span::raw("use the "),
            Span::styled("foo", Style::default().fg(CODE_COLOR)),
            Span::raw(" helper"),
        ]);
        assert!(!is_preformatted_line(&line));
    }

    #[test]
    fn inline_code_only_line_is_not_preformatted() {
        let line = Line::from(vec![Span::styled(
            "cargo test -p smooth-tui",
            Style::default().fg(CODE_COLOR),
        )]);
        assert!(!is_preformatted_line(&line));
    }

    #[test]
    fn blank_line_is_not_preformatted() {
        assert!(!is_preformatted_line(&Line::default()));
    }
}
