use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

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

use crate::config_state;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
/// Resolved themes cached by name so reinstalling config (and tests installing
/// a different theme) take effect, rather than freezing the first theme.
static THEMES: OnceLock<RwLock<HashMap<String, Arc<Theme>>>> = OnceLock::new();

const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;
const OPAQUE_ALPHA: u8 = 0xFF;
/// Fallback when a configured theme name is unknown (also the built-in default).
const FALLBACK_THEME: EmbeddedThemeName = EmbeddedThemeName::CatppuccinMocha;

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

/// Map a config theme name (a two-face `EmbeddedThemeName` identifier) to the
/// enum variant. Returns `None` for unknown names; the config layer validates
/// names at startup, and `theme()` falls back to [`FALLBACK_THEME`].
pub(crate) fn embedded_theme_name(name: &str) -> Option<EmbeddedThemeName> {
    let variant = match name {
        "Ansi" => EmbeddedThemeName::Ansi,
        "Base16" => EmbeddedThemeName::Base16,
        "Base16EightiesDark" => EmbeddedThemeName::Base16EightiesDark,
        "Base16MochaDark" => EmbeddedThemeName::Base16MochaDark,
        "Base16OceanDark" => EmbeddedThemeName::Base16OceanDark,
        "Base16OceanLight" => EmbeddedThemeName::Base16OceanLight,
        "Base16_256" => EmbeddedThemeName::Base16_256,
        "CatppuccinFrappe" => EmbeddedThemeName::CatppuccinFrappe,
        "CatppuccinLatte" => EmbeddedThemeName::CatppuccinLatte,
        "CatppuccinMacchiato" => EmbeddedThemeName::CatppuccinMacchiato,
        "CatppuccinMocha" => EmbeddedThemeName::CatppuccinMocha,
        "ColdarkCold" => EmbeddedThemeName::ColdarkCold,
        "ColdarkDark" => EmbeddedThemeName::ColdarkDark,
        "DarkNeon" => EmbeddedThemeName::DarkNeon,
        "Dracula" => EmbeddedThemeName::Dracula,
        "Github" => EmbeddedThemeName::Github,
        "GruvboxDark" => EmbeddedThemeName::GruvboxDark,
        "GruvboxLight" => EmbeddedThemeName::GruvboxLight,
        "InspiredGithub" => EmbeddedThemeName::InspiredGithub,
        "Leet" => EmbeddedThemeName::Leet,
        "MonokaiExtended" => EmbeddedThemeName::MonokaiExtended,
        "MonokaiExtendedBright" => EmbeddedThemeName::MonokaiExtendedBright,
        "MonokaiExtendedLight" => EmbeddedThemeName::MonokaiExtendedLight,
        "MonokaiExtendedOrigin" => EmbeddedThemeName::MonokaiExtendedOrigin,
        "Nord" => EmbeddedThemeName::Nord,
        "OneHalfDark" => EmbeddedThemeName::OneHalfDark,
        "OneHalfLight" => EmbeddedThemeName::OneHalfLight,
        "SolarizedDark" => EmbeddedThemeName::SolarizedDark,
        "SolarizedLight" => EmbeddedThemeName::SolarizedLight,
        "SublimeSnazzy" => EmbeddedThemeName::SublimeSnazzy,
        "TwoDark" => EmbeddedThemeName::TwoDark,
        "Zenburn" => EmbeddedThemeName::Zenburn,
        _ => return None,
    };
    Some(variant)
}

fn theme() -> Arc<Theme> {
    let name = config_state::current().tui.highlight_theme.clone();
    let cache = THEMES.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(themes) = cache.read()
        && let Some(theme) = themes.get(&name)
    {
        return Arc::clone(theme);
    }
    let embedded = embedded_theme_name(&name).unwrap_or(FALLBACK_THEME);
    let theme = Arc::new(two_face::theme::extra().get(embedded).clone());
    if let Ok(mut themes) = cache.write() {
        themes.insert(name, Arc::clone(&theme));
    }
    theme
}

pub(crate) fn exceeds_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    let tui = &config_state::current().tui;
    total_bytes > tui.max_highlight_bytes || total_lines > tui.max_highlight_lines
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
    let theme = theme();
    let mut highlighter = HighlightLines::new(syntax, theme.as_ref());
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
        let lines = highlight_code_to_styled_spans("plain text", "unknown-cazean-language");

        assert!(lines.is_none());
    }
}
