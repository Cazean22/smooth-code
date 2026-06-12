use app_server_protocol::AskUserQuestionAnswer;
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use smooth_protocol::{FileChange, FileChangeOutput, TodoItem, TodoStatus};

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
        raw: String,
    ) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Assistant {
                lines,
                is_first_line,
                raw,
            },
        }
    }

    pub(crate) fn reasoning(
        id: TranscriptItemId,
        lines: Vec<Line<'static>>,
        is_first_line: bool,
        raw: String,
    ) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::Reasoning {
                lines,
                is_first_line,
                raw,
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

    /// Summary row pushed when the user answers an `ask_user_question` picker,
    /// so the Q→A exchange stays visible in scrollback.
    pub(crate) fn question_answers(
        id: TranscriptItemId,
        answers: &[AskUserQuestionAnswer],
    ) -> Self {
        let mut lines = Vec::new();
        for answer in answers {
            lines.push(Line::from(vec![
                Span::styled("? ", Style::default().fg(Color::Cyan).bold()),
                Span::styled(answer.question.clone(), Style::default().dim()),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  → "),
                Span::styled(answer.selected.join(", "), Style::default().fg(Color::Cyan)),
            ]));
        }
        Self::plain_hanging(id, lines, 4)
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

    /// Checklist snapshot left in the transcript by a successful `todo_write`
    /// call, replacing the generic tool row.
    pub(crate) fn todo_list(id: TranscriptItemId, todos: Vec<TodoItem>) -> Self {
        Self {
            id,
            version: 0,
            kind: TranscriptItemKind::TodoList { todos },
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

    pub(crate) fn is_user(&self) -> bool {
        matches!(self.kind, TranscriptItemKind::User { .. })
    }

    pub(crate) fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        match &self.kind {
            TranscriptItemKind::User { message } => render_user_message(message, width),
            TranscriptItemKind::Assistant {
                lines,
                is_first_line,
                ..
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
                ..
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
            TranscriptItemKind::TodoList { todos } => {
                render_todo_list(todos, usize::from(width.max(1)))
            }
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

    pub(crate) fn tool_group_cell(&self) -> Option<&ToolCallGroupCell> {
        match &self.kind {
            TranscriptItemKind::ToolCallGroup(cell) => Some(cell),
            _ => None,
        }
    }

    /// The real (unrendered) content of this row for copying to the clipboard.
    /// Tool groups yield their result; the args variant is reached separately
    /// through [`ToolCallGroupCell::copy_args`].
    pub(crate) fn copy_text(&self) -> Option<String> {
        match &self.kind {
            TranscriptItemKind::User { message } => Some(message.clone()),
            TranscriptItemKind::Assistant { lines, raw, .. }
            | TranscriptItemKind::Reasoning { lines, raw, .. } => {
                if raw.is_empty() {
                    Some(flatten_lines(lines))
                } else {
                    Some(raw.clone())
                }
            }
            TranscriptItemKind::Plain { lines, .. } => Some(flatten_lines(lines)),
            TranscriptItemKind::Patch { file_change } => {
                let path = file_change.path.display();
                Some(match &file_change.change {
                    FileChange::Add { content } => format!("add {path}\n{content}"),
                    FileChange::Delete { content } => format!("delete {path}\n{content}"),
                    FileChange::Update {
                        unified_diff,
                        move_path,
                    } => match move_path {
                        Some(move_path) => {
                            format!("rename to {}\n{unified_diff}", move_path.display())
                        }
                        None => unified_diff.clone(),
                    },
                    FileChange::Omitted {
                        operation,
                        reason,
                        added,
                        removed,
                        bytes,
                    } => format!(
                        "{operation:?} {path}: omitted ({reason}, +{added}/-{removed}, {bytes} bytes)"
                    ),
                })
            }
            TranscriptItemKind::TodoList { todos } => Some(
                todos
                    .iter()
                    .map(|todo| {
                        let marker = match todo.status {
                            TodoStatus::Pending => "- [ ] ",
                            TodoStatus::InProgress => "- [~] ",
                            TodoStatus::Completed => "- [x] ",
                        };
                        format!("{marker}{}", todo.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            TranscriptItemKind::ToolCallGroup(cell) => cell.copy_result(),
        }
    }

    pub(crate) fn replace_with_patch(&mut self, file_change: FileChangeOutput) {
        self.kind = TranscriptItemKind::Patch { file_change };
        self.version = self.version.saturating_add(1);
    }

    pub(crate) fn replace_with_todos(&mut self, todos: Vec<TodoItem>) {
        self.kind = TranscriptItemKind::TodoList { todos };
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
        raw: String,
    },
    Reasoning {
        lines: Vec<Line<'static>>,
        is_first_line: bool,
        raw: String,
    },
    Plain {
        lines: Vec<Line<'static>>,
        hang_indent: usize,
    },
    Patch {
        file_change: FileChangeOutput,
    },
    TodoList {
        todos: Vec<TodoItem>,
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
    output: Option<String>,
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
                output: None,
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
            output: None,
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

    pub(crate) fn set_entry_output(&mut self, entry_idx: usize, output: String) {
        if let Some(entry) = self.entries.get_mut(entry_idx) {
            entry.output = Some(output);
        }
    }

    /// The real tool results for copying: each finished entry contributes its
    /// stored output (or its error), running entries are skipped. `None` when
    /// no entry has anything to report yet.
    pub(crate) fn copy_result(&self) -> Option<String> {
        let parts: Vec<String> = self
            .entries
            .iter()
            .filter_map(|entry| match (&entry.output, &entry.error) {
                (Some(output), _) => Some(output.clone()),
                (None, Some(error)) => Some(format!("error: {error}")),
                (None, None) => None,
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// The real tool parameters for copying: the full JSON args of every entry.
    pub(crate) fn copy_args(&self) -> String {
        self.entries
            .iter()
            .map(|entry| entry.args_preview.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
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

/// Flatten rendered lines to plain text: spans concatenated per line, lines
/// joined with newlines.
fn flatten_lines(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a `todo_write` checklist snapshot: one glyph-prefixed row per todo,
/// continuation rows hang-indented under the content.
fn render_todo_list(todos: &[TodoItem], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(todos.len());
    for todo in todos {
        let (glyph, content_style) = match todo.status {
            TodoStatus::Pending => (
                Span::styled("☐ ", Style::default().dim()),
                Style::default().dim(),
            ),
            TodoStatus::InProgress => (
                Span::styled("◐ ", Style::default().fg(Color::Cyan).bold()),
                Style::default().bold(),
            ),
            TodoStatus::Completed => (
                Span::styled("☑ ", Style::default().fg(Color::Green)),
                Style::default().dim().crossed_out(),
            ),
        };
        let line = Line::from(vec![
            glyph,
            Span::styled(todo.content.clone(), content_style),
        ]);
        lines.extend(wrap::wrap_line_hanging(line, width, 2));
    }
    lines
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

/// User messages are bracketed by a blue left gutter bar on every wrapped row,
/// setting them apart from the assistant's single `•` glyph. The body is wrapped
/// to the reduced width *first* so the gutter column is reserved on continuation
/// rows too (unlike a prefix-then-wrap approach, which only marks the first row).
fn render_user_message(message: &str, width: u16) -> Vec<Line<'static>> {
    const GUTTER: &str = "▌ ";
    let gutter_width = wrap::display_width(GUTTER);
    let body_width = usize::from(width.max(1))
        .saturating_sub(gutter_width)
        .max(1);

    let body = if message.is_empty() {
        vec![Line::default()]
    } else {
        message
            .split('\n')
            .map(|line| Line::from(line.to_owned()))
            .collect::<Vec<_>>()
    };
    let wrapped = wrap::wrap_lines(body, body_width);

    let gutter = Span::styled(GUTTER, Style::default().fg(Color::Blue).bold());
    let mut lines = vec![user_separator(width)];
    lines.extend(prefix_lines(wrapped, gutter.clone(), gutter));
    lines.push(user_separator(width));
    lines
}

/// Full-width muted rule used to bracket a user message above and below,
/// setting it apart from the surrounding assistant output.
fn user_separator(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(usize::from(width.max(1))),
        Style::default().fg(Color::DarkGray),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::CODE_COLOR;
    use smooth_protocol::FileChangeOperation;

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

    #[test]
    fn todo_list_renders_status_glyphs_and_wraps() {
        let item = TranscriptItem::todo_list(
            1,
            vec![
                TodoItem {
                    content: "a finished step".to_string(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "the step currently being worked on right now".to_string(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "a future step".to_string(),
                    status: TodoStatus::Pending,
                },
            ],
        );
        let lines = item.display_lines(24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(texts[0].starts_with("☑ "));
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT)
        );

        assert!(texts[1].starts_with("◐ "));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Cyan));

        // The long in-progress item wraps with a hanging indent.
        assert!(texts.len() > 3, "expected wrapping: {texts:?}");
        assert!(
            texts[2].starts_with("  "),
            "continuation not hung: {:?}",
            texts[2]
        );

        assert!(texts.last().is_some_and(|t| t.starts_with("☐ ")));
        within_width(&lines, 24);
    }

    #[test]
    fn copy_text_returns_user_message_verbatim() {
        let item = TranscriptItem::user(1, "hello\nworld".to_string());
        assert_eq!(item.copy_text().as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn copy_text_returns_assistant_raw_markdown() {
        let raw = "# Title\n\n```rust\nlet x = 1;\n```";
        let item = TranscriptItem::assistant(1, vec![Line::from("Title")], true, raw.to_string());
        assert_eq!(item.copy_text().as_deref(), Some(raw));
    }

    #[test]
    fn copy_text_falls_back_to_flattened_lines_when_raw_empty() {
        let item = TranscriptItem::assistant(
            1,
            vec![
                Line::from(vec![Span::raw("first "), Span::raw("line")]),
                Line::from("second"),
            ],
            true,
            String::new(),
        );
        assert_eq!(item.copy_text().as_deref(), Some("first line\nsecond"));
    }

    #[test]
    fn copy_text_flattens_plain_rows() {
        let item = TranscriptItem::info(1, "something happened");
        assert_eq!(item.copy_text().as_deref(), Some("i something happened"));
    }

    #[test]
    fn copy_text_renders_todo_checkboxes() {
        let item = TranscriptItem::todo_list(
            1,
            vec![
                TodoItem {
                    content: "done".to_string(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "doing".to_string(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "later".to_string(),
                    status: TodoStatus::Pending,
                },
            ],
        );
        assert_eq!(
            item.copy_text().as_deref(),
            Some("- [x] done\n- [~] doing\n- [ ] later")
        );
    }

    #[test]
    fn copy_text_returns_patch_content_with_header() {
        let item = TranscriptItem::patch(
            1,
            FileChangeOutput {
                path: "src/new.rs".into(),
                change: FileChange::Add {
                    content: "fn main() {}".to_string(),
                },
            },
        );
        assert_eq!(
            item.copy_text().as_deref(),
            Some("add src/new.rs\nfn main() {}")
        );

        let item = TranscriptItem::patch(
            1,
            FileChangeOutput {
                path: "src/lib.rs".into(),
                change: FileChange::Update {
                    unified_diff: "@@ -1 +1 @@\n-a\n+b".to_string(),
                    move_path: None,
                },
            },
        );
        assert_eq!(item.copy_text().as_deref(), Some("@@ -1 +1 @@\n-a\n+b"));

        let item = TranscriptItem::patch(
            1,
            FileChangeOutput {
                path: "big.bin".into(),
                change: FileChange::Omitted {
                    operation: FileChangeOperation::Update,
                    reason: "too large".to_string(),
                    added: 10,
                    removed: 2,
                    bytes: 4096,
                },
            },
        );
        assert_eq!(
            item.copy_text().as_deref(),
            Some("Update big.bin: omitted (too large, +10/-2, 4096 bytes)")
        );
    }

    #[test]
    fn tool_copy_result_joins_outputs_and_errors() {
        let mut cell = ToolCallGroupCell::new("run".to_string(), "{\"a\":1}".to_string());
        cell.set_entry_output(0, "first output".to_string());
        cell.set_entry_outcome(0, ToolCallState::Success, None);
        let second = cell.push_entry("{\"b\":2}".to_string());
        cell.set_entry_outcome(second, ToolCallState::Failure, Some("boom".to_string()));

        assert_eq!(
            cell.copy_result().as_deref(),
            Some("first output\n\nerror: boom")
        );
    }

    #[test]
    fn tool_copy_result_is_none_while_running() {
        let cell = ToolCallGroupCell::new("run".to_string(), "{}".to_string());
        assert_eq!(cell.copy_result(), None);
    }

    #[test]
    fn tool_copy_args_joins_full_json_args() {
        let mut cell = ToolCallGroupCell::new("run".to_string(), "{\"a\":1}".to_string());
        cell.push_entry("{\"b\":2}".to_string());
        assert_eq!(cell.copy_args(), "{\"a\":1}\n\n{\"b\":2}");
    }

    #[test]
    fn user_message_is_framed_by_gutter_and_separators() {
        let item = TranscriptItem::user(
            1,
            "this is a fairly long user message that should wrap across several rows".to_string(),
        );
        let lines = item.display_lines(20);

        assert!(lines.len() > 3, "expected top/bottom rules + wrapped body");

        // The first and last rows are full-width muted rules bracketing the
        // message above and below.
        for sep in [&lines[0], &lines[lines.len() - 1]] {
            let sep_text = line_text(sep);
            assert!(
                !sep_text.is_empty() && sep_text.chars().all(|c| c == '─'),
                "expected a rule, got: {sep_text:?}"
            );
            assert_eq!(sep.spans[0].style.fg, Some(Color::DarkGray));
        }

        // Every body row between the rules carries the blue gutter, including
        // wrapped continuations.
        for line in &lines[1..lines.len() - 1] {
            let gutter = &line.spans[0];
            assert_eq!(gutter.content.as_ref(), "▌ ");
            assert_eq!(gutter.style.fg, Some(Color::Blue));
        }
        within_width(&lines, 20);
    }
}
