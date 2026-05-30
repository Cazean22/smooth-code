use diffy::{Hunk, Patch};
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use smooth_protocol::{FileChange, FileChangeOutput};

pub(crate) fn create_diff_summary(change: &FileChangeOutput, width: u16) -> Vec<Line<'static>> {
    let (added, removed) = line_counts(&change.change);
    let verb = match change.change {
        FileChange::Add { .. } => "Added",
        FileChange::Delete { .. } => "Deleted",
        FileChange::Update { .. } => "Edited",
    };

    let mut lines = vec![Line::from(vec![
        "• ".dim(),
        Span::raw(format!("{verb} 1 file ")),
        "(".into(),
        format!("+{added}").green(),
        " ".into(),
        format!("-{removed}").red(),
        ")".into(),
    ])];
    lines.push(Line::from(vec![
        "  ".into(),
        Span::raw(change.path.display().to_string()),
        " ".into(),
        "(".into(),
        format!("+{added}").green(),
        " ".into(),
        format!("-{removed}").red(),
        ")".into(),
    ]));

    let diff_width = usize::from(width.saturating_sub(4).max(20));
    for line in render_change(&change.change, diff_width) {
        let mut spans = vec![Span::raw("    ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans).style(line.style));
    }
    lines
}

fn line_counts(change: &FileChange) -> (usize, usize) {
    match change {
        FileChange::Add { content } => (content.lines().count(), 0),
        FileChange::Delete { content } => (0, content.lines().count()),
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
            .map(|line| Line::from(line.to_string()))
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
    let gutter_width = line_number_width + 2;
    let available = width.saturating_sub(gutter_width).max(1);
    let chunks = wrap_text(text, available);
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

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.chars().count() >= width {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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
}
