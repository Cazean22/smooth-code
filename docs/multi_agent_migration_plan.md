# Multi-Agent Migration Plan

Port codex's hierarchical multi-agent system into `smooth-code` while preserving `smooth-code`'s existing layering:

`tui -> app-server -> core -> protocol`

This plan is written against the current `smooth-code` codebase:

- `ThreadManagerState` in `crates/core/src/thread_manager.rs` owns a flat `HashMap<ThreadId, Arc<CoreThread>>`
- `Op` in `crates/protocol/src/lib.rs` only supports `UserInput(String)`
- `Core::start_user_input` is the only turn submission entry point
- persistence today is rollout JSONL under `.smooth-code/sessions/...`
- model construction is hard-wired through `SessionModel::from_env(...)`

The target is codex-style hierarchical multi-agent support with:

- hierarchical `AgentPath` addressing like `/root/foo/bar`
- inter-agent messaging through a mailbox
- five model-facing tools: `spawn_agent`, `send_message`, `wait_agent`, `list_agents`, `close_agent`
- built-in roles: `default`, `explorer`, `worker`
- optional fork-from-parent-history on spawn
- completion watchers that notify the parent when a child reaches terminal state
- persisted spawn edges so resuming a root thread rehydrates its live subtree

The multi-agent surface in this migration is model-facing tools only. No new `ClientRequest` variants and no TUI slash commands are added.

## Constraints

### Child event visibility

v1 keeps child activity parent-visible-only:

- app-server does not auto-subscribe to spawned children
- the parent sees `Collab*Begin/End` tool events
- child completion reaches the parent through an inter-agent mailbox notification
- `wait_agent` and completion watchers use internal status subscriptions only

This preserves current TUI behavior and avoids an immediate UI redesign. A future `SubscribeThread(thread_id)` RPC can add opt-in child visibility later.

### Mailbox model

This migration does not port codex's full mailbox semantics.

v1 uses a simplified turn-boundary-only mailbox:

- mailbox drains exactly once per turn, at the start of `RegularTask::run`
- there is no mid-turn drain
- `MailboxDeliveryPhase` is deleted
- trigger-turn signals are coalesced at task boundaries

This removes mailbox delivery duplication and reordering risk. It does not preserve codex's one-follow-up-turn-per-send behavior.

### Resume parity scope

Resume parity here means parity with codex's resume loop shape and failure handling:

- breadth-first traversal of open child edges
- per-child `try / warn / continue`
- no auto-close on missing rollout

It does not mean bit-for-bit parity with codex runtime internals, config objects, or submission loop structure.

## Architecture mapping

| Codex | Smooth-code target |
| --- | --- |
| `protocol/src/agent_path.rs` | `crates/protocol/src/agent_path.rs` |
| `SessionSource`, `SubAgentSource`, `InterAgentCommunication`, `AgentStatus` additions | extend `crates/protocol/src/lib.rs` |
| `Op::InterAgentCommunication`, `Interrupt`, `Shutdown` | extend `Op` |
| `EventMsg::Collab*`, `InterAgentMessage` | extend `EventMsg` |
| `core/src/agent/mod.rs` | `crates/core/src/agent/mod.rs` |
| `registry.rs` | `crates/core/src/agent/registry.rs` |
| `control.rs` | `crates/core/src/agent/control.rs` |
| full codex mailbox | `crates/core/src/agent/mailbox.rs`, simplified turn-boundary-only model |
| `role.rs` | `crates/core/src/agent/role.rs` |
| `status.rs` | `crates/core/src/agent/status.rs` |
| `agent_resolver.rs` | `crates/core/src/agent/agent_resolver.rs` |
| rollout state DB | new `crates/state-db/` crate |
| fork logic | `crates/core/src/agent/fork.rs` |
| multi-agent tools | `crates/tools/src/multi_agents/*.rs` |
| `MailboxDeliveryPhase` | delete smooth-code's mirrored copy |

## Structural differences from codex

1. `smooth-code` has no submission loop. Add `Core::submit(op: Op) -> Result<String>` and route existing `start_user_input` through it.
2. `smooth-code` uses `rig::Tool` directly. Multi-agent tools should live in `crates/tools` and depend on a new `MultiAgentClient` trait.
3. `smooth-code` has no config-layer stack. Roles only override optional preamble and model selection.

## Phase order

| # | Phase |
| --- | --- |
| 1 | Protocol foundations + `Core::submit(Op)` |
| 2 | `SessionModelFactory` test seam |
| 3 | Agent skeleton: registry, mailbox, status, resolver |
| 4 | `ThreadManagerState` / `CoreThread` plumbing |
| 5 | Minimal spawn end-to-end |
| 6 | Mailbox delivery + `Op::InterAgentCommunication` |
| 7 | Completion watcher |
| 8 | Roles |
| 9 | Fork from parent history |
| 10 | SQLite state DB |
| 11 | Subtree resume |
| 12 | Tool surface + end-to-end verification |

## Phase 1: Protocol foundations + `Core::submit(Op)`

Goal: add shared wire types and an op-dispatch entry point without changing current behavior.

Add `crates/protocol/src/agent_path.rs` by porting codex's `AgentPath` and re-exporting it from `protocol::lib`.

Extend `crates/protocol/src/lib.rs` with:

```rust
pub enum SessionSource {
    Cli,
    SubAgent(SubAgentSource),
}

pub enum SubAgentSource {
    Review,
    ThreadSpawn {
        parent_thread_id: ThreadId,
        depth: i32,
        agent_path: Option<AgentPath>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    },
}

impl SessionSource {
    pub fn get_agent_path(&self) -> Option<AgentPath> { ... }
    pub fn get_nickname(&self) -> Option<String> { ... }
    pub fn get_agent_role(&self) -> Option<String> { ... }
}

pub struct InterAgentCommunication {
    pub author: AgentPath,
    pub recipient: AgentPath,
    pub attachments: Vec<String>,
    pub content: String,
    pub trigger_turn: bool,
}
```

Extend `Op`:

```rust
pub enum Op {
    UserInput(String),
    InterAgentCommunication { communication: InterAgentCommunication },
    Interrupt,
    Shutdown,
}
```

Extend `EventMsg` with:

- `CollabAgentSpawnBegin`
- `CollabAgentSpawnEnd`
- `CollabSendMessageBegin`
- `CollabSendMessageEnd`
- `CollabWaitingBegin`
- `CollabWaitingEnd`
- `CollabCloseBegin`
- `CollabCloseEnd`
- `CollabResumeBegin`
- `CollabResumeEnd`
- `InterAgentMessage(InterAgentCommunicationEvent)`

Add `Core::submit(&self, op: Op) -> anyhow::Result<String>` in `crates/core/src/core.rs`:

- `UserInput` executes existing turn-start behavior
- `InterAgentCommunication` bails until Phase 6
- `Interrupt` aborts all tasks with reason `"interrupted"`
- `Shutdown` aborts all tasks with reason `"shutdown"` and sets `AgentStatus::Shutdown`

Rewrite `start_user_input` to call `submit(Op::UserInput(...))`.

Tests:

- port `agent_path` tests
- add serde round-trip tests for `Op`
- compare event sequence from old `start_user_input` vs `submit(UserInput(...))`

## Phase 2: `SessionModelFactory` test seam

Goal: make model construction injectable so multi-agent tests do not depend on real providers.

Add a `SessionModelFactory` trait and default `EnvSessionModelFactory` in `provider.rs` or `provider/factory.rs`:

```rust
pub trait SessionModelFactory: Send + Sync {
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        current_turn_id: Arc<watch::Sender<Option<String>>>,
    ) -> anyhow::Result<SessionModel>;
}
```

Plumb factory injection through:

- `ThreadManagerState::new(...)`
- `CoreThread::new(...)`
- `CoreThread::resume(...)`

Promote these from `pub(crate)` to `pub` for test support:

- `SessionModel`
- `SessionStreamEvent`
- `SessionAssistantContent`
- `SessionStream`

Add `crates/core/src/test_support.rs` with a `StubSessionModelFactory` keyed by `ThreadId`.

Tests:

- `EnvSessionModelFactory` returns equivalent models to prior `SessionModel::from_env`

## Phase 3: Agent skeleton

Goal: land the data structures and contracts with no user-visible behavior changes.

Create:

- `crates/core/src/agent/mod.rs`
- `registry.rs`
- `mailbox.rs`
- `status.rs`
- `agent_resolver.rs`

Wire `mod agent;` into `crates/core/src/lib.rs`.

### Registry

Port codex registry with `smooth-code` adaptations:

- no OTel counters
- keep nickname pool via copied `agent_names.txt`
- expose `reserve_spawn_slot`, `register_root_thread`, `agent_id_for_path`, `agent_metadata_for_thread`, `live_agents`, `next_thread_spawn_depth`, depth-limit helpers, `SpawnReservation`, `AgentMetadata`

### Mailbox

Use a simplified turn-boundary-only design.

```rust
pub(crate) struct Mailbox {
    tx: mpsc::UnboundedSender<InterAgentCommunication>,
    pending_trigger_turn: Arc<AtomicI64>,
}

pub(crate) struct MailboxReceiver {
    rx: mpsc::UnboundedReceiver<InterAgentCommunication>,
    pending_trigger_turn: Arc<AtomicI64>,
}
```

Contract:

- `send()` increments `pending_trigger_turn` before enqueue if `trigger_turn == true`
- `try_drain_all()` drains FIFO order and decrements by the number of drained trigger messages
- `has_trigger_turn_pending()` returns `pending_trigger_turn > 0`
- there is no `has_pending()` API

This avoids the lost-wakeup bug an `AtomicBool` design would have.

### Status

Add:

```rust
pub fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> { ... }
pub fn is_final(status: &AgentStatus) -> bool { ... }
```

### Resolver contract

Define stable semantics now because later tools depend on them.

`target` resolution:

1. parse as `ThreadId` first; if successful, return it
2. otherwise resolve as an `AgentPath` reference from:
   - current agent path if present
   - `/root` for CLI/root session
3. if valid but not live, return model-visible `live agent path not found`
4. validation failures are also model-visible and do not abort the parent turn

`list_agents(path_prefix)` contract:

- `None` means no filter
- empty string is invalid
- absolute prefixes ignore current path
- relative prefixes resolve from current path or `/root`
- match means exact path or starts with `<prefix>/`
- `/root` matches all
- empty result is not an error

Tests:

- port registry tests
- mailbox send/drain ordering tests
- resolver tests covering the contract examples

## Phase 4: `ThreadManagerState` / `CoreThread` plumbing

Goal: thread `SessionSource`, `AgentControl`, mailbox, and future spawn control through construction.

In `crates/core/src/thread_manager.rs`:

- wrap internal manager state in `Arc`
- add `agent_control() -> AgentControl`
- add `spawn_new_thread_with_source(...)`
- add `send_op(thread_id, op)`
- add rollback helpers for later phases:

```rust
pub(crate) async fn remove_thread(&self, thread_id: ThreadId) -> Option<Arc<CoreThread>>;
pub(crate) async fn shutdown_and_remove_thread(
    &self,
    thread_id: ThreadId,
    reason: &str,
) -> anyhow::Result<()>;
```

Contracts:

- `remove_thread` only removes from the manager map
- `shutdown_and_remove_thread` aborts/shuts down the child first, then removes it

In `core_thread.rs`:

- extend `new` and `resume` to accept `SessionSource`, `AgentControl`, and model factory

In `core.rs`, add to `Session`:

- `session_source: SessionSource`
- `agent_control: AgentControl`
- `mailbox: Mailbox`
- `mailbox_rx: Mutex<Option<MailboxReceiver>>`

Add a skeletal `AgentControl` type with:

- `new`
- `register_session_root`
- `get_status`
- `subscribe_status`
- remaining methods stubbed until Phase 5

Tests:

- thread manager still constructs
- `agent_control()` clones refer to the same manager state

## Phase 5: Minimal spawn end-to-end

Goal: spawn a child programmatically and let it complete, still without model-facing tools.

Implement in `AgentControl`:

- `spawn_agent`
- `spawn_agent_with_metadata`
- `spawn_agent_internal`
- `send_input`
- `interrupt_agent`
- `shutdown_live_agent`
- `close_agent`
- `live_agents`
- `list_agents`
- `get_agent_metadata`
- `resolve_agent_reference`
- `format_environment_context_subagents`

Skip for now:

- fork behavior
- role overrides
- completion watcher
- DB persistence

### Spawn sequence

Order matters:

1. `reserve_spawn_slot(...)`
2. build `SessionSource::SubAgent(SubAgentSource::ThreadSpawn { ... })`
3. `spawn_new_thread_with_source(...)`
4. send initial op before commit or persistence
5. if initial send fails, roll back by shutting down and removing the thread
6. only then commit the reservation into the registry
7. later phases insert DB writes and completion watcher hookup here

Reference pseudocode:

```rust
let reservation = state.reserve_spawn_slot(max_threads)?;
let child_source = prepare_thread_spawn(...)?;
let new_thread = state.spawn_new_thread_with_source(child_source, self.clone(), ...).await?;

if let Err(err) = state.send_op(new_thread.thread_id, initial_op).await {
    tracing::warn!(
        thread_id = %new_thread.thread_id,
        error = %err,
        "initial op submission failed; rolling back spawn"
    );
    let _ = state
        .shutdown_and_remove_thread(new_thread.thread_id, "spawn_rollback")
        .await;
    return Err(err);
}

agent_metadata.agent_id = Some(new_thread.thread_id);
reservation.commit(agent_metadata.clone());
```

Invariant:

- no `reservation.commit(...)`
- no `state_db.upsert_*`

before initial `send_op` succeeds.

Constants:

- `AGENT_MAX_DEPTH: i32 = 8`
- `AGENT_MAX_THREADS: usize = 16`

Tests:

- successful spawn reaches `Completed(...)`
- child gets a non-root `AgentPath`
- `live_agents()` reflects one child

Regression test:

- force initial `send_op` failure
- assert spawn returns an error
- assert no live child remains in the registry
- assert thread manager no longer contains the child
- after Phase 10, assert no DB edge was persisted

## Phase 6: Mailbox delivery + `Op::InterAgentCommunication`

Goal: deliver inter-agent communication on the next turn boundary, never mid-turn.

Delete `MailboxDeliveryPhase` from `crates/core/src/state/turn.rs`.

Keep `TurnState` mailbox-free.

### Delivery semantics

`RegularTask::run` must drain the mailbox exactly once at the very start, before the first model call.

For each drained message, prepend a rendered synthetic user message such as:

```text
<inter_agent_message from="/root/parent">...</inter_agent_message>
```

Then proceed with the turn as normal.

### Idle trigger path

When `Core::submit(Op::InterAgentCommunication { communication })` sees:

- `trigger_turn == true`
- no active turn

it must allocate a fresh turn exactly like `start_user_input`:

- `sub_id = session.next_internal_sub_id()`
- `assistant_item_id = format!("{sub_id}-assistant")`
- `TurnContext { sub_id, assistant_item_id, timezone: None }`

then call:

```rust
session.start_task(turn_context, vec![String::new()], RegularTask::new()).await;
```

Empty input is intentional because mailbox drain provides the effective prompt content.

### Submission behavior

`Core::submit(Op::InterAgentCommunication { ... })`:

- always enqueues in the mailbox
- if idle and `trigger_turn == true`, starts an empty-input regular turn
- if busy and `trigger_turn == true`, does nothing immediately
- if `trigger_turn == false`, only enqueues

### Post-turn check

After task cleanup in `Session::start_task`, do:

```rust
if mailbox.has_trigger_turn_pending() {
    start_task(fresh_turn_context, vec![String::new()], RegularTask::new()).await;
}
```

Guarantee:

- at most one follow-up turn per completed task while trigger mail is pending
- not one follow-up per send

`AgentControl::send_inter_agent_communication(...)` dispatches via `send_op(..., Op::InterAgentCommunication { ... })`.

Tests:

- multiple queued messages appear exactly once in FIFO order on the next turn
- idle `trigger_turn=true` starts a turn
- busy `trigger_turn=true` causes one follow-up turn
- contention regression proving no lost wake-up with the `AtomicI64` counter

## Phase 7: Completion watcher

Goal: notify the parent when the child reaches final state.

Port a simplified `maybe_start_completion_watcher`:

- subscribe to the child status watch channel
- wait until `is_final(status)`
- format a notification string in `agent/notify.rs`
- send the parent an `InterAgentCommunication` with `trigger_turn = false`

Hook it into `spawn_agent_internal` after registry commit and, once Phase 10 lands, after persistence.

Tests:

- child completion enqueues a parent notification

## Phase 8: Roles

Goal: allow spawned children to use built-in role-specific preambles and model overrides.

Add `agent/role.rs` with:

- `RoleConfig`
- builtin roles: `default`, `explorer`, `worker`
- `resolve_role(...)`
- `render_spawn_agent_tool_description()`

Extend `SessionModel::from_env` to accept:

```rust
RoleOverride {
    preamble: Option<String>,
    model: Option<String>,
}
```

Precedence:

- role override
- env var
- default

Add `agent_role` to `AgentMetadata`.

Break the `core -> tools -> core` dependency cycle by rendering role documentation in core and passing the final string into the tool constructor.

Tests:

- each built-in role resolves
- role override affects preamble/model as seen by the test model factory

## Phase 9: Fork from parent history

Goal: optionally seed child history from parent rollout.

Add `agent/fork.rs`:

- `SpawnAgentForkMode`
- `keep_forked_rollout_item(...)`
- `truncate_to_last_n_user_turns(...)`

Add rollout support:

- `read_persisted_items(path)`
- `RolloutRecorder::flush()`

Extend the thread manager with `fork_thread_with_source(...)`.

Extend `SpawnAgentOptions` with an optional fork mode.

Fork algorithm:

1. flush the parent rollout
2. read persisted items
3. truncate if needed
4. filter to user/assistant history only
5. convert into `Vec<Message>`
6. spawn the child with seeded history

Tests:

- child history matches filtered parent history

## Phase 10: SQLite state DB

Goal: persist thread metadata and open/closed spawn edges in a new workspace crate.

This is an architectural persistence change, not just a runtime addition.

### Pre-flight

Edit `smooth-code/AGENTS.md` to retire the no-SQL rule and document `.smooth-code/state.db`.

### New crate

Create `crates/state-db/` with:

- `Cargo.toml`
- `src/lib.rs`
- `src/error.rs`
- `src/handle.rs`
- `migrations/0001_initial.sql`

Schema:

- `threads(thread_id, agent_path, agent_nickname, agent_role, created_at, updated_at)`
- `thread_spawn_edges(parent_thread_id, child_thread_id, status, created_at, updated_at)`

Use `SqliteConnectOptions::filename(PathBuf)` rather than hand-built SQLite URLs.

Implement methods:

- `open`
- `upsert_thread`
- `get_thread`
- `upsert_thread_spawn_edge`
- `set_thread_spawn_edge_status`
- `list_thread_spawn_children_with_status`

UPSERT semantics must preserve `created_at` on conflict.

### Runtime integration

Make `ThreadManagerState::new(...)` async and open `StateDbHandle`.

Propagate async construction into `crates/app-server/src/core_message_processor.rs`.

Persist root thread rows in `start_thread` and `resume_thread`.

Persist child thread rows and open edges in `spawn_agent_internal`, but only after the initial op succeeded and the registry commit happened.

Mark an edge closed in `close_agent` before shutdown.

Tests:

- migrations are idempotent
- Unicode / space path open works
- edge round-trip works
- `created_at` preservation regression test
- root-thread row exists with `agent_path = NULL`
- concurrent edge upserts avoid `SQLITE_BUSY`

## Phase 11: Subtree resume

Goal: rehydrate the live subtree by walking persisted open child edges.

After existing root resume logic, BFS over open edges:

1. query open children by parent
2. for each child:
   - enforce depth limit
   - try resume from rollout
   - on failure, warn and continue with no auto-close
   - on success, push into the queue

On resumed children, rebuild `SessionSource::SubAgent(SubAgentSource::ThreadSpawn { ... })` using DB metadata.

Emit `CollabResumeBegin/End` for each resumed child.

Tests:

- three-deep subtree rehydrates
- closed child does not rehydrate
- missing rollout warns and leaves the edge open
- edge marked closed before shutdown prevents rehydration after crash

## Phase 12: Model-facing tools + end-to-end verification

Goal: let the model drive multi-agent behavior autonomously.

Add `crates/tools/src/multi_agents/`:

- `client.rs`
- `spawn_agent.rs`
- `send_message.rs`
- `wait_agent.rs`
- `list_agents.rs`
- `close_agent.rs`

`MultiAgentClient` methods:

- `spawn`
- `send_message`
- `wait_agent`
- `list_agents`
- `close_agent`

Implement tool schemas with `schemars::JsonSchema`.

Tool contracts:

- `spawn_agent`: `message`, optional `agent_type`, optional `model`, optional `fork_context`
- `send_message`: `target`, `content`, optional `trigger_turn`
- `wait_agent`: `target`, optional timeout
- `list_agents`: optional `path_prefix`, using the Phase 3 prefix contract
- `close_agent`: `target`

Create `agent/multi_agent_client.rs` in core as the in-process adapter implementing `MultiAgentClient` on top of `AgentControl`.

Conditionally register these tools in `provider.rs::build_agent(...)`.

Tests:

- tool definition snapshots
- adapter round-trip tests with a mock client
- smoke test: model emits `spawn_agent`, then `wait_agent`
- end-to-end test in `crates/core/tests/multi_agent_e2e.rs`

## Files to create

- `crates/protocol/src/agent_path.rs`
- `crates/core/src/agent/mod.rs`
- `crates/core/src/agent/agent_names.txt`
- `crates/core/src/agent/registry.rs`
- `crates/core/src/agent/mailbox.rs`
- `crates/core/src/agent/status.rs`
- `crates/core/src/agent/agent_resolver.rs`
- `crates/core/src/agent/control.rs`
- `crates/core/src/agent/role.rs`
- `crates/core/src/agent/fork.rs`
- `crates/core/src/agent/notify.rs`
- `crates/core/src/agent/multi_agent_client.rs`
- `crates/state-db/Cargo.toml`
- `crates/state-db/src/lib.rs`
- `crates/state-db/src/error.rs`
- `crates/state-db/src/handle.rs`
- `crates/state-db/migrations/0001_initial.sql`
- `crates/state-db/tests/migration.rs`
- `crates/tools/src/multi_agents/mod.rs`
- `crates/tools/src/multi_agents/client.rs`
- `crates/tools/src/multi_agents/spawn_agent.rs`
- `crates/tools/src/multi_agents/send_message.rs`
- `crates/tools/src/multi_agents/wait_agent.rs`
- `crates/tools/src/multi_agents/list_agents.rs`
- `crates/tools/src/multi_agents/close_agent.rs`
- `crates/core/tests/multi_agent_e2e.rs`

## Existing files to modify

- `smooth-code/AGENTS.md`
- `smooth-code/Cargo.toml`
- `crates/protocol/src/lib.rs`
- `crates/core/Cargo.toml`
- `crates/core/src/lib.rs`
- `crates/core/src/provider.rs`
- `crates/core/src/thread_manager.rs`
- `crates/core/src/core_thread.rs`
- `crates/core/src/core.rs`
- `crates/core/src/state/turn.rs`
- `crates/core/src/tasks/regular.rs`
- `crates/core/src/rollout.rs`
- `crates/app-server/src/core_message_processor.rs`
- `crates/app-server/src/in_process.rs`
- `crates/tools/src/lib.rs`

## Reuse, do not reimplement

- `rollout.rs::list_threads`
- `RolloutRecorder::append`
- `DynamicToolClient` pattern
- app-server in-process dynamic-tool client pattern
- `ThreadId`
- codex `codex_rollout::state_db` API shape

## Verification

Run after Phase 12:

```bash
cd smooth-code
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace

cargo test -p smooth-protocol agent_path
cargo test -p smooth-core agent::registry
cargo test -p smooth-core agent::mailbox
cargo test -p smooth-core agent::control
cargo test -p smooth-core agent::role
cargo test -p smooth-state-db
cargo test -p smooth-core --test multi_agent_e2e
cargo test -p tools multi_agents
```

Manual smoke:

```bash
SMOOTH_CODE_LLM_PROVIDER=openai \
SMOOTH_CODE_LLM_MODEL=gpt-5.4 \
cargo run -p smooth-tui
```

Prompt:

```text
Use spawn_agent (agent_type=explorer) to find the README.md in this workspace, then wait for it and tell me what you found.
```

Resume smoke:

1. spawn two children, one with a grandchild
2. quit
3. inspect `.smooth-code/state.db`
4. resume root
5. verify subtree rehydrates
6. delete one child rollout
7. verify warn + edge remains open

## Out of scope

- app-server RPC for multi-agent
- TUI slash commands
- user-defined roles
- sandbox or guardian systems
- MCP
- unrelated `Op` variants from codex
- realtime conversation
- telemetry counters
