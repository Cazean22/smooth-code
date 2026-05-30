# Progress

Use this file to capture concise, durable insights when a task produces knowledge worth preserving. Organize entries by topic or subsystem rather than timeline, and preserve reusable repo knowledge, implementation decisions, caveats, and verification notes; skip routine task logs.

## Repository Practices

- `PROGRESS.md` is for judgment-based durable insights worth preserving, not every task.
- Organize `PROGRESS.md` by topic or subsystem instead of chronological task history.

## OpenAI Provider

- The OpenAI provider uses Rig's Responses WebSocket session for the manual tool loop; keep OpenAI turn streaming on `stream_completion_turn` and do not route OpenAI through the generic `CompletionModel::stream()` SSE path.
- WebSocket item decoding in `crates/core/src/provider.rs` preserves provider item IDs for reasoning deltas and reuses one internal call ID across tool-call name, args, and final tool-call events so the TUI/core can correlate streamed tool state.
- The local OpenAI-compatible proxy can close/reset WebSocket sessions without a closing handshake; retry only before any assistant item is yielded, and only treat a reset as graceful after a complete output item or terminal response has been observed.
- The local OpenAI-compatible WebSocket proxy closes immediately after upgrade when `input` contains `role: system`; normalize OpenAI WebSocket completion requests so system instructions are sent as leading user-visible instruction text instead of system-role input items.
- Keep OpenAI WebSocket inbound parsing local and tolerant rather than relying on Rig's private `StreamingCompletionChunk` parser; the local proxy emits provider telemetry and some known event types with payload shapes that may not match Rig's strict structs.
- OpenAI WebSocket lifecycle responses from the local proxy may omit `output`; parse lifecycle fields (`status`, `usage`, errors, optional output IDs) from raw JSON instead of deserializing the full Responses completion object.
