use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

#[derive(Default)]
struct ListState {
    next_ordered: Option<u64>,
}

#[allow(dead_code)]
pub(crate) fn render_markdown_text(input: &str) -> Text<'static> {
    render_markdown_text_with_width(input, None)
}

pub(crate) fn render_markdown_text_with_width(input: &str, _width: Option<usize>) -> Text<'static> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(input, options);
    let mut renderer = Renderer::default();
    for event in parser {
        renderer.handle_event(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    current_line: Vec<Span<'static>>,
    inline_styles: Vec<Style>,
    list_stack: Vec<ListState>,
    blockquote_depth: usize,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
    pending_link_href: Option<String>,
}

impl Renderer {
    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_block_buffer.push_str(&text);
                } else {
                    self.push_text(text.as_ref());
                }
            }
            Event::Code(code) => {
                let style = Style::default().fg(Color::Cyan);
                self.current_line
                    .push(Span::styled(code.into_string(), style));
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.in_code_block {
                    self.code_block_buffer.push('\n');
                } else {
                    self.flush_line();
                }
            }
            Event::Rule => {
                self.flush_line();
                self.lines
                    .push(Line::from("───").style(Style::default().dim()));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if self.in_code_block {
                    self.code_block_buffer.push_str(&html);
                } else {
                    self.push_text(html.as_ref());
                }
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.push_text(marker);
            }
            Event::FootnoteReference(reference) => {
                self.push_text(reference.as_ref());
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.current_line.push(Span::styled(
                    text.into_string(),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.apply_style(style_for_heading(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.code_block_lang = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(lang) => Some(lang.into_string()),
                };
            }
            Tag::List(start) => {
                self.list_stack.push(ListState {
                    next_ordered: start,
                });
            }
            Tag::Item => {
                self.flush_line();
                let marker = if let Some(list) = self.list_stack.last_mut() {
                    if let Some(next) = list.next_ordered.as_mut() {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    } else {
                        "- ".to_owned()
                    }
                } else {
                    "- ".to_owned()
                };
                self.push_text(&marker);
            }
            Tag::Emphasis => self.apply_style(Style::default().italic()),
            Tag::Strong => self.apply_style(Style::default().bold()),
            Tag::Strikethrough => {
                self.apply_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { dest_url, .. } => {
                self.pending_link_href = Some(dest_url.into_string());
                self.apply_style(Style::default().fg(Color::Blue).underlined());
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush_line();
                self.lines.push(Line::default());
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                let code_style = Style::default().fg(Color::Cyan);
                if let Some(lang) = self.code_block_lang.take()
                    && !lang.is_empty()
                {
                    self.lines.push(Line::from(vec![
                        Span::styled("```", code_style),
                        Span::styled(lang, code_style),
                    ]));
                }
                for line in self.code_block_buffer.lines() {
                    self.lines.push(Line::from(vec![Span::styled(
                        format!("    {line}"),
                        code_style,
                    )]));
                }
                if self.code_block_buffer.ends_with('\n')
                    && self.code_block_buffer.trim().is_empty()
                {
                    self.lines
                        .push(Line::from(Span::styled("    ", code_style)));
                }
                self.code_block_buffer.clear();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.pop_style();
                self.pending_link_href = None;
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Text<'static> {
        if self.in_code_block {
            self.end_tag(TagEnd::CodeBlock);
        }
        self.flush_line();
        while self
            .lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
        {
            self.lines.pop();
        }
        Text::from(self.lines)
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let base_style = self.current_style();
        let mut lines = text.split('\n').peekable();
        while let Some(segment) = lines.next() {
            if !segment.is_empty() {
                self.current_line
                    .push(Span::styled(segment.to_owned(), base_style));
            }
            if lines.peek().is_some() {
                self.flush_line();
            }
        }
    }

    fn flush_line(&mut self) {
        if self.current_line.is_empty() {
            if !self.list_stack.is_empty() {
                return;
            }
            if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
                self.lines.push(Line::default());
            }
            return;
        }

        let mut spans = Vec::new();
        if self.blockquote_depth > 0 {
            spans.push(Span::styled(
                format!("{} ", "│".repeat(self.blockquote_depth)),
                Style::default().fg(Color::Green).bold(),
            ));
        }
        spans.append(&mut self.current_line);
        self.lines.push(Line::from(spans));
    }

    fn apply_style(&mut self, style: Style) {
        self.inline_styles.push(style);
    }

    fn pop_style(&mut self) {
        self.inline_styles.pop();
    }

    fn current_style(&self) -> Style {
        self.inline_styles
            .iter()
            .fold(Style::default(), |style, next| style.patch(*next))
    }
}

fn style_for_heading(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default().bold().underlined(),
        HeadingLevel::H2 => Style::default().bold(),
        HeadingLevel::H3 => Style::default().bold().italic(),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => Style::default().italic(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_strings(text: &Text<'static>) -> Vec<String> {
        text.lines
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
    fn renders_lists_and_code_blocks() {
        let text = render_markdown_text("- one\n- two\n\n```rust\nfn main() {}\n```\n");
        assert_eq!(
            lines_to_strings(&text),
            vec![
                "- one".to_string(),
                "- two".to_string(),
                "".to_string(),
                "```rust".to_string(),
                "    fn main() {}".to_string(),
            ]
        );
    }
}
