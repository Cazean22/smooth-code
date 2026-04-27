# AGENTS.md

## Workspace
- Root is a Cargo workspace with five members: `app-server`, `app-server-protocol`, `smooth-core`, `smooth-protocol`, and `smooth-tui`.
- `crates/tui` is the only `default-member`, and `smooth-tui` is the only binary target. `cargo run` at repo root launches the TUI.

## Crate map
- `crates/tui`: user-facing shell; starts the app server and submits thread ops.
- `crates/app-server`: in-process request bridge; not a standalone daemon.
- `crates/core`: session runtime, task orchestration, and model/provider integration.
- `crates/protocol`: shared thread/op/event/status types.
- `crates/app-server-protocol`: request/response schema between the TUI and app server.

## Commands and verification
- There are no checked-in repo wrappers or workflow configs (`README*`, `Justfile`, `Makefile`, `.github/workflows`, `nextest`, `pre-commit`, `.cargo/config*`, `rust-toolchain*` were not found). Use direct Cargo commands.
- Prefer focused verification first: `cargo check -p smooth-core`, `cargo check -p smooth-tui`, etc. Widen to `cargo check --workspace` only for cross-crate changes.
- `cargo metadata --no-deps --format-version 1` is the fastest source of truth for workspace members and targets.

## Runtime gotchas
- `smooth_tui::run()` auto-submits `Op::UserInput("Hi")` on startup. `cargo run` is not a passive smoke test.
- The TUI starts the app server in-process via `AppServerClient::start(...)` -> `app_server::in_process::start(...)`; do not assume a separate server binary exists.
- Non-TTY runs skip terminal setup, so headless runs exercise different behavior than an interactive TUI session.
- Provider config comes from `crates/core/src/provider.rs`: `SMOOTH_CODE_LLM_PROVIDER`, `SMOOTH_CODE_LLM_MODEL`, `SMOOTH_CODE_LLM_PREAMBLE`, optional `OPENAI_BASE_URL`, plus the selected provider key (`OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`).
- This project integrates LLM providers through `rig-core`; when a suitable type already exists there, prefer using the `rig-core` type directly instead of introducing a parallel local type.
- Model tool calls currently fail explicitly in `crates/core/src/provider.rs`. The protocol already defines dynamic tool-call messages, but execution is not wired end-to-end yet.
- Do not assume live server events work yet: `Session::emit_event()` constructs an event and drops it, and `app_server::in_process::start()` creates `_event_tx` without wiring it.
- `ThreadId` values are client-generated (`Uuid::now_v7()`), and sessions are created lazily in `ThreadManagerState::get_or_create()`.

## Tests and docs
- No checked-in `tests/`, `benches/`, `examples/`, or Rust `#[test]` / `#[tokio::test]` modules were found. Do not claim an established test suite or CI workflow unless you add one.
- `docs/codex-session-analysis.md` is useful design context for the session/task architecture, but executable behavior should be taken from the current source files.
- `.gitignore` only ignores `/target` and `.codex`; never treat `target/` output as source of truth.
