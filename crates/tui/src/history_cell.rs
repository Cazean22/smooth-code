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

    /// Plain transcript rows whose continuation lines hang-indent by
    /// `hang_indent` columns, so a glyph-prefixed message keeps its alignment
    /// when it wraps.
    fn plain_hanging(id: TranscriptItemId, lines: Vec<Line<'static>>, hang_indent: usize) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Plain { lines, hang_indent },
        }
    }

    pub(crate) fn info(id: TranscriptItemId, message: impl Into<String>) -> Self {
        Self::plain_hanging(
            id,
            vec![Line::from(vec![
                Span::styled("i ", Style::default().fg(Color::Yellow).bold()),
                Span::styled(message.into(), Style::default().dim()),
            ])],
            2,
        )
    }

    pub(crate) fn error(id: TranscriptItemId, message: impl Into<String>) -> Self {
        Self::plain_hanging(
            id,
            vec![Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red).bold()),
                Span::styled(message.into(), Style::default().fg(Color::Red)),
            ])],
            2,
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
            TranscriptItemKind::Plain { lines, hang_indent } => {
                let wrap_width = usize::from(width.max(1));
                lines
                    .iter()
                    .cloned()
                    .flat_map(|line| wrap::wrap_line_hanging(line, wrap_width, *hang_indent))
                    .collect()
            }
            TranscriptItemKind::Patch { file_change } => create_diff_summary(file_change, width),
            TranscriptItemKind::ToolCallGroup(cell) => {
                cell.display_lines(usize::from(width.max(1)))
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
        hang_indent: usize,
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
        if let Some(entry) = self.entries.get_mut(entry_idx) {
            entry.state = state;
            entry.error = error;
        }
    }

    pub(crate) fn display_lines(&self, width: usize) -> Vec<Line<'static>> {
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

        let mut lines = Vec::new();
        if self.entries.len() == 1 {
            let entry = &self.entries[0];
            let (line, indent) =
                tool_call_line("", entry.state, Some(self.tool_name()), &entry.args_preview);
            lines.extend(wrap::wrap_line_char_hanging(line, width, indent));
            if matches!(entry.state, ToolCallState::Failure)
                && let Some(error) = entry.error.as_deref()
            {
                let (line, indent) = tool_error_line("      ", error);
                lines.extend(wrap::wrap_line_char_hanging(line, width, indent));
            }
            return lines;
        }

        let (header, indent) = tool_call_line("", header_state, Some(self.tool_name()), "");
        lines.extend(wrap::wrap_line_char_hanging(header, width, indent));
        for entry in &self.entries {
            let (line, indent) = tool_call_line("      ", entry.state, None, &entry.args_preview);
            lines.extend(wrap::wrap_line_char_hanging(line, width, indent));
            if matches!(entry.state, ToolCallState::Failure)
                && let Some(error) = entry.error.as_deref()
            {
                let (line, indent) = tool_error_line("        ", error);
                lines.extend(wrap::wrap_line_char_hanging(line, width, indent));
            }
        }
        lines
    }
}

/// Build a tool-call row and report the column at which its args content starts,
/// so wrapped continuation rows can hang-indent under the args rather than the
/// glyph or tool name.
fn tool_call_line(
    indent: &'static str,
    state: ToolCallState,
    label: Option<&str>,
    args_preview: &str,
) -> (Line<'static>, usize) {
    let mut spans = Vec::new();
    let mut content_col = 0usize;
    if !indent.is_empty() {
        spans.push(Span::raw(indent));
        content_col += wrap::display_width(indent);
    }
    let glyph = tool_call_glyph(state);
    content_col += wrap::display_width(glyph.content.as_ref());
    spans.push(glyph);
    if let Some(label) = label {
        content_col += wrap::display_width(label);
        spans.push(Span::raw(label.to_owned()));
    }
    if !args_preview.is_empty() {
        if label.is_some() {
            spans.push(Span::raw(" "));
            content_col += 1;
        }
        spans.push(Span::styled(
            args_preview.to_owned(),
            Style::default().dim(),
        ));
    }
    (Line::from(spans), content_col)
}

fn tool_call_glyph(state: ToolCallState) -> Span<'static> {
    let (glyph, glyph_style) = match state {
        ToolCallState::Running => ("⠋ ", Style::default().fg(Color::Yellow).bold()),
        ToolCallState::Success => ("✓ ", Style::default().fg(Color::Green).bold()),
        ToolCallState::Failure => ("✗ ", Style::default().fg(Color::Red).bold()),
    };
    Span::styled(glyph, glyph_style)
}

/// Build a tool failure row and report the column after the `! ` marker so
/// wrapped continuation rows hang-indent under the error text.
fn tool_error_line(indent: &'static str, error: &str) -> (Line<'static>, usize) {
    let content_col = wrap::display_width(indent) + 2;
    let line = Line::from(vec![
        Span::raw(indent),
        Span::styled("! ", Style::default().fg(Color::Red).bold()),
        Span::styled(error.to_owned(), Style::default().fg(Color::Red).dim()),
    ]);
    (line, content_col)
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

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn within_width(lines: &[Line<'static>], width: usize) {
        for line in lines {
            assert!(
                crate::wrap::display_width(&line_text(line)) <= width,
                "line exceeds width {width}: {:?}",
                line_text(line)
            );
        }
    }

    #[test]
    fn single_tool_args_hang_indent_under_args() {
        let cell = ToolCallGroupCell::new(
            "run_command".to_string(),
            "echo hello world this is a long command line".to_string(),
        );
        let lines = cell.display_lines(20);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(texts.len() > 1, "expected wrapping: {texts:?}");
        // glyph (2) + "run_command" (11) + space (1) = 14.
        let indent = " ".repeat(14);
        for cont in &texts[1..] {
            assert!(cont.starts_with(&indent), "continuation not hung: {cont:?}");
        }
        within_width(&lines, 20);
    }

    #[test]
    fn grouped_tool_entries_hang_indent_under_nested_args() {
        let mut cell = ToolCallGroupCell::new(
            "run".to_string(),
            "first command that is quite long indeed yes".to_string(),
        );
        cell.push_entry("second command also long enough to wrap nicely".to_string());
        let lines = cell.display_lines(24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        // entry indent "      " (6) + glyph (2) = 8.
        let indent = " ".repeat(8);
        assert!(
            texts.iter().skip(1).any(|t| t.starts_with(&indent)),
            "no nested continuation found: {texts:?}"
        );
        within_width(&lines, 24);
    }

    #[test]
    fn failed_tool_error_hangs_indent_under_message() {
        let mut cell = ToolCallGroupCell::new("run".to_string(), "do thing".to_string());
        cell.set_entry_outcome(
            0,
            ToolCallState::Failure,
            Some("a long failure explanation that needs to wrap across rows".to_string()),
        );
        let lines = cell.display_lines(20);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        // error indent "      " (6) + "! " (2) = 8.
        let indent = " ".repeat(8);
        assert!(
            texts.iter().any(|t| t.starts_with(&indent)),
            "error not hung: {texts:?}"
        );
        within_width(&lines, 20);
    }

    #[test]
    fn info_row_hangs_indent_under_message() {
        let item = TranscriptItem::info(
            1,
            "this is a fairly long informational message that should wrap",
        );
        let lines = item.display_lines(20);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(texts.len() > 1, "expected wrapping: {texts:?}");
        assert!(texts[0].starts_with("i "));
        for cont in &texts[1..] {
            assert!(
                cont.starts_with("  "),
                "info continuation not hung: {cont:?}"
            );
        }
        within_width(&lines, 20);
    }

    #[test]
    fn error_row_hangs_indent_under_message() {
        let item = TranscriptItem::error(
            1,
            "this is a fairly long error message that should certainly wrap",
        );
        let lines = item.display_lines(20);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(texts.len() > 1, "expected wrapping: {texts:?}");
        assert!(texts[0].starts_with("! "));
        for cont in &texts[1..] {
            assert!(
                cont.starts_with("  "),
                "error continuation not hung: {cont:?}"
            );
        }
        within_width(&lines, 20);
    }
}
