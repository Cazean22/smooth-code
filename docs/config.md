# Configuration

cazean reads layered TOML configuration. Each layer overrides the one
before it (lowest precedence first):

1. **Built-in defaults** — reproduce the hardcoded defaults. See
   `config.example.toml` at the repo root for the full set with comments.
2. **User config** — `~/.cazean/config.toml` (override the directory with
   `$CAZEAN_HOME`).
3. **Project config** — `<workspace>/.cazean/config.toml`, where
   `<workspace>` is the current working directory.
4. **Environment variables** — the `CAZEAN_*` variables below.

Missing files are skipped. Configuration is loaded once at startup (before
logging is initialized), so a malformed file prints a clear error to stderr and
the process exits.

## Format and validation

- Unknown keys are rejected (`deny_unknown_fields`) — typos fail loudly with the
  file path and TOML line/column.
- Semantic checks run after merging and report the full key path and value, e.g.
  `tools.run_command.default_timeout_secs (9999) must be <= max_timeout_secs (10)`.
- `provider.provider` must be one of `openai`, `openrouter`, `anthropic`,
  `gemini`. An unknown provider fails at startup (rather than at first model use).
- `tui.highlight_theme` is validated against the available two-face themes by the
  TUI at startup; an unknown theme is an error, with a `CatppuccinMocha` fallback
  as a last-resort defense.
- Colors (`[tui.colors]`) accept a named color (`"cyan"`, `"dark-gray"`, …), a
  palette index (`"22"` or `"indexed:22"`, `0..=255`), or RGB hex (`"#rrggbb"`).

## Environment variables (v1)

Only these variables are supported — there is **no** generic
`CAZEAN_<SECTION>_<KEY>` mapping for every TOML key yet. New keys are
file-only.

| Variable | Maps to | Notes |
|---|---|---|
| `CAZEAN_LLM_PROVIDER` | `provider.provider` | |
| `CAZEAN_LLM_MODEL` | `provider.model` | empty string accepted as-is |
| `CAZEAN_LLM_PREAMBLE` | `provider.preamble` | empty string is a real override to an empty preamble |
| `CAZEAN_RUN_COMMAND_TIMEOUT_SECS` | `tools.run_command.default_timeout_secs` | invalid/zero values are ignored (fall through) |
| `CAZEAN_TRACE_STDERR` | `telemetry.force_stderr` | truthy (`1/true/yes/on`) → true, falsey (`0/false/no/off`) → false, anything else ignored |

These env vars are permissive (an invalid value falls through to the lower
layer); values in TOML files are strict (an invalid value is an error).

Provider API keys (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`)
are read directly from the environment and are intentionally **not** part of the
config file — secrets do not belong in committed project config. The OpenAI
`api_key`/`base_url` defaults are the exception, since they are a non-secret
local-proxy stand-in.

## Notes and limits

- `tools.max_tool_output_bytes` bounds the **captured** tool output; a
  `\n...[truncated]` suffix is appended after clamping, so the returned text can
  be slightly longer. Avoid pathologically small values.
- `tools.max_skill_bytes` caps how much of a `SKILL.md` is read before
  truncation, everywhere skills are loaded (the `skill` tool, `/name`
  slash-command expansion, and the TUI skill popup).
- `[provider.websocket]` tunes the OpenAI WebSocket retry path used by both the
  provider stream and the manual turn-retry loop. `retry_budget = 0` disables
  pre-output retries. These values only affect the OpenAI provider.
- `tools.web_search.enabled` (default `true`) declares OpenAI's hosted
  `web_search` tool, which the backend executes server-side via the Responses
  API; the model's answer streams back with citations. It applies to the OpenAI
  provider only (other providers ignore it) and is suppressed while in plan mode.
- `telemetry.log_file_name` must be a bare file name (no path separators, `.`,
  or `..`); it is written under `~/.cazean/logs/`.

## Storage locations

cazean keeps user-global state in a single home directory and per-project state
alongside the workspace:

- **User-global home** — `~/.cazean/` (identical layout on Linux, macOS, and
  Windows, where it resolves to `%USERPROFILE%\.cazean\`). Set `$CAZEAN_HOME` to
  a non-empty path to relocate it. Holds:
  - `config.toml` — user config (layer 2 above).
  - `skills/` — user-global skills.
  - `logs/cazean.log` — the runtime log (file name from `telemetry.log_file_name`).
- **Project-local** — `<workspace>/.cazean/`. Holds `config.toml` (project
  config, layer 3), `skills/` (project skills), `sessions/`, `state.db`, and
  `plans/`.

Skills are loaded from both `~/.cazean/skills/` and `<workspace>/.cazean/skills`;
when both define a skill with the same name, the project's wins.

`$CAZEAN_HOME` overrides only the user-global home directory. It is distinct from
the `CAZEAN_*` config-override variables above, which map to individual config
fields.
