**In smooth-code today: no.** The HashMap is populated at exactly one site — `crates/app-server/src/in_process.rs:64-67`:

```rust
outbound_connections.insert(
    IN_PROCESS_CONNECTION_ID,        // ConnectionId(0)
    OutboundConnectionState::new(writer_tx, None),
);
```

That insert runs once at runtime startup; nothing else produces `OutboundConnectionState`s. So the HashMap always has size 1 in this codebase — it's strictly plumbing for parity with codex.

**Where multi-entry actually happens — codex.** The same data structure in codex is shared across three transports, each of which calls `next_connection_id()` and registers an entry whenever a peer attaches:

| Transport | Insertion site | When it fires |
|---|---|---|
| stdio | `codex-rs/app-server/src/transport/stdio.rs:28` | At process start, one entry for `stdin`/`stdout`. |
| websocket | `codex-rs/app-server/src/transport/websocket.rs:114` | Each accepted WS connection (every desktop/web/IDE client gets its own). |
| remote control | `codex-rs/app-server/src/transport/remote_control/client_tracker.rs:157` | Every remote-control peer that joins via the control plane. |

So a typical codex session running in app-server mode could have, say, 1 stdio + 2 attached websocket clients + 1 remote-control peer = 4 entries in the same HashMap. `OutgoingEnvelope::Broadcast` then fans the same `OutgoingMessage` out to all four; `OutgoingEnvelope::ToConnection { connection_id, ... }` picks one (e.g., the websocket peer that originally issued the request whose response we're sending).

**For smooth-code to grow past 1:** you'd need to add a non-in-process transport — e.g., stdout/stdin JSONL for embedding via subprocess, or a websocket server for a separate UI process. The current `route_outgoing_envelope` / `OutboundConnectionState` machinery would handle multi-entry as-is; only the runtime startup needs to register additional `IN_PROCESS_CONNECTION_ID + N` entries (or use `next_connection_id()` like codex does).
