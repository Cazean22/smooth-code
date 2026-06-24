use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[allow(dead_code)]
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

/// Number of usable content columns for a row: the first row gets the full
/// `width`; continuation rows reserve `indent` columns for the hanging prefix.
fn row_budget(completed_rows: usize, width: usize, indent: usize) -> usize {
    if completed_rows == 0 {
        width
    } else {
        width.saturating_sub(indent).max(1)
    }
}

/// Build wrapped rows into lines, prefixing every continuation row with `indent`
/// spaces so wrapped content hangs under the first row's content.
fn build_hanging_lines(
    rows: Vec<Vec<(char, Style)>>,
    line_style: Style,
    indent: usize,
) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(idx, row)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if idx > 0 && indent > 0 {
                spans.push(Span::raw(" ".repeat(indent)));
            }
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
        })
        .collect()
}

/// Character/column-faithful wrapping with a hanging indent. Like
/// `wrap_line_char`, but continuation rows are prefixed with `indent` spaces so
/// wrapped content (e.g. a long tool-args preview or diff summary) aligns under
/// the first row's content instead of falling back to column 0.
pub(crate) fn wrap_line_char_hanging(
    line: Line<'static>,
    width: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let indent = indent.min(width.saturating_sub(1));
    let line_style = line.style;

    let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut current_width: usize = 0;
    for span in line.spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            let budget = row_budget(rows.len() - 1, width, indent);
            if current_width > 0 && current_width.saturating_add(ch_width) > budget {
                rows.push(Vec::new());
                current_width = 0;
            }
            if let Some(row) = rows.last_mut() {
                row.push((ch, style));
            }
            current_width = current_width.saturating_add(ch_width);
        }
    }

    build_hanging_lines(rows, line_style, indent)
}

/// Word-aware wrapping with a hanging indent: like `wrap_line`, but continuation
/// rows are prefixed with `indent` spaces so wrapped prose (e.g. an option
/// description or an info/error message) aligns under the first row's content.
pub(crate) fn wrap_line_hanging(
    line: Line<'static>,
    width: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let indent = indent.min(width.saturating_sub(1));
    let line_style = line.style;

    let mut state = HangingWrapState::new(width, indent);
    let mut saw_content = false;

    for span in &line.spans {
        let style = span.style;
        let text = span.content.as_ref();
        let mut run_start = 0usize;
        let mut run_width = 0usize;
        let mut run_is_space = None;

        for (idx, ch) in text.char_indices() {
            let is_space = ch == ' ';
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            match run_is_space {
                None => {
                    run_is_space = Some(is_space);
                    run_start = idx;
                    run_width = ch_width;
                }
                Some(current_is_space) if current_is_space == is_space => {
                    run_width += ch_width;
                }
                Some(current_is_space) => {
                    saw_content = true;
                    state.place_span_run(current_is_space, &text[run_start..idx], run_width, style);
                    run_is_space = Some(is_space);
                    run_start = idx;
                    run_width = ch_width;
                }
            }
        }

        if let Some(is_space) = run_is_space {
            saw_content = true;
            state.place_span_run(is_space, &text[run_start..], run_width, style);
        }
    }
    if !saw_content {
        return vec![Line::default().style(line_style)];
    }

    state.finish(line_style)
}

type BorrowedSpanRun<'a> = (&'a str, Style, usize);

struct HangingWrapState<'a> {
    rows: Vec<Vec<Span<'static>>>,
    current: Vec<Span<'static>>,
    current_width: usize,
    pending_word: Vec<BorrowedSpanRun<'a>>,
    pending_word_width: usize,
    width: usize,
    indent: usize,
}

impl<'a> HangingWrapState<'a> {
    fn new(width: usize, indent: usize) -> Self {
        Self {
            rows: Vec::new(),
            current: Vec::new(),
            current_width: 0,
            pending_word: Vec::new(),
            pending_word_width: 0,
            width,
            indent,
        }
    }

    fn place_span_run(&mut self, is_space: bool, text: &'a str, text_width: usize, style: Style) {
        if text.is_empty() {
            return;
        }

        if is_space {
            self.place_pending_word();
            self.place_spaces(text, style);
        } else {
            self.pending_word.push((text, style, text_width));
            self.pending_word_width += text_width;
        }
    }

    fn place_spaces(&mut self, spaces: &str, style: Style) {
        let mut segment_start = None;
        let mut segment_width = 0usize;

        for (idx, ch) in spaces.char_indices() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            let logical_width = self.current_width.saturating_add(segment_width);
            let budget = row_budget(self.rows.len(), self.width, self.indent);
            if logical_width.saturating_add(ch_width) > budget {
                if let Some(start) = segment_start.take() {
                    push_span_text(&mut self.current, &spaces[start..idx], style);
                    self.current_width += segment_width;
                    segment_width = 0;
                }
                self.rows.push(std::mem::take(&mut self.current));
                self.current_width = 0;
                continue;
            }

            let dropping_leading = logical_width == 0 && !self.rows.is_empty();
            if dropping_leading {
                continue;
            }

            if segment_start.is_none() {
                segment_start = Some(idx);
            }
            segment_width += ch_width;
        }

        if let Some(start) = segment_start {
            push_span_text(&mut self.current, &spaces[start..], style);
            self.current_width += segment_width;
        }
    }

    /// Place the accumulated word onto the current row, accounting for the
    /// hanging indent budget of continuation rows. A word wider than a
    /// continuation row is hard-broken so it still renders.
    fn place_pending_word(&mut self) {
        if self.pending_word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.pending_word);
        let word_width = std::mem::replace(&mut self.pending_word_width, 0);

        let cur_budget = row_budget(self.rows.len(), self.width, self.indent);
        let next_budget = row_budget(self.rows.len() + 1, self.width, self.indent);
        // Move a whole word to a fresh row only when it actually fits there. A word
        // too wide even for the next row is hard-broken starting in the current
        // row's remaining space, so a prefix never lands alone on its own row.
        if self.current_width > 0
            && self.current_width + word_width > cur_budget
            && word_width <= next_budget
        {
            self.rows.push(std::mem::take(&mut self.current));
            self.current_width = 0;
        }

        let budget = row_budget(self.rows.len(), self.width, self.indent);
        if self.current_width + word_width <= budget {
            self.push_word_fragments(word);
            self.current_width += word_width;
            return;
        }

        self.hard_break_word_fragments(word);
    }

    fn push_word_fragments(&mut self, word: Vec<BorrowedSpanRun<'a>>) {
        for (text, style, _) in word {
            push_span_text(&mut self.current, text, style);
        }
    }

    fn hard_break_word_fragments(&mut self, word: Vec<BorrowedSpanRun<'a>>) {
        for (text, style, text_width) in word {
            let budget = row_budget(self.rows.len(), self.width, self.indent);
            if self.current_width.saturating_add(text_width) <= budget {
                push_span_text(&mut self.current, text, style);
                self.current_width += text_width;
                continue;
            }

            self.hard_break_text_fragment(text, style);
        }
    }

    fn hard_break_text_fragment(&mut self, text: &str, style: Style) {
        let mut segment_start = 0usize;
        let mut segment_width = 0usize;

        for (idx, ch) in text.char_indices() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            let budget = row_budget(self.rows.len(), self.width, self.indent);
            let used_width = self.current_width.saturating_add(segment_width);
            if used_width > 0 && used_width.saturating_add(ch_width) > budget {
                if segment_start < idx {
                    push_span_text(&mut self.current, &text[segment_start..idx], style);
                    self.current_width += segment_width;
                }
                self.rows.push(std::mem::take(&mut self.current));
                self.current_width = 0;
                segment_start = idx;
                segment_width = 0;
            }
            segment_width += ch_width;
        }

        if segment_start < text.len() {
            push_span_text(&mut self.current, &text[segment_start..], style);
            self.current_width += segment_width;
        }
    }

    fn finish(mut self, line_style: Style) -> Vec<Line<'static>> {
        self.place_pending_word();
        let current = std::mem::take(&mut self.current);
        if !current.is_empty() || self.rows.is_empty() {
            self.rows.push(current);
        }

        build_hanging_span_lines(self.rows, line_style, self.indent)
    }
}

fn build_hanging_span_lines(
    rows: Vec<Vec<Span<'static>>>,
    line_style: Style,
    indent: usize,
) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(idx, row)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if idx > 0 && indent > 0 {
                spans.push(Span::raw(" ".repeat(indent)));
            }
            for span in row {
                push_owned_span(&mut spans, span);
            }
            Line::from(spans).style(line_style)
        })
        .collect()
}

fn push_owned_span(spans: &mut Vec<Span<'static>>, span: Span<'static>) {
    if span.content.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == span.style
    {
        last.content.to_mut().push_str(span.content.as_ref());
    } else {
        spans.push(span);
    }
}

fn push_span_text(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
    } else {
        spans.push(Span::styled(text.to_owned(), style));
    }
}

/// Truncate `text` to at most `max_width` display columns, replacing the cut
/// tail with a single-column ellipsis. Width-aware, so wide characters are never
/// split across the boundary.
pub(crate) fn truncate_display(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
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
    fn wrap_line_hanging_coalesces_adjacent_same_style_output_spans() {
        let green = Style::default().fg(Color::Green);
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![
            Span::styled("alpha", green),
            Span::styled(" ", green),
            Span::styled("beta", green),
            Span::styled("!", red),
        ]);

        let wrapped = wrap_line_hanging(line, 20, 2);

        assert_eq!(wrapped.len(), 1);
        assert_eq!(line_text(&wrapped[0]), "alpha beta!");
        assert_eq!(wrapped[0].spans.len(), 2);
        assert_eq!(wrapped[0].spans[0].content.as_ref(), "alpha beta");
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(wrapped[0].spans[1].content.as_ref(), "!");
        assert_eq!(wrapped[0].spans[1].style.fg, Some(Color::Red));
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

    #[test]
    fn wrap_line_char_hanging_indents_continuation_rows() {
        let wrapped = wrap_line_char_hanging(Line::from("abcdefghij"), 5, 2);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        assert_eq!(
            texts,
            vec!["abcde".to_string(), "  fgh".to_string(), "  ij".to_string()]
        );
        for line in &wrapped {
            assert!(display_width(&line_text(line)) <= 5);
        }
    }

    #[test]
    fn wrap_line_hanging_aligns_continuations_under_content() {
        // indent 2 mimics a "X " glyph prefix; continuations align under the text.
        let wrapped = wrap_line_hanging(Line::from("X aaa bbb ccc ddd"), 7, 2);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        assert!(texts.len() > 1);
        assert!(texts[0].starts_with("X "));
        for cont in &texts[1..] {
            assert!(
                cont.starts_with("  "),
                "continuation not indented: {cont:?}"
            );
        }
        for line in &wrapped {
            assert!(display_width(&line_text(line)) <= 7);
        }
    }

    #[test]
    fn wrap_line_hanging_preserves_styles_and_wide_chars() {
        let line = Line::from(vec![
            Span::styled("你好 ", Style::default().fg(Color::Green)),
            Span::styled("world", Style::default().fg(Color::Red)),
        ]);
        let wrapped = wrap_line_hanging(line, 5, 2);

        for line in &wrapped {
            assert!(display_width(&line_text(line)) <= 5);
        }
        assert!(
            wrapped
                .iter()
                .any(|l| l.spans.iter().any(|s| s.style.fg == Some(Color::Red)))
        );
    }

    #[test]
    fn wrap_line_hanging_with_zero_indent_is_flush_left() {
        let wrapped = wrap_line_hanging(Line::from("alpha beta gamma"), 6, 0);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();
        for cont in &texts[1..] {
            assert!(!cont.starts_with(' '), "unexpected indent: {cont:?}");
        }
    }

    #[test]
    fn wrap_line_hanging_fills_first_row_before_breaking_long_token() {
        // A long unbreakable token after a prefix must start filling the first
        // row, not strand the prefix ("Thread ") alone with the value on row two.
        let wrapped = wrap_line_hanging(Line::from("Thread 01234567890123456789"), 20, 7);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        assert!(texts.len() > 1, "expected wrapping: {texts:?}");
        assert!(
            texts[0].contains('0'),
            "long token did not start on the first row: {texts:?}"
        );
        for line in &wrapped {
            assert!(display_width(&line_text(line)) <= 20);
        }
    }

    #[test]
    fn wrap_line_hanging_keeps_styled_fragments_in_one_word() {
        let green = Style::default().fg(Color::Green);
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![
            Span::raw("X "),
            Span::styled("abc", green),
            Span::styled("def", red),
        ]);

        let wrapped = wrap_line_hanging(line, 7, 2);
        let texts: Vec<String> = wrapped.iter().map(line_text).collect();

        // "abcdef" crosses a span/style boundary. It is still one logical word,
        // so it hard-breaks from the first row instead of moving "def" whole to
        // the continuation row.
        assert_eq!(texts, vec!["X abcde".to_string(), "  f".to_string()]);
        assert!(
            wrapped[0].spans.iter().any(|span| {
                span.content.as_ref() == "abc" && span.style.fg == Some(Color::Green)
            })
        );
        assert!(wrapped.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.as_ref().contains('f') && span.style.fg == Some(Color::Red)
            })
        }));
    }

    #[test]
    fn truncate_display_adds_ellipsis_and_respects_width() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello world", 5), "hell…");
        assert_eq!(display_width(&truncate_display("hello world", 5)), 5);

        // Wide characters are not split across the boundary.
        let wide = truncate_display("你好世界", 5);
        assert!(display_width(&wide) <= 5);
        assert!(wide.ends_with('…'));

        assert_eq!(truncate_display("anything", 0), "");
    }
}
