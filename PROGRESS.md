# Progress

Use this file to capture concise, durable insights when a task produces knowledge worth preserving. Organize entries by topic or subsystem rather than timeline, and preserve reusable repo knowledge, implementation decisions, caveats, and verification notes; skip routine task logs.

## Repository Practices

- `PROGRESS.md` is for judgment-based durable insights worth preserving, not every task.
- Organize `PROGRESS.md` by topic or subsystem instead of chronological task history.
- `docs/system_prompt.md` is the runtime default base prompt via `include_str!` in `crates/core/src/provider.rs`; `SMOOTH_CODE_LLM_PREAMBLE` still replaces that default, while role-specific preambles from `crates/core/src/agent/role.rs` layer on top of the selected base.
- Environment context placeholders in the selected base preamble are filled by `crates/core/src/environment.rs` before role-specific and plan-mode text is appended. Keep this context cache-stable: working directory, git-repo yes/no, platform, OS version, shell, and preferred CLI availability (`rg`, `fd`, `eza`).
- The model-facing `list_dir` and `dynamic_echo` tools were removed; directory listings should go through shell commands such as `eza`, and client-mediated interaction should use the typed `ask_user_question` tool/server request. Plan mode includes `run_command` for read-only exploration/validation, but still withholds file-mutating `delete`, `edit`, and `write`.
- Source changes should go through structured file tools (`edit`, `write`, `delete`) so the runtime can emit structured `FileChangeOutput`; keep `run_command` for inspection, validation, formatters, and project commands rather than Python/`sed -i`/`awk`/redirection rewrite scripts.
- Plan-mode model rebuilds must preserve typed client-backed tools such as `ask_user_question`; the unchecked `exit_plan_mode` path runs in-turn and should reuse the `Session`'s stored client when no explicit client is passed.

## OpenAI Provider

- The OpenAI provider uses Rig's Responses WebSocket session for the manual tool loop; keep OpenAI turn streaming on `stream_completion_turn` and do not route OpenAI through the generic `CompletionModel::stream()` SSE path.
- WebSocket item decoding in `crates/core/src/provider.rs` preserves provider item IDs for reasoning deltas and reuses one internal call ID across tool-call name, args, and final tool-call events so the TUI/core can correlate streamed tool state.
- The local OpenAI-compatible proxy can close/reset WebSocket sessions without a closing handshake; retry only before any assistant item is yielded, and only treat a reset as graceful after a complete output item or terminal response has been observed.
- The local OpenAI-compatible proxy can also fail during WebSocket startup with early close/no-close-reason messages or the generic `An error occurred while processing the request.` provider error. These are retryable only before any assistant item is yielded, to avoid duplicating visible output or tool calls.
- The local OpenAI-compatible WebSocket proxy closes immediately after upgrade when `input` contains `role: system`; after Rig builds the Responses request, move system-role input items into the provider-level `instructions` field instead of downgrading them to user-visible text.
- Keep OpenAI WebSocket inbound parsing local and tolerant rather than relying on Rig's private `StreamingCompletionChunk` parser; the local proxy emits provider telemetry and some known event types with payload shapes that may not match Rig's strict structs.
- OpenAI WebSocket lifecycle responses from the local proxy may omit `output`; parse lifecycle fields (`status`, `usage`, errors, optional output IDs) from raw JSON instead of deserializing the full Responses completion object.

## TUI Subagent Display

- The parent TUI does not subscribe to child thread streams or render a child transcript. Child-visible output reaches the parent as collaboration lifecycle events emitted on the parent thread (`CollabAgentSpawnBegin`, `CollabAgentSpawnEnd`, `CollabAgentCompleted`).
- A live `spawn_agent` tool result is emitted as `ToolCallCompleted` with `result_kind: StatusUpdate` and `related_thread_id`; the TUI records that child-thread-to-call mapping and keeps the `spawn_agent` row running until the matching `CollabAgentCompleted` arrives.
- `CollabAgentSpawnBegin` and `CollabAgentSpawnEnd` are transcript-silent in the TUI because the `spawn_agent` tool row already displays the prompt/arguments and running state; keep prompt/status text out of extra info rows unless adding a distinct subagent transcript surface.
- `CollabAgentCompleted` is transcript-silent in the TUI; it only finalizes the correlated `spawn_agent` tool row. The detailed child result for the model is separate structured JSON returned by the manual tool loop.
- Parent completion notifications should include every final child status, including `Shutdown` and `NotFound`, so retained inline waiters and TUI `spawn_agent` rows cannot remain running after a child reaches a terminal state.

## TUI File Change Display

- Successful `delete`, `edit`, and `write` tools can carry structured `FileChangeOutput` metadata through `ToolCallCompletedEvent.file_change`; core decodes this metadata before sending tool results back to the model so model-visible output remains the concise success message.
- The TUI replaces a single completed file-mutating tool row with a patch transcript item and Codex-style diff summary; grouped file tool calls keep their group row and append the diff item so other entries are not lost.
- The first Smooth diff renderer intentionally omits Codex's syntax-highlighting/theme stack and uses `diffy` plus ratatui styles for line counts, gutters, hunk separators, and red/green insert/delete cues.
- After the reducer rewrite, committed file changes render as concise inline transcript summaries; full diff details remain available from the inspector/detail rendering path using the same bounded `FileChangeOutput` renderer.

## TUI Architecture

- `smooth-tui` state now flows through `UiModel::update(UiEvent) -> Vec<UiEffect>`; `App` is the async effect runner and renderer, and effect completions/failures re-enter the reducer by deterministic `EffectId`.
- TUI startup now opens a dashboard backed by `ThreadList`; `n` starts a new thread and `Enter` resumes the selected `ThreadListItem`. Resume hydration replays `ThreadResumeResponse.initial_messages` through the same protocol-event reducer path used for live events.
- Transcript storage is typed `TranscriptItem` data with stable item IDs and versions instead of dynamic `HistoryCell` trait objects. Cached row rendering is keyed by item ID, item version, width, and detail mode; resize eviction retains only current-width cache entries.
- Active assistant/reasoning streams remain separate mutable overlays and are committed into typed transcript items only when finalized, preserving the dedupe rules around deltas, completed events, late reasoning completions, and turn-completed fallbacks.
- Active streams are not in `render_cache` (they change every delta), so their wrapped output is memoized separately in `active_wrap`, keyed by `(width, active_version)`. Every write to the active lines goes through `set_active_*_lines`, which bumps `active_version`, so the memo can never go stale; this keeps the active block from being re-wrapped twice per frame (row count + visible) and on idle frames. The non-cached `append_active_lines` is now test-only.
- `TurnStart` effect context retains the submitted composer text so failed sends can restore the draft if the user has not typed a replacement while the effect was in flight.
- Transcript virtualization must count the same rows the renderer will draw: every transcript item type, including tool groups, goes through style-preserving wrapping before row counting, and active-stream separators are counted from total row state rather than currently visible rows.
- `AgentMessageCompleted` is the authoritative assistant transcript event; `AgentMessage` can follow with identical text from core/rollout replay and must be treated as a guarded fallback to avoid double-rendering live and resumed assistant messages.
- App-server subscriptions stay open for every started/resumed thread, so in-process `SessionEvent`s carry the subscription `ThreadId` and the TUI reducer ignores sourced protocol events that do not match `current_thread_id` before switching screens or mutating transcript state. Ask-user server requests are also scoped by their payload `thread_id`; inactive or invalid-thread requests are failed instead of opening overlays. Replay hydration still applies its `initial_messages` directly because the resume response is already thread-scoped.
- Keyboard scroll math should use the transcript pane's inner height, not a terminal-level approximation, so manual bottom scrolling matches what `render_transcript` actually draws.
- Row counting must also use the transcript pane's inner *width*, not `terminal_width`: in the split workspace the pane is only ~70% of the terminal, so wrapping (and thus row count and `max_scroll`) differs. `render_transcript` records the width it drew at (`transcript_inner_width`) and `max_scroll` reads it, so counting and drawing stay on the same width and the newest content stays reachable.
- Transcript wrapping is policy-split, not one-size: prose word-wraps (`wrap::wrap_line` breaks at spaces, hard-breaking only a word wider than the line, so words like "like" are not split), while code must stay column-faithful (`wrap::wrap_line_char`). Code blocks live inside assistant/reasoning Markdown, so `markdown_render` marks fenced code at the line level and `history_cell` routes those raw (pre-prefix) lines to char wrapping; inline code only colors spans, so even inline-code-only prose stays word-wrapped. Tool-group rows and `wrap::wrap_text` (diff/code) are char-wrapped too.
- Dashboard rendering reserves visible space for `QuestionPicker` when an ask-user server request arrives, so overlay key routing cannot capture input for an invisible picker. Inline file-change summaries wrap their header/path rows before truncation, keeping cached row counts aligned with rendered rows even for long paths.
- The TUI has dashboard/workspace screens plus Normal, Insert, Command, and Overlay modes. Ctrl-C remains an always-exit shortcut before mode or overlay dispatch; Visual mode and slash search remain intentionally unimplemented.

## TUI File Change Safety

- Structured file-change tool output is a private transport and must only be decoded for successful final built-in `delete`/`edit`/`write` completions; arbitrary tool stdout and opaque stream outputs stay plain text to avoid spoofed diffs.
- File-change metadata is capped at 512 KiB; oversized diffs/new-file contents and unreadable/non-UTF8 existing files use `FileChange::Omitted` so the TUI can show counts/reason without carrying or rendering large/unavailable content.
- The TUI diff renderer caps rendered diff body lines at 1,000 and appends a truncation marker, protecting transcript recalculation and scroll performance.
- Omitted file changes must preserve the original operation (`add`, `delete`, or `update`) so large added files still render as Added and external JSON clients receive operation-aware metadata.
