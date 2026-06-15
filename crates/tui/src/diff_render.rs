use cazean_protocol::{FileChange, FileChangeOperation, FileChangeOutput};
use diffy::{Hunk, Patch};
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use crate::config_state;
use crate::highlight::{exceeds_highlight_limits, highlight_code_to_styled_spans};
use crate::wrap;

fn add_line_bg() -> Color {
    config_state::to_color(config_state::current().tui.colors.diff_add_bg)
}

fn delete_line_bg() -> Color {
    config_state::to_color(config_state::current().tui.colors.diff_delete_bg)
}

fn max_rendered_diff_lines() -> usize {
    config_state::current().tui.max_rendered_diff_lines
}

pub(crate) fn create_diff_summary(change: &FileChangeOutput, width: u16) -> Vec<Line<'static>> {
    let (added, removed) = line_counts(&change.change);
    let verb = match &change.change {
        FileChange::Add { .. } => "Added",
        FileChange::Delete { .. } => "Deleted",
        FileChange::Update {
            move_path: Some(_), ..
        } => "Moved",
        FileChange::Update { .. } => "Edited",
        FileChange::Omitted { operation, .. } => operation_verb(operation),
    };
    let path_label = file_change_path_label(change);

    let wrap_width = usize::from(width.max(1));
    let mut lines = wrap::wrap_line_char_hanging(
        Line::from(vec![
            "• ".dim(),
            Span::raw(format!("{verb} 1 file ")),
            "(".into(),
            format!("+{added}").green(),
            " ".into(),
            format!("-{removed}").red(),
            ")".into(),
        ]),
        wrap_width,
        2,
    );
    lines.extend(wrap::wrap_line_char_hanging(
        Line::from(vec![
            "  ".into(),
            Span::raw(path_label),
            " ".into(),
            "(".into(),
            format!("+{added}").green(),
            " ".into(),
            format!("-{removed}").red(),
            ")".into(),
        ]),
        wrap_width,
        2,
    ));

    let diff_width = usize::from(width.saturating_sub(4).max(1));
    let rendered_change = render_change(change, diff_width);
    let rendered_len = rendered_change.len();
    let max_rendered = max_rendered_diff_lines();
    for line in rendered_change.into_iter().take(max_rendered) {
        let mut spans = vec![Span::raw("    ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans).style(line.style));
    }
    if rendered_len > max_rendered {
        lines.push(Line::from(vec![
            Span::raw("    "),
            format!("⋮ diff truncated after {max_rendered} rendered lines").dim(),
        ]));
    }
    lines
}

pub(crate) fn file_change_path_label(change: &FileChangeOutput) -> String {
    match &change.change {
        FileChange::Update {
            move_path: Some(move_path),
            ..
        } => format!("{} -> {}", change.path.display(), move_path.display()),
        _ => change.path.display().to_string(),
    }
}

fn operation_verb(operation: &FileChangeOperation) -> &'static str {
    match operation {
        FileChangeOperation::Add => "Added",
        FileChangeOperation::Delete => "Deleted",
        FileChangeOperation::Update => "Edited",
    }
}

fn line_counts(change: &FileChange) -> (usize, usize) {
    match change {
        FileChange::Add { content } => (content.lines().count(), 0),
        FileChange::Delete { content } => (0, content.lines().count()),
        FileChange::Omitted { added, removed, .. } => (*added, *removed),
        FileChange::Update { unified_diff, .. } => Patch::from_str(unified_diff)
            .map(|patch| {
                patch
                    .hunks()
                    .iter()
                    .flat_map(Hunk::lines)
                    .fold((0, 0), |(added, removed), line| match line {
                        diffy::Line::Insert(_) => (added + 1, removed),
                        diffy::Line::Delete(_) => (added, removed + 1),
                        diffy::Line::Context(_) => (added, removed),
                    })
            })
            .unwrap_or((0, 0)),
    }
}

fn render_change(change: &FileChangeOutput, width: usize) -> Vec<Line<'static>> {
    let lang = detect_lang_for_change(change);
    match &change.change {
        FileChange::Add { content } => {
            render_whole_file(content, DiffLineKind::Insert, width, lang.as_deref())
        }
        FileChange::Delete { content } => {
            render_whole_file(content, DiffLineKind::Delete, width, lang.as_deref())
        }
        FileChange::Update { unified_diff, .. } => {
            render_unified_diff(unified_diff, width, lang.as_deref())
        }
        FileChange::Omitted { reason, bytes, .. } => wrap::wrap_line_char(
            Line::from(vec![
                "⋮ ".dim(),
                format!("diff omitted ({bytes} bytes): {reason}").dim(),
            ]),
            width,
        ),
    }
}

fn detect_lang_for_change(change: &FileChangeOutput) -> Option<String> {
    let path = match &change.change {
        FileChange::Update {
            move_path: Some(move_path),
            ..
        } => move_path,
        _ => &change.path,
    };
    path.extension()?.to_str().map(ToOwned::to_owned)
}

fn render_whole_file(
    content: &str,
    kind: DiffLineKind,
    width: usize,
    lang: Option<&str>,
) -> Vec<Line<'static>> {
    let line_number_width = line_number_width(content.lines().count());
    let syntax_lines = lang.and_then(|lang| highlight_code_to_styled_spans(content, lang));
    content
        .lines()
        .enumerate()
        .flat_map(|(idx, line)| {
            render_diff_line(
                idx + 1,
                kind,
                line,
                width,
                line_number_width,
                syntax_lines
                    .as_ref()
                    .and_then(|lines| lines.get(idx).map(Vec::as_slice)),
            )
        })
        .collect()
}

fn render_unified_diff(unified_diff: &str, width: usize, lang: Option<&str>) -> Vec<Line<'static>> {
    let Ok(patch) = Patch::from_str(unified_diff) else {
        return unified_diff
            .lines()
            .flat_map(|line| wrap::wrap_line_char(Line::from(line.to_string()), width))
            .collect();
    };

    let max_line_number = patch
        .hunks()
        .iter()
        .flat_map(|hunk| [hunk.old_range().end(), hunk.new_range().end()])
        .max()
        .unwrap_or(1);
    let line_number_width = line_number_width(max_line_number);
    let mut lines = Vec::new();
    let (total_diff_bytes, total_diff_lines) = patch.hunks().iter().flat_map(Hunk::lines).fold(
        (0usize, 0usize),
        |(bytes, lines), line| {
            let text = match line {
                diffy::Line::Insert(text)
                | diffy::Line::Delete(text)
                | diffy::Line::Context(text) => text,
            };
            (bytes + text.len(), lines + 1)
        },
    );
    let diff_lang = if exceeds_highlight_limits(total_diff_bytes, total_diff_lines) {
        None
    } else {
        lang
    };

    for (hunk_idx, hunk) in patch.hunks().iter().enumerate() {
        if hunk_idx > 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:>line_number_width$}  ", ""),
                    Style::default().dim(),
                ),
                "⋮".dim(),
            ]));
        }

        // Highlight each displayed hunk as one synthetic source block, matching
        // Codex's strategy. This preserves parser state across multiline
        // strings/comments within the hunk; the diff gutter/background remains
        // the authoritative add/delete cue when old/new sides affect each other.
        let hunk_syntax_lines = diff_lang.and_then(|lang| {
            let hunk_text: String = hunk
                .lines()
                .iter()
                .map(|line| match line {
                    diffy::Line::Insert(text)
                    | diffy::Line::Delete(text)
                    | diffy::Line::Context(text) => *text,
                })
                .collect();
            let syntax_lines = highlight_code_to_styled_spans(&hunk_text, lang)?;
            (syntax_lines.len() == hunk.lines().len()).then_some(syntax_lines)
        });

        let mut old_line = hunk.old_range().start();
        let mut new_line = hunk.new_range().start();
        for (line_idx, line) in hunk.lines().iter().enumerate() {
            let syntax_spans = hunk_syntax_lines
                .as_ref()
                .and_then(|syntax_lines| syntax_lines.get(line_idx).map(Vec::as_slice));
            match line {
                diffy::Line::Insert(text) => {
                    lines.extend(render_diff_line(
                        new_line,
                        DiffLineKind::Insert,
                        text.trim_end_matches('\n'),
                        width,
                        line_number_width,
                        syntax_spans,
                    ));
                    new_line += 1;
                }
                diffy::Line::Delete(text) => {
                    lines.extend(render_diff_line(
                        old_line,
                        DiffLineKind::Delete,
                        text.trim_end_matches('\n'),
                        width,
                        line_number_width,
                        syntax_spans,
                    ));
                    old_line += 1;
                }
                diffy::Line::Context(text) => {
                    lines.extend(render_diff_line(
                        new_line,
                        DiffLineKind::Context,
                        text.trim_end_matches('\n'),
                        width,
                        line_number_width,
                        syntax_spans,
                    ));
                    old_line += 1;
                    new_line += 1;
                }
            }
        }
    }
    lines
}

#[derive(Debug, Clone, Copy)]
enum DiffLineKind {
    Insert,
    Delete,
    Context,
}

fn render_diff_line(
    line_number: usize,
    kind: DiffLineKind,
    text: &str,
    width: usize,
    line_number_width: usize,
    syntax_spans: Option<&[Span<'static>]>,
) -> Vec<Line<'static>> {
    let gutter_width = line_number_width + 3;
    let available = width.saturating_sub(gutter_width).max(1);
    let (sign, style) = match kind {
        DiffLineKind::Insert => ('+', Style::default().fg(Color::Green)),
        DiffLineKind::Delete => ('-', Style::default().fg(Color::Red)),
        DiffLineKind::Context => (' ', Style::default().dim()),
    };
    let content_spans = syntax_spans
        .map(|spans| syntax_diff_spans(spans, kind))
        .unwrap_or_else(|| vec![Span::styled(text.to_string(), style)]);
    let chunks = wrap_styled_spans(&content_spans, available);
    let mut out = Vec::with_capacity(chunks.len());

    for (idx, chunk) in chunks.into_iter().enumerate() {
        let line_number_text = if idx == 0 {
            format!("{line_number:>line_number_width$} ")
        } else {
            format!("{:>line_number_width$} ", "")
        };
        let mut spans = vec![
            Span::styled(line_number_text, Style::default().dim()),
            Span::styled(format!("{sign} "), style),
        ];
        spans.extend(chunk);
        out.push(Line::from(spans).style(diff_line_style(kind)));
    }
    out
}

fn diff_line_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Insert => Style::default().bg(add_line_bg()),
        DiffLineKind::Delete => Style::default().bg(delete_line_bg()),
        DiffLineKind::Context => Style::default().dim(),
    }
}

fn syntax_diff_spans(spans: &[Span<'static>], kind: DiffLineKind) -> Vec<Span<'static>> {
    spans
        .iter()
        .map(|span| {
            let style = if matches!(kind, DiffLineKind::Delete | DiffLineKind::Context) {
                span.style.add_modifier(Modifier::DIM)
            } else {
                span.style
            };
            Span::styled(span.content.clone().into_owned(), style)
        })
        .collect()
}

fn wrap_styled_spans(spans: &[Span<'static>], max_cols: usize) -> Vec<Vec<Span<'static>>> {
    let max_cols = max_cols.max(1);
    let mut rows = Vec::new();
    let mut current_row: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_width = if ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            if current_width > 0 && current_width.saturating_add(ch_width) > max_cols {
                rows.push(std::mem::take(&mut current_row));
                current_width = 0;
            }
            if let Some(last) = current_row.last_mut()
                && last.style == style
            {
                last.content.to_mut().push(ch);
            } else {
                current_row.push(Span::styled(ch.to_string(), style));
            }
            current_width = current_width.saturating_add(ch_width);
        }
    }

    if !current_row.is_empty() || rows.is_empty() {
        rows.push(current_row);
    }
    rows
}

fn line_number_width(max_line_number: usize) -> usize {
    max_line_number.max(1).to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_texts(lines: Vec<Line<'static>>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| line.spans.into_iter().map(|span| span.content).collect())
            .collect()
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn renders_added_file_summary_and_lines() {
        let output = FileChangeOutput {
            path: "src/new.rs".into(),
            change: FileChange::Add {
                content: "fn main() {}\n".to_string(),
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 80));

        assert_eq!(rendered[0], "• Added 1 file (+1 -0)");
        assert_eq!(rendered[1], "  src/new.rs (+1 -0)");
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("1 + fn main() {}"))
        );
    }

    #[test]
    fn syntax_highlights_added_rust_file() {
        let output = FileChangeOutput {
            path: "src/new.rs".into(),
            change: FileChange::Add {
                content: "fn main() {}\n".to_string(),
            },
        };

        let rendered = create_diff_summary(&output, 80);
        let fn_span = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "fn");
        let Some(fn_span) = fn_span else {
            panic!("expected a highlighted fn span in rendered diff");
        };

        assert!(fn_span.style.fg.is_some() || !fn_span.style.add_modifier.is_empty());
        assert!(
            rendered
                .iter()
                .any(|line| line_text(line).contains("1 + fn main() {}")
                    && line.style.bg == Some(add_line_bg()))
        );
    }

    #[test]
    fn renders_update_summary_and_diff_lines() {
        let diff = diffy::create_patch("old\n", "new\n").to_string();
        let output = FileChangeOutput {
            path: "src/lib.rs".into(),
            change: FileChange::Update {
                unified_diff: diff,
                move_path: None,
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 80));

        assert_eq!(rendered[0], "• Edited 1 file (+1 -1)");
        assert!(rendered.iter().any(|line| line.contains("1 - old")));
        assert!(rendered.iter().any(|line| line.contains("1 + new")));
    }

    #[test]
    fn syntax_highlights_update_using_move_destination_extension() {
        let diff = diffy::create_patch("plain\n", "fn main() {}\n").to_string();
        let output = FileChangeOutput {
            path: "scripts/generated.txt".into(),
            change: FileChange::Update {
                unified_diff: diff,
                move_path: Some("src/generated.rs".into()),
            },
        };

        let rendered = create_diff_summary(&output, 80);
        assert!(
            rendered
                .iter()
                .any(|line| line_text(line).contains("1 + fn main() {}"))
        );
        let fn_span = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "fn");
        let Some(fn_span) = fn_span else {
            panic!("expected moved Rust file to use destination extension for highlighting");
        };
        assert!(fn_span.style.fg.is_some() || !fn_span.style.add_modifier.is_empty());
        assert!(
            rendered
                .iter()
                .any(|line| line_text(line).contains("1 + fn main() {}")
                    && line.style.bg == Some(add_line_bg()))
        );
    }

    #[test]
    fn renders_move_summary_and_destination() {
        let output = FileChangeOutput {
            path: "src/old.rs".into(),
            change: FileChange::Update {
                unified_diff: String::new(),
                move_path: Some("src/new.rs".into()),
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 80));

        assert_eq!(rendered[0], "• Moved 1 file (+0 -0)");
        assert_eq!(rendered[1], "  src/old.rs -> src/new.rs (+0 -0)");
    }

    #[test]
    fn renders_omitted_file_change_message() {
        let output = FileChangeOutput {
            path: "large.txt".into(),
            change: FileChange::Omitted {
                operation: FileChangeOperation::Update,
                reason: "too large".to_string(),
                added: 10,
                removed: 0,
                bytes: 600_000,
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 80));

        assert_eq!(rendered[0], "• Edited 1 file (+10 -0)");
        assert!(
            rendered
                .iter()
                .any(|line| { line.contains("diff omitted (600000 bytes): too large") })
        );
    }

    #[test]
    fn renders_omitted_added_file_as_added() {
        let output = FileChangeOutput {
            path: "large.txt".into(),
            change: FileChange::Omitted {
                operation: FileChangeOperation::Add,
                reason: "too large".to_string(),
                added: 10,
                removed: 0,
                bytes: 600_000,
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 80));

        assert_eq!(rendered[0], "• Added 1 file (+10 -0)");
    }

    #[test]
    fn wraps_long_summary_paths_with_hanging_indent() {
        let output = FileChangeOutput {
            path: format!("src/{}{}", "very_long_path_segment_".repeat(4), "file.rs").into(),
            change: FileChange::Add {
                content: "line\n".to_string(),
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 24));

        assert!(
            rendered
                .iter()
                .all(|line| crate::wrap::display_width(line) <= 24),
            "{rendered:?}"
        );
        assert_eq!(rendered[0], "• Added 1 file (+1 -0)");

        // Path rows follow the summary until the four-column diff body begins;
        // every path row keeps the two-column hanging indent.
        let mut path_rows = Vec::new();
        for line in &rendered[1..] {
            if line.starts_with("    ") {
                break;
            }
            path_rows.push(line.clone());
        }
        assert!(path_rows.len() > 1, "path did not wrap: {path_rows:?}");
        for row in &path_rows {
            assert!(row.starts_with("  "), "path row not hung: {row:?}");
        }
    }

    #[test]
    fn wraps_long_summary_line_with_hanging_indent() {
        let output = FileChangeOutput {
            path: "f.rs".into(),
            change: FileChange::Add {
                content: "line\n".to_string(),
            },
        };

        let rendered = line_texts(create_diff_summary(&output, 12));

        assert!(rendered[0].starts_with("• "));
        // The summary wraps at this width; its continuation hangs under "Added…".
        assert!(rendered[1].starts_with("  "), "{rendered:?}");
        assert!(
            rendered
                .iter()
                .all(|line| crate::wrap::display_width(line) <= 12),
            "{rendered:?}"
        );
    }

    #[test]
    fn wraps_long_diff_content_within_width() {
        let old = format!("{}\n", "a".repeat(80));
        let new = format!("{}\n", "b".repeat(80));
        let output = FileChangeOutput {
            path: "src/lib.rs".into(),
            change: FileChange::Update {
                unified_diff: diffy::create_patch(&old, &new).to_string(),
                move_path: None,
            },
        };
        let width = 24;

        let rendered = line_texts(create_diff_summary(&output, width));

        assert!(rendered.iter().any(|line| line.contains("- a")));
        assert!(rendered.iter().any(|line| line.contains("+ b")));
        assert!(
            rendered
                .iter()
                .all(|line| crate::wrap::display_width(line) <= usize::from(width)),
            "line exceeded width: {rendered:?}"
        );
    }

    #[test]
    fn caps_rendered_diff_lines() {
        let content = (0..1_010)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = FileChangeOutput {
            path: "large.txt".into(),
            change: FileChange::Add { content },
        };

        let rendered = line_texts(create_diff_summary(&output, 80));

        assert!(rendered.len() <= 1_003);
        assert!(
            rendered
                .iter()
                .any(|line| { line.contains("diff truncated after 1000 rendered lines") })
        );
    }
}
