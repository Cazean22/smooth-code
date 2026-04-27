use ratatui::text::Line;

pub(crate) fn append_markdown(
    markdown_source: &str,
    width: Option<usize>,
    lines: &mut Vec<Line<'static>>,
) {
    let rendered = crate::markdown_render::render_markdown_text_with_width(markdown_source, width);
    lines.extend(rendered.lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn append_markdown_keeps_plain_text_on_one_line() {
        let mut lines = Vec::new();
        append_markdown("hello world\n", None, &mut lines);
        assert_eq!(lines_to_strings(&lines), vec!["hello world".to_string()]);
    }
}
