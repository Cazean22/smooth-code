# smooth-code agent notes

## Repo Scope

- This is a Rust 2024 workspace. No nested `AGENTS.md`/`AGENT.md` files or Cursor/Claude/Windsurf/Cline/Goose/Copilot rule files were found in this repo.
- When a real implementation, debugging, or fix task yields durable repo knowledge, record it in `PROGRESS.md` before the final response. Organize entries by subsystem/topic, not by chat timeline, and keep them focused on reusable decisions, caveats, and verification notes. Do not update `PROGRESS.md` for inspection-only/listing/exploration tasks unless the user explicitly asks to persist findings.
- `.cargo/config.toml` enables `tokio_unstable`; do not remove or bypass it casually.
- Default workspace member is `crates/tui`, so bare `cargo build`, `cargo test`, and `cargo run` target `smooth-tui` unless `--workspace` or `-p` is used.
- Workspace members are `app-server`, `app-server-protocol`, `smooth-core`, `smooth-protocol`, `smooth-state-db`, `tools`, and `smooth-tui`.

## Main Commands

- Build/check: `cargo check --workspace`, `cargo build --workspace`, `cargo run -p smooth-tui`.
- Format/lint: `cargo fmt`, `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`.
- Tests: `cargo test --workspace`; package tests use `cargo test -p smooth-tui` or `cargo test -p smooth-core`.
- Single test pattern: `cargo test -p <package> <module_path::test_name> -- --exact --nocapture`.
- Multi-agent integration tests live in `crates/core/tests/multi_agent_e2e.rs`; run them with `cargo test -p smooth-core --test multi_agent_e2e -- --nocapture`. `cargo test -p smooth-core multi_agent_e2e -- --nocapture` is only a name filter and may match zero tests.

## Architecture

- Runtime flow is TUI -> `AppServerSession` -> `app_server::in_process` -> `MessageProcessor`/`CoreMessageProcessor` -> `ThreadManagerState` -> `CoreThread`/`Core` -> Rig model/tool loop -> protocol events back to the TUI.
- `smooth-tui` is the terminal UI entrypoint. It uses ratatui/crossterm, keeps state in a reducer-style `UiModel::update(UiEvent) -> Vec<UiEffect>`, and lets `App` run async effects and render.
- `app-server` is an in-process request/event bridge. It owns `CoreMessageProcessor`, thread subscriptions, and client-directed server requests such as `ask_user_question` and `request_plan_approval`.
- `app-server-protocol` owns typed client/server request envelopes: `ClientRequest`, `ServerRequest`, `ThreadStart`, `ThreadResume`, `ThreadList`, `TurnStart`, `SetPlanMode`, and ask-user/plan-approval request/response types.
- `smooth-core` owns session/thread runtime, rollout persistence, model/provider setup, manual tool-loop execution, plan-mode state and approval, and multi-agent orchestration.
- `smooth-protocol` owns shared event/status/thread/file-change wire types, `ThreadId`, `AgentPath`, and structured `ErrorInfo`.
- `smooth-state-db` owns SQLite state at `.smooth-code/state.db`, currently thread metadata plus parent/child thread spawn edges.
- `tools` owns Rig tool definitions and implementations: `read`, `run_command`, `ask_user_question`, `spawn_agent`, `delete`, `edit`, `write`, `plan_write`, and `exit_plan_mode`.

## Persistence and Events

- Rollout sessions are newline-delimited JSON under `.smooth-code/sessions/YYYY/MM/DD/*.jsonl`; telemetry logs go to `.smooth-code/logs/smooth-tui.log`; multi-agent thread metadata lives in `.smooth-code/state.db`.
- `ThreadManagerState::start_thread` creates a root thread, registers `/root` in `AgentControl`, persists root metadata, and emits `SessionConfigured` after app-server subscribes.
- `ThreadManagerState::resume_thread` reloads rollout history/events and rehydrates open child subtrees from SQLite, emitting `CollabResumeBegin`/`CollabResumeEnd` initial messages for resumed children.
- Only selected protocol events are persisted to rollout. Check `rollout::persist_event` before assuming a new event will replay on resume.
- App-server subscriptions stay open for every started/resumed thread. `InProcessServerEvent::SessionEvent` includes the source `ThreadId`; the TUI ignores sourced protocol events that do not match `current_thread_id`.

## LLM Providers and Prompting

- LLM config comes from env vars: `SMOOTH_CODE_LLM_PROVIDER`, `SMOOTH_CODE_LLM_MODEL`, and `SMOOTH_CODE_LLM_PREAMBLE`.
- Supported providers are `openai`, `openrouter`, `anthropic`, and `gemini`. The current defaults are `openai` and `gpt-5.5`.
- The `openai` path is wired to a local OpenAI-compatible base URL at `http://localhost:8317/v1` with a placeholder API key; `openrouter`, `anthropic`, and `gemini` require their respective API keys.
- `docs/system_prompt.md` is the default base prompt included by `crates/core/src/provider.rs`. `SMOOTH_CODE_LLM_PREAMBLE` replaces that base prompt; role-specific preambles and plan-mode instructions layer on top.
- Environment placeholders are filled by `crates/core/src/environment.rs`: working directory, git repo yes/no, platform, OS version, shell, and command availability for `rg`, `fd`, and `eza`.
- OpenAI uses Rig's Responses model plus a local WebSocket streaming path for the manual tool loop. Keep OpenAI manual turns on `stream_completion_turn`; do not route them through the generic SSE-style `CompletionModel::stream()` path.
- The local OpenAI-compatible proxy may emit provider telemetry, omit `output` on lifecycle events, and reset/close WebSockets early. Keep the tolerant local parser and retry only before any assistant item has been yielded.

## Tools and Plan Mode

- Default tools include `read`, `run_command`, `delete`, `edit`, `write`, optional `ask_user_question`, and `spawn_agent`.
- Plan mode swaps mutating file tools for `plan_write` and `exit_plan_mode`, while keeping read/inspection tools and `spawn_agent`. `PLAN_MODE_TOOLS` in `crates/core/src/agent/plan_mode.rs` is the single source of truth for that list: the plan-mode instructions are generated from it and a provider test asserts the registered tool set matches.
- Both plan-mode and normal session models are prebuilt once at thread creation (`SessionModels` on `Session`); toggling plan mode is a flag flip plus a `PlanModeChanged` event, never a model rebuild. `apply_plan_mode` holds the `active_turn` guard and refuses mid-turn; the unchecked variant is reserved for the in-turn `exit_plan_mode` handler.
- `exit_plan_mode` is an approval gate, not a plain toggle: core reads the plan written by `plan_write` (`tools::plan_file_path`), sends a `RequestPlanApproval` server request, and waits. Approval flips plan mode off mid-turn (the rest of the turn runs on the normal model); rejection keeps plan mode on and feeds the user's feedback back as a successful tool result. Missing plan file, non-plan-mode calls, or a client without plan-approval support are tool errors.
- Plan mode holds for the whole agent subtree: any `spawn_agent` issued while the parent is in plan mode is coerced to the structurally read-only `Explore` prompt kind regardless of `subagent_type`.
- `PlanModeChanged` is persisted to the rollout and the last one wins on resume (`ResumeState.plan_mode`), so a thread resumed mid-planning keeps its restricted tool set and the TUI badge restores from replay.
- The model-facing `list_dir` and `dynamic_echo` tools are gone. Directory inspection should use shell commands through `run_command` or local agent shell tools.
- Source changes should go through structured file tools (`edit`, `write`, `delete`) so core can emit `FileChangeOutput` and the TUI can render diffs. Keep `run_command` for inspection, validation, formatters, and project commands, not ad hoc source rewrites.
- Tool argument schemas should derive `schemars::JsonSchema` on args structs and use `schema_for!(Args).to_value()`. Prefer `#[serde(deny_unknown_fields)]`; doc comments become schema descriptions and `#[schemars(range(...))]` carries numeric bounds.
- Structured file-change tool output is a private transport. Only successful final `delete`/`edit`/`write` completions are decoded; arbitrary stdout or failed tool output must remain plain text to avoid spoofed diffs.
- File-change metadata is capped by `tools::MAX_FILE_CHANGE_BYTES` at 512 KiB. Oversized or unavailable content should use `FileChange::Omitted` with the original operation.

## Multi-Agent Behavior

- Only `spawn_agent` is registered as an LLM-callable agent tool. Its schema lives in `tools`, but execution is handled manually in `crates/core/src/tasks/regular.rs` through `AgentControl`.
- Do not reintroduce the old `MultiAgentClient` adapter or expose `send_message`, `list_agents`, or `close_agent` as model tools. Internal controls live behind `AgentControl`/`ThreadManagerState`.
- Agent paths use typed `AgentPath`; root is `/root`, children are generated under their parent path, and relative references resolve from the current agent path.
- Subagent limits are enforced in `AgentControl`: max depth `8`, max live threads `16`.
- Built-in agent roles are `default`, `explorer`, and `worker` in `crates/core/src/agent/role.rs`. Role-specific preambles may change the child model prompt; `spawn_agent` `model` overrides are parsed but currently return `spawn_agent model override is not implemented yet`.
- Manual tool loop behavior: collect all streamed tool calls before execution, run normal tools concurrently, start all `spawn_agent` calls, return live spawn status only for mixed normal/subagent batches after the grace period, and keep the parent turn open for retained subagent completions.
- Pure `spawn_agent` batches additionally block on every retained receiver from prior turns. Retained completions that resolve while the next stream is running are appended as `UserContent::Text` before the loop continues.
- Parent completion notifications should include every final child status, including `Shutdown` and `NotFound`, so retained inline waiters and TUI `spawn_agent` rows cannot remain running.

## TUI Notes

- The TUI has dashboard/workspace screens plus Normal, Insert, Command, and Overlay modes. Ctrl-C is an always-exit shortcut before mode or overlay dispatch.
- Startup opens the dashboard backed by `ThreadList`; `n` starts a new thread and `Enter` resumes the selected thread.
- Resume hydration replays `ThreadResumeResponse.initial_messages` through the same protocol reducer path used for live events.
- Ask-user server requests are scoped by payload `thread_id`; inactive or invalid-thread requests are failed instead of opening overlays.
- `CollabAgentSpawnBegin`, `CollabAgentSpawnEnd`, and `CollabAgentCompleted` are transcript-silent. The `spawn_agent` tool row displays running/final state using `ToolCallCompleted.result_kind = StatusUpdate` and `related_thread_id`.
- `AgentMessageCompleted` is the authoritative assistant transcript event; `AgentMessage` is a guarded fallback and may duplicate replayed/completed text if treated as authoritative.
- Transcript storage is typed `TranscriptItem` data with stable IDs and versions. Cached rendering is keyed by item ID/version/width/detail mode; active assistant/reasoning streams are memoized separately by active version.
- Row counting and scrolling must use the transcript pane's inner width/height, not terminal-wide approximations, so wrapped rows and rendered rows stay aligned.
- Prose and code wrap differently: prose word-wraps, fenced code and tool/diff rows char-wrap. Keep this split when changing markdown, history, or wrapping code.

## Error Handling

- Workspace crates deny `clippy::unwrap_used` and `clippy::expect_used` across all targets, including tests. Use `Result`-returning tests, `?`, explicit `let Some/Ok ... else { panic!(...) }` assertions, or typed error conversion instead of `unwrap`/`expect`.
- Crate boundaries use typed errors: `smooth_protocol::AgentPathError`, `smooth_core::CoreError`, `app_server::AppServerError`, `tools::ToolError`, `smooth_state_db::StateDbError`, and `smooth_tui::TuiError`.
- Keep `anyhow` at app entrypoints/tests or provider-facing glue when useful, but prefer crate result aliases and typed variants for internal public APIs.
- Wire-level errors are structured: `app-server-protocol::JsonRpcError` keeps JSON-RPC `code`/`message` and carries `smooth_protocol::ErrorInfo { kind, message, source, details }` in `data`.
- Protocol `ErrorEvent` and `AgentStatus::Errored` carry `ErrorInfo` directly. Do not reintroduce string-only error payloads.
- App-server JSON-RPC conversion is centralized through `AppServerError::to_json_rpc_error`; core errors preserve their `smooth-core` source and typed kind when surfaced to clients.
- Tool errors expose stable `kind()` values while preserving readable `Display` text for model/UI output.
- Replace poisoned `std::sync::Mutex` assumptions with helpers that map lock errors into typed domain errors. In tests, map poisoned locks into test errors rather than using `expect`.

## Coding Style

- Follow `rustfmt`; keep imports grouped as std, external crates, then crate/local modules, matching nearby code.
- This project integrates LLM providers through `rig-core`; prefer existing Rig types over parallel local types when suitable.
- Use typed structs/enums plus serde rename attributes for wire formats. Avoid ad hoc JSON construction when a protocol type already exists.
- Shared async state is typically `Arc` plus Tokio `Mutex`/`RwLock`/`watch`/`broadcast`; align new concurrency primitives with that style.
- Naming conventions are consistent: `*Event`, `*Params`, `*Response`, `*Task`, `*Tool`, `*Session`, `*Thread`; modules and functions stay `snake_case`.
- Keep changes scoped to the relevant crate and preserve the existing TUI -> app-server -> core -> protocol layering instead of bypassing it.
