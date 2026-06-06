use std::sync::OnceLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Color as SyntectColor, FontStyle, Style as SyntectStyle, Theme},
    parsing::{SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};
use two_face::theme::EmbeddedThemeName;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;
const OPAQUE_ALPHA: u8 = 0xFF;
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        two_face::theme::extra()
            .get(EmbeddedThemeName::CatppuccinMocha)
            .clone()
    })
}

pub(crate) fn exceeds_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    total_bytes > MAX_HIGHLIGHT_BYTES || total_lines > MAX_HIGHLIGHT_LINES
}

#[allow(clippy::disallowed_methods)]
fn ansi_palette_color(index: u8) -> Color {
    match index {
        0x00 => Color::Black,
        0x01 => Color::Red,
        0x02 => Color::Green,
        0x03 => Color::Yellow,
        0x04 => Color::Blue,
        0x05 => Color::Magenta,
        0x06 => Color::Cyan,
        0x07 => Color::Gray,
        n => Color::Indexed(n),
    }
}

#[allow(clippy::disallowed_methods)]
fn convert_syntect_color(color: SyntectColor) -> Option<Color> {
    match color.a {
        ANSI_ALPHA_INDEX => Some(ansi_palette_color(color.r)),
        ANSI_ALPHA_DEFAULT => None,
        OPAQUE_ALPHA => Some(Color::Rgb(color.r, color.g, color.b)),
        _ => Some(Color::Rgb(color.r, color.g, color.b)),
    }
}

fn convert_style(syn_style: SyntectStyle) -> Style {
    let mut style = Style::default();
    if let Some(fg) = convert_syntect_color(syn_style.foreground) {
        style = style.fg(fg);
    }
    if syn_style.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let syntax_set = syntax_set();
    let patched = match lang {
        "csharp" | "c-sharp" => "c#",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        _ => lang,
    };

    syntax_set
        .find_syntax_by_token(patched)
        .or_else(|| syntax_set.find_syntax_by_name(patched))
        .or_else(|| {
            let lower = patched.to_ascii_lowercase();
            syntax_set
                .syntaxes()
                .iter()
                .find(|syntax| syntax.name.to_ascii_lowercase() == lower)
        })
        .or_else(|| syntax_set.find_syntax_by_extension(lang))
}

fn highlight_to_line_spans(code: &str, lang: &str) -> Option<Vec<Vec<Span<'static>>>> {
    if code.is_empty() || exceeds_highlight_limits(code.len(), code.lines().count()) {
        return None;
    }

    let syntax = find_syntax(lang)?;
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut lines = Vec::new();

    for line in LinesWithEndings::from(code) {
        let ranges = highlighter.highlight_line(line, syntax_set()).ok()?;
        let mut spans = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches(['\n', '\r']);
            if !text.is_empty() {
                spans.push(Span::styled(text.to_string(), convert_style(style)));
            }
        }
        if spans.is_empty() {
            spans.push(Span::raw(String::new()));
        }
        lines.push(spans);
    }

    Some(lines)
}

pub(crate) fn highlight_code_to_styled_spans(
    code: &str,
    lang: &str,
) -> Option<Vec<Vec<Span<'static>>>> {
    highlight_to_line_spans(code, lang)
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;
    use ratatui::text::Line;

    use super::*;

    fn reconstructed(lines: &[Line<'static>]) -> String {
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

    #[test]
    fn highlights_rust_keywords() {
        let Some(lines) = highlight_code_to_styled_spans("fn main() {}", "rust") else {
            panic!("expected Rust code to highlight");
        };
        let lines: Vec<Line<'static>> = lines.into_iter().map(Line::from).collect();

        assert_eq!(reconstructed(&lines), "fn main() {}");
        let fn_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "fn");
        let Some(fn_span) = fn_span else {
            panic!("expected a span containing the fn keyword");
        };
        assert!(fn_span.style.fg.is_some() || fn_span.style.add_modifier != Modifier::empty());
    }

    #[test]
    fn unknown_language_returns_plain_fallback_lines() {
        let lines = highlight_code_to_styled_spans("plain text", "unknown-smooth-language");

        assert!(lines.is_none());
    }
}
