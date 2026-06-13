use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// A terminal color, parsed from a TOML string and stored without any
/// dependency on `ratatui` (the TUI layer converts `ColorSpec` to its own
/// `Color`). The grammar is owned here, so unknown values are a parse error
/// rather than a silent fallback downstream — which makes the TUI-side
/// conversion total.
///
/// Accepted string forms (lowercase):
/// - a named color from the fixed set (e.g. `"cyan"`, `"dark-gray"`)
/// - a palette index: `"22"` or `"indexed:22"` (`0..=255`)
/// - an RGB hex triple: `"#rrggbb"`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpec {
    Named(NamedColor),
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

/// The fixed set of named colors, mirroring the standard 16-color ANSI/ratatui
/// names. Kept here (not in the TUI) so the named grammar is validated at
/// config-load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
}

impl NamedColor {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Blue => "blue",
            Self::Magenta => "magenta",
            Self::Cyan => "cyan",
            Self::Gray => "gray",
            Self::DarkGray => "dark-gray",
            Self::LightRed => "light-red",
            Self::LightGreen => "light-green",
            Self::LightYellow => "light-yellow",
            Self::LightBlue => "light-blue",
            Self::LightMagenta => "light-magenta",
            Self::LightCyan => "light-cyan",
            Self::White => "white",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let named = match value {
            "black" => Self::Black,
            "red" => Self::Red,
            "green" => Self::Green,
            "yellow" => Self::Yellow,
            "blue" => Self::Blue,
            "magenta" => Self::Magenta,
            "cyan" => Self::Cyan,
            "gray" => Self::Gray,
            "dark-gray" => Self::DarkGray,
            "light-red" => Self::LightRed,
            "light-green" => Self::LightGreen,
            "light-yellow" => Self::LightYellow,
            "light-blue" => Self::LightBlue,
            "light-magenta" => Self::LightMagenta,
            "light-cyan" => Self::LightCyan,
            "white" => Self::White,
            _ => return None,
        };
        Some(named)
    }
}

/// Error returned when a color string does not match the grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseColorError(String);

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid color `{}`: expected a named color (e.g. `cyan`, `dark-gray`), \
             a palette index (`0`..`255` or `indexed:N`), or `#rrggbb`",
            self.0
        )
    }
}

impl std::error::Error for ParseColorError {}

impl FromStr for ColorSpec {
    type Err = ParseColorError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = raw.trim();
        let err = || ParseColorError(raw.to_string());

        if let Some(hex) = value.strip_prefix('#') {
            // `#rrggbb` only — six hex digits.
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(err());
            }
            let parse = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16);
            let (Ok(r), Ok(g), Ok(b)) = (parse(0..2), parse(2..4), parse(4..6)) else {
                return Err(err());
            };
            return Ok(Self::Rgb { r, g, b });
        }

        if let Some(index) = value.strip_prefix("indexed:") {
            return index.parse::<u8>().map(Self::Indexed).map_err(|_| err());
        }

        if let Some(named) = NamedColor::parse(value) {
            return Ok(Self::Named(named));
        }

        // A bare decimal is a palette index.
        if value.bytes().all(|b| b.is_ascii_digit()) && !value.is_empty() {
            return value.parse::<u8>().map(Self::Indexed).map_err(|_| err());
        }

        Err(err())
    }
}

impl fmt::Display for ColorSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(named) => f.write_str(named.as_str()),
            Self::Indexed(index) => write!(f, "indexed:{index}"),
            Self::Rgb { r, g, b } => write!(f, "#{r:02x}{g:02x}{b:02x}"),
        }
    }
}

impl<'de> Deserialize<'de> for ColorSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(D::Error::custom)
    }
}

impl Serialize for ColorSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_colors() {
        assert_eq!(
            "cyan".parse::<ColorSpec>(),
            Ok(ColorSpec::Named(NamedColor::Cyan))
        );
        assert_eq!(
            "dark-gray".parse::<ColorSpec>(),
            Ok(ColorSpec::Named(NamedColor::DarkGray))
        );
    }

    #[test]
    fn parses_indexed_colors() {
        assert_eq!("22".parse::<ColorSpec>(), Ok(ColorSpec::Indexed(22)));
        assert_eq!(
            "indexed:52".parse::<ColorSpec>(),
            Ok(ColorSpec::Indexed(52))
        );
        assert_eq!("255".parse::<ColorSpec>(), Ok(ColorSpec::Indexed(255)));
    }

    #[test]
    fn rejects_out_of_range_index() {
        assert!("256".parse::<ColorSpec>().is_err());
        assert!("indexed:300".parse::<ColorSpec>().is_err());
    }

    #[test]
    fn parses_hex_rgb() {
        assert_eq!(
            "#aabbcc".parse::<ColorSpec>(),
            Ok(ColorSpec::Rgb {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc
            })
        );
    }

    #[test]
    fn rejects_short_hex_and_unknown_names() {
        assert!("#abc".parse::<ColorSpec>().is_err());
        assert!("chartreuse".parse::<ColorSpec>().is_err());
        assert!("".parse::<ColorSpec>().is_err());
    }

    #[test]
    fn round_trips_through_display() {
        for raw in ["cyan", "light-magenta", "indexed:22", "#aabbcc"] {
            let Ok(spec) = raw.parse::<ColorSpec>() else {
                panic!("expected `{raw}` to parse");
            };
            assert_eq!(spec.to_string().parse::<ColorSpec>(), Ok(spec));
        }
    }
}
