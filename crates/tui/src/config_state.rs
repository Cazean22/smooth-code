//! Process-global handle to the resolved [`Config`] for the TUI render layer.
//!
//! Core and tools receive `Arc<Config>` through explicit constructors; the TUI
//! render path is a web of free functions with no such seam, so appearance
//! settings are read from this global instead. It is installed once at startup
//! (before the first render) and is replaceable so tests can install isolated
//! configs. All accessors are poison-safe — render code cannot return errors,
//! so a poisoned lock recovers the inner value rather than panicking.

use std::sync::{Arc, OnceLock, RwLock};

use cazean_config::{ColorSpec, Config, NamedColor};
use ratatui::style::Color;

static RUNTIME_CONFIG: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<Config>> {
    RUNTIME_CONFIG.get_or_init(|| RwLock::new(Arc::new(Config::default())))
}

/// Install (or replace) the runtime config. Called once at startup; tests may
/// call it to install an isolated config.
pub(crate) fn install(config: Arc<Config>) {
    let mut guard = match cell().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = config;
}

/// Clone the current runtime config, defaulting to `Config::default()` if it
/// was never installed. Never panics on a poisoned lock.
pub(crate) fn current() -> Arc<Config> {
    match cell().read() {
        Ok(guard) => Arc::clone(&guard),
        Err(poisoned) => Arc::clone(&poisoned.into_inner()),
    }
}

/// Convert a config [`ColorSpec`] to a ratatui [`Color`]. Total: the config
/// crate validates named colors at load time, so every variant maps cleanly.
#[allow(clippy::disallowed_methods)]
pub(crate) fn to_color(spec: ColorSpec) -> Color {
    match spec {
        ColorSpec::Indexed(index) => Color::Indexed(index),
        ColorSpec::Rgb { r, g, b } => Color::Rgb(r, g, b),
        ColorSpec::Named(named) => match named {
            NamedColor::Black => Color::Black,
            NamedColor::Red => Color::Red,
            NamedColor::Green => Color::Green,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::Gray => Color::Gray,
            NamedColor::DarkGray => Color::DarkGray,
            NamedColor::LightRed => Color::LightRed,
            NamedColor::LightGreen => Color::LightGreen,
            NamedColor::LightYellow => Color::LightYellow,
            NamedColor::LightBlue => Color::LightBlue,
            NamedColor::LightMagenta => Color::LightMagenta,
            NamedColor::LightCyan => Color::LightCyan,
            NamedColor::White => Color::White,
        },
    }
}
