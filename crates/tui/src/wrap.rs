use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width: usize = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width > 0 && current_width.saturating_add(ch_width) > width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(ch_width);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub(crate) fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let line_style = line.style;

    // Flatten to per-character (char, style) pairs so word boundaries can be
    // found across span boundaries while each character keeps its own style.
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = span.style;
            span.content.chars().map(move |ch| (ch, style))
        })
        .collect();
    if chars.is_empty() {
        return vec![Line::default().style(line_style)];
    }

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_width = 0usize;
    let mut word: Vec<(char, Style)> = Vec::new();
    let mut word_width = 0usize;

    for (ch, style) in chars {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if ch == ' ' {
            // A space ends the pending word; commit it before placing the space.
            place_word(
                &mut rows,
                &mut current,
                &mut current_width,
                &mut word,
                &mut word_width,
                width,
            );
            // Dropping a leading space on a continuation row keeps wrapped text
            // flush-left; a leading space on the first row is genuine indent.
            let dropping_leading = current_width == 0 && !rows.is_empty();
            if current_width.saturating_add(ch_width) > width {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            } else if !dropping_leading {
                current.push((ch, style));
                current_width += ch_width;
            }
        } else {
            word.push((ch, style));
            word_width += ch_width;
        }
    }
    place_word(
        &mut rows,
        &mut current,
        &mut current_width,
        &mut word,
        &mut word_width,
        width,
    );
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }

    rows.into_iter()
        .map(|row| build_line(row, line_style))
        .collect()
}

/// Place the accumulated `word` onto the current row, wrapping to a fresh row
/// when it does not fit. A word wider than a full line is hard-broken so it
/// still renders (e.g. a long URL), but a word that fits on its own line is
/// never split mid-word.
fn place_word(
    rows: &mut Vec<Vec<(char, Style)>>,
    current: &mut Vec<(char, Style)>,
    current_width: &mut usize,
    word: &mut Vec<(char, Style)>,
    word_width: &mut usize,
    width: usize,
) {
    if word.is_empty() {
        return;
    }
    let word = std::mem::take(word);
    let ww = std::mem::replace(word_width, 0);

    if *current_width > 0 && *current_width + ww > width && ww <= width {
        rows.push(std::mem::take(current));
        *current_width = 0;
    }

    if ww <= width {
        current.extend(word);
        *current_width += ww;
        return;
    }

    for (ch, style) in word {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *current_width > 0 && *current_width + ch_width > width {
            rows.push(std::mem::take(current));
            *current_width = 0;
        }
        current.push((ch, style));
        *current_width += ch_width;
    }
}

fn build_line(row: Vec<(char, Style)>, line_style: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (ch, style) in row {
        if let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push(ch);
        } else {
            spans.push(Span::styled(ch.to_string(), style));
        }
    }
    Line::from(spans).style(line_style)
}

pub(crate) fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// Character/column-faithful wrapping: breaks at the exact column rather than
/// at word boundaries, and never drops leading whitespace. Used for code blocks
/// and tool rows where column alignment and indentation must be preserved.
pub(crate) fn wrap_line_char(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let line_style = line.style;
    let mut out: Vec<Line<'static>> = vec![Line::default()];
    let mut current_width: usize = 0;

    for span in line.spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width > 0 && current_width.saturating_add(ch_width) > width {
                out.push(Line::default());
                current_width = 0;
            }
            if let Some(current) = out.last_mut() {
                if let Some(last) = current.spans.last_mut()
                    && last.style == style
                {
                    last.content.to_mut().push(ch);
                } else {
                    current.spans.push(Span::styled(ch.to_string(), style));
                }
            }
            current_width = current_width.saturating_add(ch_width);
        }
    }

    out.into_iter().map(|line| line.style(line_style)).collect()
}

pub(crate) fn wrap_lines_char(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| wrap_line_char(line, width))
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::{
        style::{Color, Style},
        text::{Line, Span},
    };

    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn cjk_width_is_counted_as_two_columns() {
        let wrapped = wrap_text("你好ab", 4);
        assert_eq!(wrapped, vec!["你好".to_string(), "ab".to_string()]);
        assert_eq!(display_width(&wrapped[0]), 4);
    }

    #[test]
    fn combining_mark_stays_with_base_width() {
        let wrapped = wrap_text("e\u{301}xy", 2);
        assert_eq!(display_width(&wrapped[0]), 2);
        assert_eq!(wrapped[0], "e\u{301}x");
    }

    #[test]
    fn wrap_line_preserves_span_styles() {
        let line = Line::from(vec![
            Span::styled("你好", Style::default().fg(Color::Green)),
            Span::styled("ab", Style::default().fg(Color::Red)),
        ]);

        let wrapped = wrap_line(line, 4);

        assert_eq!(wrapped.len(), 2);
        assert_eq!(line_text(&wrapped[0]), "你好");
        assert_eq!(line_text(&wrapped[1]), "ab");
        assert_eq!(wrapped[0].spans.len(), 1);
        assert_eq!(wrapped[1].spans.len(), 1);
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(wrapped[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn wrap_line_breaks_on_word_boundary_not_mid_word() {
        let wrapped = wrap_line(Line::from("I really like it"), 8);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        // "like" must stay intact rather than splitting into "lik" / "e".
        assert_eq!(texts, vec!["I really".to_string(), "like it".to_string()]);
        for line in &wrapped {
            assert!(display_width(&line_text(line)) <= 8);
        }
    }

    #[test]
    fn wrap_line_hard_breaks_word_longer_than_width() {
        let wrapped = wrap_line(Line::from("supercalifragilistic"), 5);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        // A single word with no break opportunity is still split to fit.
        assert_eq!(
            texts,
            vec![
                "super".to_string(),
                "calif".to_string(),
                "ragil".to_string(),
                "istic".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_line_keeps_leading_indent_on_first_row_only() {
        let wrapped = wrap_line(Line::from("  hello world"), 9);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        // Leading indent stays on the first row; the wrapped row is flush-left.
        assert_eq!(texts[0], "  hello ");
        assert_eq!(texts[1], "world");
    }

    #[test]
    fn wrap_line_char_breaks_at_column_not_word_boundary() {
        // Character wrapping is column-faithful: it splits within a token at the
        // exact width, unlike the word-aware wrapper which would break at spaces.
        let texts: Vec<String> = wrap_line_char(Line::from("ab cd ef"), 4)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(texts, vec!["ab c".to_string(), "d ef".to_string()]);

        let word: Vec<String> = wrap_line(Line::from("ab cd ef"), 4)
            .iter()
            .map(line_text)
            .collect();
        assert_ne!(texts, word);
    }
}
