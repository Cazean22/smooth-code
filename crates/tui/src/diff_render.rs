use diffy::{Hunk, Patch};
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use smooth_protocol::{FileChange, FileChangeOperation, FileChangeOutput};

use crate::wrap;

const MAX_RENDERED_DIFF_LINES: usize = 1_000;

pub(crate) fn create_diff_summary(change: &FileChangeOutput, width: u16) -> Vec<Line<'static>> {
    let (added, removed) = line_counts(&change.change);
    let verb = match &change.change {
        FileChange::Add { .. } => "Added",
        FileChange::Delete { .. } => "Deleted",
        FileChange::Update {
            move_path: Some(_), ..
        } => "Moved",
        FileChange::Update { .. } => "Edited",
        FileChange::Omitted { operation, .. } => operation_verb(&operation),
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
    let rendered_change = render_change(&change.change, diff_width);
    let rendered_len = rendered_change.len();
    for line in rendered_change.into_iter().take(MAX_RENDERED_DIFF_LINES) {
        let mut spans = vec![Span::raw("    ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans).style(line.style));
    }
    if rendered_len > MAX_RENDERED_DIFF_LINES {
        lines.push(Line::from(vec![
            Span::raw("    "),
            format!("⋮ diff truncated after {MAX_RENDERED_DIFF_LINES} rendered lines").dim(),
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

fn render_change(change: &FileChange, width: usize) -> Vec<Line<'static>> {
    match change {
        FileChange::Add { content } => render_whole_file(content, DiffLineKind::Insert, width),
        FileChange::Delete { content } => render_whole_file(content, DiffLineKind::Delete, width),
        FileChange::Update { unified_diff, .. } => render_unified_diff(unified_diff, width),
        FileChange::Omitted { reason, bytes, .. } => wrap::wrap_line_char(
            Line::from(vec![
                "⋮ ".dim(),
                format!("diff omitted ({bytes} bytes): {reason}").dim(),
            ]),
            width,
        ),
    }
}

fn render_whole_file(content: &str, kind: DiffLineKind, width: usize) -> Vec<Line<'static>> {
    let line_number_width = line_number_width(content.lines().count());
    content
        .lines()
        .enumerate()
        .flat_map(|(idx, line)| render_diff_line(idx + 1, kind, line, width, line_number_width))
        .collect()
}

fn render_unified_diff(unified_diff: &str, width: usize) -> Vec<Line<'static>> {
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

        let mut old_line = hunk.old_range().start();
        let mut new_line = hunk.new_range().start();
        for line in hunk.lines() {
            match line {
                diffy::Line::Insert(text) => {
                    lines.extend(render_diff_line(
                        new_line,
                        DiffLineKind::Insert,
                        text.trim_end_matches('\n'),
                        width,
                        line_number_width,
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
) -> Vec<Line<'static>> {
    let gutter_width = line_number_width + 3;
    let available = width.saturating_sub(gutter_width).max(1);
    let chunks = wrap::wrap_text(text, available);
    let mut out = Vec::with_capacity(chunks.len());

    for (idx, chunk) in chunks.into_iter().enumerate() {
        let line_number_text = if idx == 0 {
            format!("{line_number:>line_number_width$} ")
        } else {
            format!("{:>line_number_width$} ", "")
        };
        let (sign, style) = match kind {
            DiffLineKind::Insert => ('+', Style::default().fg(Color::Green)),
            DiffLineKind::Delete => ('-', Style::default().fg(Color::Red)),
            DiffLineKind::Context => (' ', Style::default().dim()),
        };
        out.push(Line::from(vec![
            Span::styled(line_number_text, Style::default().dim()),
            Span::styled(format!("{sign} "), style),
            Span::styled(chunk, style),
        ]));
    }
    out
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
