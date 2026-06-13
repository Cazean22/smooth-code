use std::path::PathBuf;

/// Errors surfaced while loading and resolving configuration.
///
/// These messages are intended to be fully self-describing: configuration is
/// loaded before telemetry/tracing exists, so a `ConfigError` may be printed
/// straight to stderr with no logger to enrich it.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A config file existed but could not be read from disk.
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A config file could not be parsed as TOML. The `toml` crate's error
    /// already carries line/column and a snippet; the path is prepended so the
    /// message reads like `path: <toml error>`.
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// The merged configuration failed a semantic constraint. Reported after
    /// the layers are merged, so per-layer line/column is no longer available;
    /// the message names the full key path and the offending value(s).
    #[error("invalid config: {message}")]
    Validate { message: String },
}

impl ConfigError {
    pub(crate) fn validate(message: impl Into<String>) -> Self {
        Self::Validate {
            message: message.into(),
        }
    }
}
