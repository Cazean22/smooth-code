use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::color::ColorSpec;
use crate::error::ConfigError;

/// The set of LLM providers the app knows how to build. Owned here so an
/// unknown provider fails at config-load time rather than first model use.
pub const KNOWN_PROVIDERS: &[&str] = &["openai", "openrouter", "anthropic", "gemini"];

/// Canonical form of a provider string for matching: trimmed and lowercased.
/// Both config validation and provider construction must use this so a value
/// that passes validation (which trims) also matches a provider arm later.
pub fn normalize_provider(provider: &str) -> String {
    provider.trim().to_ascii_lowercase()
}

// ===========================================================================
// Reasoning enums (mirror the Rig API exactly; converted to Rig types in core)
// ===========================================================================

/// OpenAI reasoning effort. Mirrors `rig`'s `ReasoningEffort`. Default is
/// `High`, preserving today's hardcoded behavior (not Rig's own `Medium`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffortConfig {
    None,
    Minimal,
    Low,
    Medium,
    #[default]
    High,
    Xhigh,
}

/// OpenAI reasoning summary level. Mirrors `rig`'s `ReasoningSummaryLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummaryConfig {
    #[default]
    Auto,
    Concise,
    Detailed,
}

// ===========================================================================
// Resolved config — concrete types consumers use
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub provider: ProviderConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub tui: TuiConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub provider: String,
    pub model: String,
    /// `None` means use the built-in system prompt. `Some` (including an empty
    /// string) overrides the root preamble only.
    pub preamble: Option<String>,
    pub max_turns: u32,
    pub openai: OpenAiConfig,
    pub websocket: WebSocketConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: String,
    pub reasoning_effort: ReasoningEffortConfig,
    pub reasoning_summary: ReasoningSummaryConfig,
}

/// OpenAI WebSocket retry/cancel tuning. Used by both the provider stream and
/// the manual turn-retry loop (`tasks/regular.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct WebSocketConfig {
    /// Number of transient-failure retries before output starts. `0` disables.
    pub retry_budget: usize,
    /// Base backoff (ms) for the exponential retry delay.
    pub retry_base_ms: u64,
    /// Maximum backoff (ms) the retry delay is capped to.
    pub retry_max_ms: u64,
    /// How long (ms) to keep draining after a cancel before dropping the socket.
    pub cancel_drain_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub max_depth: i32,
    pub max_threads: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolsConfig {
    pub run_command: RunCommandConfig,
    pub read_default_limit: usize,
    pub max_tool_output_bytes: usize,
    pub max_file_change_bytes: usize,
    pub max_todos: usize,
    pub max_skill_bytes: usize,
    pub web_search: WebSearchConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunCommandConfig {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub term_grace_ms: u64,
}

/// The hosted `web_search` tool. Only the OpenAI provider declares it (it is
/// executed server-side via the Responses API); other providers ignore it.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiConfig {
    pub highlight_theme: String,
    pub max_highlight_bytes: usize,
    pub max_highlight_lines: usize,
    pub max_rendered_diff_lines: usize,
    pub colors: TuiColors,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiColors {
    pub diff_add_bg: ColorSpec,
    pub diff_delete_bg: ColorSpec,
    pub code: ColorSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryConfig {
    pub log_filter: String,
    pub log_file_name: String,
    pub force_stderr: bool,
}

// ===========================================================================
// Defaults — the single source of truth for the built-in layer.
// Each value reproduces today's hardcoded behavior. (`Config` itself derives
// `Default` since every section is `Default`.)
// ===========================================================================

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            preamble: None,
            max_turns: 99999,
            openai: OpenAiConfig::default(),
            websocket: WebSocketConfig::default(),
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key: "cazean".to_string(),
            base_url: "http://localhost:8317/v1".to_string(),
            reasoning_effort: ReasoningEffortConfig::default(),
            reasoning_summary: ReasoningSummaryConfig::default(),
        }
    }
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            retry_budget: 8,
            retry_base_ms: 250,
            retry_max_ms: 3000,
            cancel_drain_ms: 1500,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_threads: 16,
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            run_command: RunCommandConfig::default(),
            read_default_limit: 2000,
            max_tool_output_bytes: 16 * 1024,
            max_file_change_bytes: 512 * 1024,
            max_todos: 50,
            max_skill_bytes: 64 * 1024,
            web_search: WebSearchConfig::default(),
        }
    }
}

impl Default for RunCommandConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 300,
            max_timeout_secs: 3600,
            term_grace_ms: 2000,
        }
    }
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            highlight_theme: "CatppuccinMocha".to_string(),
            max_highlight_bytes: 512 * 1024,
            max_highlight_lines: 10_000,
            max_rendered_diff_lines: 1000,
            colors: TuiColors::default(),
        }
    }
}

impl Default for TuiColors {
    fn default() -> Self {
        Self {
            diff_add_bg: ColorSpec::Indexed(22),
            diff_delete_bg: ColorSpec::Indexed(52),
            code: ColorSpec::Named(crate::color::NamedColor::Cyan),
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_filter: "info,cazean_tui=debug,app_server=debug,cazean_core=debug".to_string(),
            log_file_name: "cazean.log".to_string(),
            force_stderr: false,
        }
    }
}

// ===========================================================================
// Partial config — one Option-per-field overlay layer. This is what every
// TOML file and the env overlay deserialize into. `deny_unknown_fields`
// catches typos.
// ===========================================================================

/// Overlay `higher` on top of `lower`, returning the merged option. Scalars:
/// the higher layer wins when present. Used for every leaf field.
fn overlay<T>(lower: Option<T>, higher: Option<T>) -> Option<T> {
    higher.or(lower)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialConfig {
    pub provider: Option<PartialProvider>,
    pub agent: Option<PartialAgent>,
    pub tools: Option<PartialTools>,
    pub tui: Option<PartialTui>,
    pub telemetry: Option<PartialTelemetry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialProvider {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub preamble: Option<String>,
    pub max_turns: Option<u32>,
    pub openai: Option<PartialOpenAi>,
    pub websocket: Option<PartialWebSocket>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialOpenAi {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub reasoning_effort: Option<ReasoningEffortConfig>,
    pub reasoning_summary: Option<ReasoningSummaryConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialWebSocket {
    pub retry_budget: Option<usize>,
    pub retry_base_ms: Option<u64>,
    pub retry_max_ms: Option<u64>,
    pub cancel_drain_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialAgent {
    pub max_depth: Option<i32>,
    pub max_threads: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialTools {
    pub run_command: Option<PartialRunCommand>,
    pub read_default_limit: Option<usize>,
    pub max_tool_output_bytes: Option<usize>,
    pub max_file_change_bytes: Option<usize>,
    pub max_todos: Option<usize>,
    pub max_skill_bytes: Option<usize>,
    pub web_search: Option<PartialWebSearch>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialRunCommand {
    pub default_timeout_secs: Option<u64>,
    pub max_timeout_secs: Option<u64>,
    pub term_grace_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialWebSearch {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialTui {
    pub highlight_theme: Option<String>,
    pub max_highlight_bytes: Option<usize>,
    pub max_highlight_lines: Option<usize>,
    pub max_rendered_diff_lines: Option<usize>,
    pub colors: Option<PartialTuiColors>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialTuiColors {
    pub diff_add_bg: Option<ColorSpec>,
    pub diff_delete_bg: Option<ColorSpec>,
    pub code: Option<ColorSpec>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialTelemetry {
    pub log_filter: Option<String>,
    pub log_file_name: Option<String>,
    pub force_stderr: Option<bool>,
}

// ===========================================================================
// Merge — nested sections recurse so a higher layer setting one field of a
// section does not discard the lower layer's other fields in that section.
// ===========================================================================

/// Merge a nested optional partial: when both layers set the section, merge it
/// field-by-field; otherwise take whichever side is present.
fn merge_nested<T: Merge>(lower: Option<T>, higher: Option<T>) -> Option<T> {
    match (lower, higher) {
        (Some(low), Some(high)) => Some(low.merge(high)),
        (low, None) => low,
        (None, high) => high,
    }
}

/// Field-wise overlay of one partial on top of another.
pub trait Merge {
    /// `self` is the lower-precedence layer; `higher` wins per field.
    fn merge(self, higher: Self) -> Self;
}

impl Merge for PartialConfig {
    fn merge(self, higher: Self) -> Self {
        Self {
            provider: merge_nested(self.provider, higher.provider),
            agent: merge_nested(self.agent, higher.agent),
            tools: merge_nested(self.tools, higher.tools),
            tui: merge_nested(self.tui, higher.tui),
            telemetry: merge_nested(self.telemetry, higher.telemetry),
        }
    }
}

impl Merge for PartialProvider {
    fn merge(self, higher: Self) -> Self {
        Self {
            provider: overlay(self.provider, higher.provider),
            model: overlay(self.model, higher.model),
            preamble: overlay(self.preamble, higher.preamble),
            max_turns: overlay(self.max_turns, higher.max_turns),
            openai: merge_nested(self.openai, higher.openai),
            websocket: merge_nested(self.websocket, higher.websocket),
        }
    }
}

impl Merge for PartialWebSocket {
    fn merge(self, higher: Self) -> Self {
        Self {
            retry_budget: overlay(self.retry_budget, higher.retry_budget),
            retry_base_ms: overlay(self.retry_base_ms, higher.retry_base_ms),
            retry_max_ms: overlay(self.retry_max_ms, higher.retry_max_ms),
            cancel_drain_ms: overlay(self.cancel_drain_ms, higher.cancel_drain_ms),
        }
    }
}

impl Merge for PartialOpenAi {
    fn merge(self, higher: Self) -> Self {
        Self {
            api_key: overlay(self.api_key, higher.api_key),
            base_url: overlay(self.base_url, higher.base_url),
            reasoning_effort: overlay(self.reasoning_effort, higher.reasoning_effort),
            reasoning_summary: overlay(self.reasoning_summary, higher.reasoning_summary),
        }
    }
}

impl Merge for PartialAgent {
    fn merge(self, higher: Self) -> Self {
        Self {
            max_depth: overlay(self.max_depth, higher.max_depth),
            max_threads: overlay(self.max_threads, higher.max_threads),
        }
    }
}

impl Merge for PartialTools {
    fn merge(self, higher: Self) -> Self {
        Self {
            run_command: merge_nested(self.run_command, higher.run_command),
            read_default_limit: overlay(self.read_default_limit, higher.read_default_limit),
            max_tool_output_bytes: overlay(
                self.max_tool_output_bytes,
                higher.max_tool_output_bytes,
            ),
            max_file_change_bytes: overlay(
                self.max_file_change_bytes,
                higher.max_file_change_bytes,
            ),
            max_todos: overlay(self.max_todos, higher.max_todos),
            max_skill_bytes: overlay(self.max_skill_bytes, higher.max_skill_bytes),
            web_search: merge_nested(self.web_search, higher.web_search),
        }
    }
}

impl Merge for PartialRunCommand {
    fn merge(self, higher: Self) -> Self {
        Self {
            default_timeout_secs: overlay(self.default_timeout_secs, higher.default_timeout_secs),
            max_timeout_secs: overlay(self.max_timeout_secs, higher.max_timeout_secs),
            term_grace_ms: overlay(self.term_grace_ms, higher.term_grace_ms),
        }
    }
}

impl Merge for PartialWebSearch {
    fn merge(self, higher: Self) -> Self {
        Self {
            enabled: overlay(self.enabled, higher.enabled),
        }
    }
}

impl Merge for PartialTui {
    fn merge(self, higher: Self) -> Self {
        Self {
            highlight_theme: overlay(self.highlight_theme, higher.highlight_theme),
            max_highlight_bytes: overlay(self.max_highlight_bytes, higher.max_highlight_bytes),
            max_highlight_lines: overlay(self.max_highlight_lines, higher.max_highlight_lines),
            max_rendered_diff_lines: overlay(
                self.max_rendered_diff_lines,
                higher.max_rendered_diff_lines,
            ),
            colors: merge_nested(self.colors, higher.colors),
        }
    }
}

impl Merge for PartialTuiColors {
    fn merge(self, higher: Self) -> Self {
        Self {
            diff_add_bg: overlay(self.diff_add_bg, higher.diff_add_bg),
            diff_delete_bg: overlay(self.diff_delete_bg, higher.diff_delete_bg),
            code: overlay(self.code, higher.code),
        }
    }
}

impl Merge for PartialTelemetry {
    fn merge(self, higher: Self) -> Self {
        Self {
            log_filter: overlay(self.log_filter, higher.log_filter),
            log_file_name: overlay(self.log_file_name, higher.log_file_name),
            force_stderr: overlay(self.force_stderr, higher.force_stderr),
        }
    }
}

// ===========================================================================
// Resolve — fill defaults, then validate.
// ===========================================================================

impl PartialConfig {
    /// Fold the merged partial onto built-in defaults and validate the result.
    pub fn resolve(self) -> Result<Config, ConfigError> {
        let defaults = Config::default();
        let provider = resolve_provider(self.provider.unwrap_or_default(), defaults.provider);
        let agent = resolve_agent(self.agent.unwrap_or_default(), defaults.agent);
        let tools = resolve_tools(self.tools.unwrap_or_default(), defaults.tools);
        let tui = resolve_tui(self.tui.unwrap_or_default(), defaults.tui);
        let telemetry = resolve_telemetry(self.telemetry.unwrap_or_default(), defaults.telemetry);

        let config = Config {
            provider,
            agent,
            tools,
            tui,
            telemetry,
        };
        config.validate()?;
        Ok(config)
    }
}

fn resolve_provider(partial: PartialProvider, def: ProviderConfig) -> ProviderConfig {
    ProviderConfig {
        provider: partial.provider.unwrap_or(def.provider),
        model: partial.model.unwrap_or(def.model),
        // `None` is a meaningful resolved value (use built-in prompt), so the
        // merged option passes straight through rather than filling a default.
        preamble: partial.preamble,
        max_turns: partial.max_turns.unwrap_or(def.max_turns),
        openai: resolve_openai(partial.openai.unwrap_or_default(), def.openai),
        websocket: resolve_websocket(partial.websocket.unwrap_or_default(), def.websocket),
    }
}

fn resolve_websocket(partial: PartialWebSocket, def: WebSocketConfig) -> WebSocketConfig {
    WebSocketConfig {
        retry_budget: partial.retry_budget.unwrap_or(def.retry_budget),
        retry_base_ms: partial.retry_base_ms.unwrap_or(def.retry_base_ms),
        retry_max_ms: partial.retry_max_ms.unwrap_or(def.retry_max_ms),
        cancel_drain_ms: partial.cancel_drain_ms.unwrap_or(def.cancel_drain_ms),
    }
}

fn resolve_openai(partial: PartialOpenAi, def: OpenAiConfig) -> OpenAiConfig {
    OpenAiConfig {
        api_key: partial.api_key.unwrap_or(def.api_key),
        base_url: partial.base_url.unwrap_or(def.base_url),
        reasoning_effort: partial.reasoning_effort.unwrap_or(def.reasoning_effort),
        reasoning_summary: partial.reasoning_summary.unwrap_or(def.reasoning_summary),
    }
}

fn resolve_agent(partial: PartialAgent, def: AgentConfig) -> AgentConfig {
    AgentConfig {
        max_depth: partial.max_depth.unwrap_or(def.max_depth),
        max_threads: partial.max_threads.unwrap_or(def.max_threads),
    }
}

fn resolve_tools(partial: PartialTools, def: ToolsConfig) -> ToolsConfig {
    ToolsConfig {
        run_command: resolve_run_command(partial.run_command.unwrap_or_default(), def.run_command),
        read_default_limit: partial.read_default_limit.unwrap_or(def.read_default_limit),
        max_tool_output_bytes: partial
            .max_tool_output_bytes
            .unwrap_or(def.max_tool_output_bytes),
        max_file_change_bytes: partial
            .max_file_change_bytes
            .unwrap_or(def.max_file_change_bytes),
        max_todos: partial.max_todos.unwrap_or(def.max_todos),
        max_skill_bytes: partial.max_skill_bytes.unwrap_or(def.max_skill_bytes),
        web_search: resolve_web_search(partial.web_search.unwrap_or_default(), def.web_search),
    }
}

fn resolve_run_command(partial: PartialRunCommand, def: RunCommandConfig) -> RunCommandConfig {
    RunCommandConfig {
        default_timeout_secs: partial
            .default_timeout_secs
            .unwrap_or(def.default_timeout_secs),
        max_timeout_secs: partial.max_timeout_secs.unwrap_or(def.max_timeout_secs),
        term_grace_ms: partial.term_grace_ms.unwrap_or(def.term_grace_ms),
    }
}

fn resolve_web_search(partial: PartialWebSearch, def: WebSearchConfig) -> WebSearchConfig {
    WebSearchConfig {
        enabled: partial.enabled.unwrap_or(def.enabled),
    }
}

fn resolve_tui(partial: PartialTui, def: TuiConfig) -> TuiConfig {
    TuiConfig {
        highlight_theme: partial.highlight_theme.unwrap_or(def.highlight_theme),
        max_highlight_bytes: partial
            .max_highlight_bytes
            .unwrap_or(def.max_highlight_bytes),
        max_highlight_lines: partial
            .max_highlight_lines
            .unwrap_or(def.max_highlight_lines),
        max_rendered_diff_lines: partial
            .max_rendered_diff_lines
            .unwrap_or(def.max_rendered_diff_lines),
        colors: resolve_colors(partial.colors.unwrap_or_default(), def.colors),
    }
}

fn resolve_colors(partial: PartialTuiColors, def: TuiColors) -> TuiColors {
    TuiColors {
        diff_add_bg: partial.diff_add_bg.unwrap_or(def.diff_add_bg),
        diff_delete_bg: partial.diff_delete_bg.unwrap_or(def.diff_delete_bg),
        code: partial.code.unwrap_or(def.code),
    }
}

fn resolve_telemetry(partial: PartialTelemetry, def: TelemetryConfig) -> TelemetryConfig {
    TelemetryConfig {
        log_filter: partial.log_filter.unwrap_or(def.log_filter),
        log_file_name: partial.log_file_name.unwrap_or(def.log_file_name),
        force_stderr: partial.force_stderr.unwrap_or(def.force_stderr),
    }
}

// ===========================================================================
// Validation — runs after the merge, so it reports key paths + values rather
// than file/line. Syntactic and range checks only (no ratatui/two-face deps).
// ===========================================================================

impl Config {
    fn validate(&self) -> Result<(), ConfigError> {
        let provider = normalize_provider(&self.provider.provider);
        if provider.is_empty() {
            return Err(ConfigError::validate("provider.provider must not be empty"));
        }
        if !KNOWN_PROVIDERS.contains(&provider.as_str()) {
            return Err(ConfigError::validate(format!(
                "provider.provider '{}' is not one of {KNOWN_PROVIDERS:?}",
                self.provider.provider
            )));
        }
        // `provider.model` is intentionally NOT checked for emptiness: today an
        // empty `CAZEAN_LLM_MODEL` is accepted as-is and passed to the
        // provider, so the resolved config preserves that.
        if self.provider.max_turns == 0 {
            return Err(ConfigError::validate("provider.max_turns must be >= 1"));
        }

        let run = &self.tools.run_command;
        if run.default_timeout_secs == 0 {
            return Err(ConfigError::validate(
                "tools.run_command.default_timeout_secs must be >= 1",
            ));
        }
        if run.max_timeout_secs == 0 {
            return Err(ConfigError::validate(
                "tools.run_command.max_timeout_secs must be >= 1",
            ));
        }
        if run.default_timeout_secs > run.max_timeout_secs {
            return Err(ConfigError::validate(format!(
                "tools.run_command.default_timeout_secs ({}) must be <= max_timeout_secs ({})",
                run.default_timeout_secs, run.max_timeout_secs
            )));
        }

        check_nonzero("tools.read_default_limit", self.tools.read_default_limit)?;
        check_nonzero(
            "tools.max_tool_output_bytes",
            self.tools.max_tool_output_bytes,
        )?;
        check_nonzero(
            "tools.max_file_change_bytes",
            self.tools.max_file_change_bytes,
        )?;
        check_nonzero("tools.max_todos", self.tools.max_todos)?;
        check_nonzero("tools.max_skill_bytes", self.tools.max_skill_bytes)?;
        // `provider.websocket.retry_budget` may be 0 (disable retries); the
        // delay/drain values are durations where 0 is meaningful, so none are
        // range-checked here.

        if self.agent.max_depth < 1 {
            return Err(ConfigError::validate("agent.max_depth must be >= 1"));
        }
        check_nonzero("agent.max_threads", self.agent.max_threads)?;

        check_nonzero("tui.max_highlight_bytes", self.tui.max_highlight_bytes)?;
        check_nonzero("tui.max_highlight_lines", self.tui.max_highlight_lines)?;
        check_nonzero(
            "tui.max_rendered_diff_lines",
            self.tui.max_rendered_diff_lines,
        )?;

        validate_log_file_name(&self.telemetry.log_file_name)?;
        Ok(())
    }
}

fn check_nonzero(key: &str, value: usize) -> Result<(), ConfigError> {
    if value == 0 {
        Err(ConfigError::validate(format!("{key} must be >= 1")))
    } else {
        Ok(())
    }
}

/// `log_file_name` is joined under `~/.cazean/logs/`; it must be a bare
/// file name so a config file can't redirect logs outside that directory.
fn validate_log_file_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::validate(
            "telemetry.log_file_name must not be empty",
        ));
    }
    let mut components = Path::new(name).components();
    let is_single_normal = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !is_single_normal {
        return Err(ConfigError::validate(format!(
            "telemetry.log_file_name '{name}' must be a bare file name \
             (no path separators, `.`, or `..`)"
        )));
    }
    Ok(())
}
