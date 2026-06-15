#![deny(clippy::unwrap_used, clippy::expect_used)]

//! Layered configuration for cazean.
//!
//! Precedence (low → high): built-in defaults → user config
//! (`~/.config/cazean/config.toml`) → project config
//! (`<workspace>/.cazean/config.toml`) → `CAZEAN_*` env vars.
//!
//! Each layer parses into a [`PartialConfig`] (all-optional fields); the layers
//! are merged highest-wins and then [`PartialConfig::resolve`] fills built-in
//! defaults and validates, producing a concrete [`Config`].

mod color;
mod error;
mod schema;

use std::path::{Path, PathBuf};

use etcetera::base_strategy::{BaseStrategy, Xdg};

pub use color::{ColorSpec, NamedColor, ParseColorError};
pub use error::ConfigError;
pub use schema::{
    AgentConfig, Config, KNOWN_PROVIDERS, Merge, OpenAiConfig, PartialConfig, ProviderConfig,
    ReasoningEffortConfig, ReasoningSummaryConfig, RunCommandConfig, TelemetryConfig, ToolsConfig,
    TuiColors, TuiConfig, WebSocketConfig, normalize_provider,
};

/// Relative path of the config file within a config root (XDG or project).
const CONFIG_RELATIVE_PATH: &str = "cazean/config.toml";
/// Project config lives under `<workspace>/.cazean/config.toml`.
const PROJECT_CONFIG_RELATIVE_PATH: &str = ".cazean/config.toml";

/// Load and resolve configuration for the given workspace root.
///
/// Layers, in increasing precedence: built-in defaults, user config file,
/// project config file, and environment variables. Missing files are skipped.
/// If the user config directory cannot be discovered, the user layer is
/// silently skipped (non-fatal).
pub fn load(workspace_root: &Path) -> Result<Config, ConfigError> {
    let env_vars = std::env::vars().collect::<Vec<_>>();
    load_from(workspace_root, user_config_path(), env_vars)
}

/// Discover the user-level config path using the XDG base strategy explicitly
/// (`~/.config/cazean/config.toml`, honoring `$XDG_CONFIG_HOME`). Returns
/// `None` if the config directory cannot be determined.
fn user_config_path() -> Option<PathBuf> {
    let strategy = Xdg::new().ok()?;
    Some(strategy.config_dir().join(CONFIG_RELATIVE_PATH))
}

/// Core of [`load`], parameterized over the user path and env vars so tests can
/// drive it deterministically without touching the process environment.
fn load_from<I>(
    workspace_root: &Path,
    user_path: Option<PathBuf>,
    env_vars: I,
) -> Result<Config, ConfigError>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut merged = PartialConfig::default();

    if let Some(user_path) = user_path
        && let Some(user) = read_partial(&user_path)?
    {
        merged = merged.merge(user);
    }

    let project_path = workspace_root.join(PROJECT_CONFIG_RELATIVE_PATH);
    if let Some(project) = read_partial(&project_path)? {
        merged = merged.merge(project);
    }

    merged = merged.merge(env_overlay(env_vars));
    merged.resolve()
}

/// Read and parse a config file, returning `None` if it does not exist.
fn read_partial(path: &Path) -> Result<Option<PartialConfig>, ConfigError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let partial =
        toml::from_str::<PartialConfig>(&contents).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(Some(partial))
}

/// Build the env-var overlay from an iterator of `(name, value)` pairs.
///
/// `CAZEAN_*` variables are the supported names. Parsing is permissive to
/// match historical behavior: invalid/zero numeric values fall through to the
/// lower layer (`None`) rather than erroring.
pub fn env_overlay<I>(vars: I) -> PartialConfig
where
    I: IntoIterator<Item = (String, String)>,
{
    use schema::{PartialProvider, PartialRunCommand, PartialTelemetry, PartialTools};

    let mut provider = PartialProvider::default();
    let mut run_command = PartialRunCommand::default();
    let mut telemetry = PartialTelemetry::default();
    let mut saw_provider = false;
    let mut saw_run_command = false;
    let mut saw_telemetry = false;

    for (name, value) in vars {
        match name.as_str() {
            "CAZEAN_LLM_PROVIDER" => {
                provider.provider = Some(value);
                saw_provider = true;
            }
            "CAZEAN_LLM_MODEL" => {
                provider.model = Some(value);
                saw_provider = true;
            }
            "CAZEAN_LLM_PREAMBLE" => {
                // Empty string is a real override to an empty preamble, matching
                // today's `env::var(...).ok()` behavior — do not filter it.
                provider.preamble = Some(value);
                saw_provider = true;
            }
            "CAZEAN_RUN_COMMAND_TIMEOUT_SECS" => {
                // Permissive: only a parseable, non-zero value overrides.
                if let Some(secs) = value.parse::<u64>().ok().filter(|secs| *secs > 0) {
                    run_command.default_timeout_secs = Some(secs);
                    saw_run_command = true;
                }
            }
            "CAZEAN_TRACE_STDERR" => {
                if let Some(flag) = parse_bool_like(&value) {
                    telemetry.force_stderr = Some(flag);
                    saw_telemetry = true;
                }
            }
            _ => {}
        }
    }

    PartialConfig {
        provider: saw_provider.then_some(provider),
        agent: None,
        // The run-command overlay lives under `tools`, not `provider`.
        tools: saw_run_command.then(|| PartialTools {
            run_command: Some(run_command),
            ..PartialTools::default()
        }),
        tui: None,
        telemetry: saw_telemetry.then_some(telemetry),
    }
}

/// Parse a boolean-like env value. Truthy/falsey map to `Some`; anything else
/// is `None` (ignored), preserving the historical "non-truthy = off" leniency
/// while still letting `=false` override a file's `force_stderr = true`.
fn parse_bool_like(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn no_env() -> Vec<(String, String)> {
        Vec::new()
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn parse(toml_src: &str) -> Result<PartialConfig, toml::de::Error> {
        toml::from_str::<PartialConfig>(toml_src)
    }

    #[test]
    fn defaults_resolve_to_today_behavior() -> TestResult {
        let config = PartialConfig::default().resolve()?;
        assert_eq!(config, Config::default());
        assert_eq!(config.provider.provider, "openai");
        assert_eq!(config.provider.model, "gpt-5.5");
        assert_eq!(config.provider.openai.api_key, "cazean");
        assert_eq!(config.provider.openai.base_url, "http://localhost:8317/v1");
        assert_eq!(
            config.provider.openai.reasoning_effort,
            ReasoningEffortConfig::High
        );
        assert_eq!(config.tools.run_command.default_timeout_secs, 300);
        assert_eq!(config.tui.highlight_theme, "CatppuccinMocha");
        Ok(())
    }

    #[test]
    fn project_overrides_user_and_env_overrides_project() -> TestResult {
        let user = parse("[provider]\nmodel = \"user-model\"\n")?;
        let project = parse("[provider]\nmodel = \"project-model\"\n")?;
        let config = PartialConfig::default()
            .merge(user)
            .merge(project)
            .merge(env_overlay(env(&[("CAZEAN_LLM_MODEL", "env-model")])))
            .resolve()?;
        assert_eq!(config.provider.model, "env-model");

        // Without the env layer, project wins over user.
        let user = parse("[provider]\nmodel = \"user-model\"\n")?;
        let project = parse("[provider]\nmodel = \"project-model\"\n")?;
        let config = PartialConfig::default()
            .merge(user)
            .merge(project)
            .merge(env_overlay(no_env()))
            .resolve()?;
        assert_eq!(config.provider.model, "project-model");
        Ok(())
    }

    #[test]
    fn nested_merge_preserves_sibling_fields() -> TestResult {
        // User sets the openai base_url; project sets only the model. The
        // project layer must NOT discard the user's [provider.openai] block.
        let user = parse("[provider.openai]\nbase_url = \"http://user/v1\"\n")?;
        let project = parse("[provider]\nmodel = \"m\"\n")?;
        let config = PartialConfig::default()
            .merge(user)
            .merge(project)
            .resolve()?;
        assert_eq!(config.provider.openai.base_url, "http://user/v1");
        assert_eq!(config.provider.model, "m");
        Ok(())
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(parse("[provider]\nmodle = \"x\"\n").is_err());
    }

    #[test]
    fn env_timeout_is_permissive() -> TestResult {
        // Invalid / zero timeout values fall through (no override).
        for bad in ["0", "abc", ""] {
            let config = PartialConfig::default()
                .merge(env_overlay(env(&[(
                    "CAZEAN_RUN_COMMAND_TIMEOUT_SECS",
                    bad,
                )])))
                .resolve()?;
            assert_eq!(config.tools.run_command.default_timeout_secs, 300);
        }
        let config = PartialConfig::default()
            .merge(env_overlay(env(&[(
                "CAZEAN_RUN_COMMAND_TIMEOUT_SECS",
                "42",
            )])))
            .resolve()?;
        assert_eq!(config.tools.run_command.default_timeout_secs, 42);
        Ok(())
    }

    #[test]
    fn env_preamble_empty_string_is_a_real_override() -> TestResult {
        let config = PartialConfig::default()
            .merge(env_overlay(env(&[("CAZEAN_LLM_PREAMBLE", "")])))
            .resolve()?;
        assert_eq!(config.provider.preamble, Some(String::new()));
        Ok(())
    }

    #[test]
    fn trace_stderr_is_bool_like_and_can_override_file_true() -> TestResult {
        let file = parse("[telemetry]\nforce_stderr = true\n")?;
        let config = PartialConfig::default()
            .merge(file.clone())
            .merge(env_overlay(env(&[("CAZEAN_TRACE_STDERR", "false")])))
            .resolve()?;
        assert!(!config.telemetry.force_stderr);

        // A non-bool-like value is ignored, leaving the file value intact.
        let config = PartialConfig::default()
            .merge(file)
            .merge(env_overlay(env(&[("CAZEAN_TRACE_STDERR", "banana")])))
            .resolve()?;
        assert!(config.telemetry.force_stderr);
        Ok(())
    }

    #[test]
    fn validation_rejects_unknown_provider() -> TestResult {
        let file = parse("[provider]\nprovider = \"foobar\"\n")?;
        let err = PartialConfig::default().merge(file).resolve();
        assert!(matches!(err, Err(ConfigError::Validate { .. })));
        Ok(())
    }

    #[test]
    fn validation_rejects_timeout_inversion() -> TestResult {
        let file =
            parse("[tools.run_command]\ndefault_timeout_secs = 9999\nmax_timeout_secs = 10\n")?;
        let err = PartialConfig::default().merge(file).resolve();
        assert!(matches!(err, Err(ConfigError::Validate { .. })));
        Ok(())
    }

    #[test]
    fn validation_rejects_escaping_log_file_name() -> TestResult {
        for bad in ["../escape.log", "logs/x.log", "..", "/abs.log"] {
            let file = parse(&format!("[telemetry]\nlog_file_name = \"{bad}\"\n"))?;
            let err = PartialConfig::default().merge(file).resolve();
            assert!(
                matches!(err, Err(ConfigError::Validate { .. })),
                "expected `{bad}` to be rejected"
            );
        }
        Ok(())
    }

    #[test]
    fn validation_rejects_unknown_color() {
        assert!(parse("[tui.colors]\ncode = \"chartreuse\"\n").is_err());
    }

    #[test]
    fn retry_budget_zero_is_allowed() -> TestResult {
        let file = parse("[provider.websocket]\nretry_budget = 0\n")?;
        let config = PartialConfig::default().merge(file).resolve()?;
        assert_eq!(config.provider.websocket.retry_budget, 0);
        Ok(())
    }

    #[test]
    fn provider_with_surrounding_whitespace_validates_and_normalizes() -> TestResult {
        // Regression: validation trims, so " openai " must pass — and the
        // shared normalizer must map it to a known provider arm, otherwise
        // model construction would later reject what config load accepted.
        let file = parse("[provider]\nprovider = \" OpenAI \"\n")?;
        let config = PartialConfig::default().merge(file).resolve()?;
        assert_eq!(normalize_provider(&config.provider.provider), "openai");
        assert!(KNOWN_PROVIDERS.contains(&normalize_provider(&config.provider.provider).as_str()));
        Ok(())
    }

    #[test]
    fn example_config_matches_defaults() -> TestResult {
        // The committed example documents the defaults; every uncommented value
        // must round-trip to `Config::default()`, keeping the docs honest.
        // (Comments are not preserved by TOML, so this parses rather than
        // asserting string equality.)
        let example = include_str!("../../../config.example.toml");
        let partial = toml::from_str::<PartialConfig>(example)?;
        let resolved = partial.resolve()?;
        assert_eq!(resolved, Config::default());
        Ok(())
    }
}
