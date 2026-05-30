# Progress

Use this file to capture concise, durable insights when a task produces knowledge worth preserving. Organize entries by topic or subsystem rather than timeline, and preserve reusable repo knowledge, implementation decisions, caveats, and verification notes; skip routine task logs.

## Repository Practices

- `PROGRESS.md` is for judgment-based durable insights worth preserving, not every task.
- Organize `PROGRESS.md` by topic or subsystem instead of chronological task history.

## OpenAI Provider

- The OpenAI provider uses Rig's Responses WebSocket session for the manual tool loop; keep OpenAI turn streaming on `stream_completion_turn` and do not route OpenAI through the generic `CompletionModel::stream()` SSE path.
- WebSocket item decoding in `crates/core/src/provider.rs` preserves provider item IDs for reasoning deltas and reuses one internal call ID across tool-call name, args, and final tool-call events so the TUI/core can correlate streamed tool state.
- The local OpenAI-compatible proxy can close/reset WebSocket sessions without a closing handshake; retry only before any assistant item is yielded, and only treat a reset as graceful after a complete output item or terminal response has been observed.
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

- Successful `edit` and `write` tools can carry structured `FileChangeOutput` metadata through `ToolCallCompletedEvent.file_change`; core decodes this metadata before sending tool results back to the model so model-visible output remains the concise success message.
- The TUI replaces a single completed file-mutating tool row with a `PatchHistoryCell` and Codex-style diff summary; grouped file tool calls keep their group row and append the diff cell so other entries are not lost.
- The first Smooth diff renderer intentionally omits Codex's syntax-highlighting/theme stack and uses `diffy` plus ratatui styles for line counts, gutters, hunk separators, and red/green insert/delete cues.

## TUI File Change Safety

- Structured file-change tool output is a private transport and must only be decoded for successful final built-in `edit`/`write` completions; arbitrary tool stdout, dynamic tools, and opaque stream outputs stay plain text to avoid spoofed diffs.
- File-change metadata is capped at 512 KiB; oversized diffs/new-file contents and unreadable/non-UTF8 existing files use `FileChange::Omitted` so the TUI can show counts/reason without carrying or rendering large/unavailable content.
- The TUI diff renderer caps rendered diff body lines at 1,000 and appends a truncation marker, protecting transcript recalculation and scroll performance.
