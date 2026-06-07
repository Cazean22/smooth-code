# `ToolEffect` — making `spawn_agent` and `exit_plan_mode` ordinary tools

> **Status: steps 1–2 implemented** in `crates/core/src/tasks/regular.rs`. The
> tool-name match now lives only in `classify_tool` (`ToolClass`); the turn loop
> dispatches via `dispatch_tool_calls` into `DispatchedTools { immediate,
> deferred, has_immediate_results }` and surfaces deferred effects via a
> `Surfacing` enum (`BlockInline` / `GraceThenRetain`). `exit_plan_mode` and
> `spawn_agent` are no longer special-cased in the top-level flow.
>
> **Deviations from the sketch below**, forced by the real code:
> - The implemented carrier is `DispatchedTools` (two buckets + a flag), not a
>   per-call `ToolOutcome` enum. The turn loop runs tools in *phases* with
>   different cancellation rules (spawn starts are uncancellable and run first;
>   immediate tools observe cancellation), so a single "map each call to an
>   outcome" pass would have changed cancellation semantics. The phasing lives
>   in `dispatch_tool_calls`; the buckets are its output.
> - The deferred carrier stays `StartedSpawnToolCall` (it holds spawn-specific
>   data — child thread id, agent metadata, the inline-completion waiter). Only
>   the *control flow* is generalized to "deferred"; the single concrete
>   producer is still spawn. A second deferred tool would generalize the data.
> - **Steps 3–4 not done** (see below) — step 3 (`on_consumed`) buys little for
>   one producer given reclamation is already localized to the consume points;
>   step 4 changes the transcript/protocol shape and is behavior-changing, not a
>   refactor, so it should be opted into separately.

The goal is to remove the tool-name string-matching and the bespoke per-tool
code paths from `crates/core/src/tasks/regular.rs` so the turn loop dispatches
every tool the same way and only the *shape of the outcome* differs.

## The problem today

`run_manual_turn` → `execute_pending_tool_calls_for_turn` currently:

1. `partition_pending_tool_calls` splits the model's tool calls into three
   buckets by **string-matching the tool name** — `"spawn_agent"`,
   `"exit_plan_mode"`, and "normal".
2. Runs three different execution paths:
   - normal → `execute_normal_tools_concurrently` (`model.call_tool`),
   - exit-plan → `execute_exit_plan_mode_call` (mutates session: flips plan
     mode, swaps the model),
   - spawn → `start_spawn_calls_concurrently` → `wait_for_spawn_batch`
     (`SpawnWaitMode::{UntilAllComplete, GracePeriod}`) → `collect_spawn_results`,
     plus the retained-completion machinery (`RetainedSpawnCompletion`,
     `next_retained_subagent_completion`, `drain_retained_subagent_completions`)
     that surfaces late completions as synthetic `Message::User` JSON.
3. Merges the three result vectors back into model order by `index`.

Consequences:

- The `spawn_agent` tool's own `Tool::call` is a **stub that errors** — its real
  behavior lives in the loop. Knowledge of "spawn is special" is duplicated
  across the stub tool, the prompt docs, and the loop.
- Adding any third long-running tool means another partition arm and another
  path. There is no extension seam.
- `run_manual_turn` already mixes stream-retry, reasoning-delta finalization,
  spawn lifecycle, plan-mode mutation, and result ordering in one ~600-line
  unit.

## The core idea

A tool call, once executed, produces one of three **outcomes**. The loop only
needs to understand these three shapes — never tool *names*.

```rust
// crates/core/src/tasks/tool_effect.rs  (new)

/// What executing one tool call produced.
pub(crate) enum ToolOutcome {
    /// Result is ready now (read, edit, write, delete, run_command, …).
    Final(ExecutedToolCall),

    /// The tool mutated session state, then produced a result
    /// (exit_plan_mode: flip plan mode + swap model, then return text).
    /// Distinguished from `Final` only so the loop can guarantee the mutation
    /// has been applied before later calls in the same batch are interpreted.
    Mutation(ExecutedToolCall),

    /// The tool started async work. It returns an immediate "live" result for
    /// the model now, plus a future that resolves to the real completion later,
    /// plus a policy for how/when to surface that completion.
    Deferred(DeferredEffect),
}

pub(crate) struct DeferredEffect {
    /// The `result_kind = StatusUpdate` tool result shown immediately
    /// (today: the `event="agent_status"` JSON).
    pub(crate) initial: ExecutedToolCall,
    /// Resolves when the async work finishes.
    pub(crate) completion: BoxFuture<'static, DeferredCompletion>,
    pub(crate) surfacing: Surfacing,
}

pub(crate) struct DeferredCompletion {
    /// Rendered model-facing text for the finished effect
    /// (today: the `event="agent_completed"` JSON).
    pub(crate) text: String,
    /// Cleanup to run once the parent has folded `text` into history.
    /// For spawn this is `agent_control.reclaim_consumed_agent(child)`.
    pub(crate) on_consumed: Option<BoxFuture<'static, ()>>,
}

pub(crate) enum Surfacing {
    /// Block the turn until this completes, deliver as the tool result inline
    /// (today's pure-`spawn_agent`-batch behavior).
    BlockInline,
    /// Show `initial` now; if not done within `grace`, retain and surface the
    /// completion later as a follow-up message (today's mixed-batch behavior).
    GraceThenRetain { grace: Duration },
}
```

`ExecutedToolCall` already exists. `Surfacing` is exactly the information
`wait_for_spawn_batch` derives today from `has_normal_tools`, made explicit and
per-effect instead of per-batch.

## One dispatcher replaces the partition

```rust
/// The single place that knows which tools are special. Returns *how* to run
/// the call; the loop runs it uniformly. No string-matching escapes this fn.
async fn dispatch_tool_call(
    session: &Arc<Session>,
    ctx: &Arc<TurnContext>,
    pending: PendingToolCall,
) -> ToolOutcome {
    match pending.tool_call.function.name.as_str() {
        "spawn_agent"    => spawn_effect(session, ctx, pending).await,    // -> Deferred
        "exit_plan_mode" => exit_plan_effect(session, ctx, pending).await,// -> Mutation
        _                => normal_effect(session, ctx, pending).await,   // -> Final
    }
}
```

The `match` still exists — but it is now **localized to one function** whose
only job is classification, instead of being smeared across `partition_*`,
three executor families, and a merge step. Each arm is a small adapter onto the
existing helpers:

- `normal_effect` wraps `execute_normal_tool_call` → `ToolOutcome::Final`.
- `exit_plan_effect` wraps `execute_exit_plan_mode_call` → `ToolOutcome::Mutation`.
- `spawn_effect` wraps `spawn_agent_for_tool`, putting the live status in
  `initial`, the `InlineChildCompletionReceiver` (mapped to text) in
  `completion`, `reclaim_consumed_agent` in `on_consumed`, and choosing
  `Surfacing` from whether the batch has any non-deferred calls.

## The generic loop

`execute_pending_tool_calls_for_turn` collapses to:

```rust
// 1. Dispatch all calls. Calls that must *start* atomically (spawn's side
//    effects) do so here; this join is not cancelled mid-flight, exactly like
//    today's start_spawn_calls_concurrently.
let outcomes: Vec<ToolOutcome> =
    join_all(pending.into_iter().map(|p| dispatch_tool_call(&session, &ctx, p))).await;

// 2. Split by shape (not by name).
let (mut ready, deferred): (Vec<_>, Vec<_>) = partition_by_shape(outcomes);
//    ready   = Final + Mutation results, already executed.
//    deferred = DeferredEffect list.

// 3. Apply surfacing uniformly. `pending_effects` replaces `retained_subagents`
//    and is a property of *deferred effects*, not of spawn.
let batch_has_ready = !ready.is_empty();
for d in deferred {
    match d.surfacing {
        Surfacing::BlockInline if !batch_has_ready => block_until_done(d, &mut ready, &cancel).await?,
        _ /* GraceThenRetain or BlockInline-with-ready */ =>
            grace_then_retain(d, &mut ready, pending_effects, &cancel).await?,
    }
}

// 4. Drain any already-retained effects whose results the model should see now.
//    Same select! the loop uses today, but over `pending_effects` generically.
Some(ready_sorted_by_index)
```

`run_manual_turn` keeps its existing `tokio::select!` that races
`pending_effects` against the model stream — but it now races *deferred tool
effects* in the abstract, with no mention of subagents. When a
`DeferredCompletion` is drained, the loop runs its `on_consumed` cleanup; that
is where reclamation lives, instead of being threaded through
`collect_spawn_results` / `next_retained_subagent_completion` /
`drain_retained_subagent_completions` separately (the three sites the current
focused fix had to touch).

## What maps onto what

| Today | After |
|---|---|
| `partition_pending_tool_calls` (name match ×3) | `dispatch_tool_call` (name match ×1, returns outcome) |
| `execute_normal_tools_concurrently` | `normal_effect` → `Final` |
| `execute_exit_plan_mode_call` | `exit_plan_effect` → `Mutation` |
| `start_spawn_calls_concurrently` + `wait_for_spawn_batch` + `collect_spawn_results` | `spawn_effect` → `Deferred` + the generic surfacing step |
| `SpawnWaitMode::{UntilAllComplete, GracePeriod}` | `Surfacing::{BlockInline, GraceThenRetain}` |
| `RetainedSpawnCompletion` + `next_retained_subagent_completion` + `drain_retained_subagent_completions` | generic `pending_effects: Vec<PendingDeferred>` + one drain helper |
| reclamation scattered at 3 consume sites | `DeferredCompletion::on_consumed` (one site) |
| `spawn_agent` stub `Tool::call` returning "unsupported" | unchanged (definition-only); `spawn_effect` is the source of truth |

## Migration (incremental, each step keeps the e2e suite green)

The turn loop is concurrency-sensitive and the `multi_agent_e2e` tests pin the
exact ordering, grace-period, and retained-completion semantics. Land this in
behavior-preserving steps, not a big bang:

1. **Introduce the types + `dispatch_tool_call`** that internally calls the
   *existing* helpers and returns `ToolOutcome`. Have
   `execute_pending_tool_calls_for_turn` consume outcomes but keep today's
   branching underneath. No behavior change; tests stay green.
2. **Generalize surfacing**: move the `UntilAllComplete`/`GracePeriod` choice
   into `Surfacing`, replace `retained_subagents` with `pending_effects`, and
   delete `partition_pending_tool_calls`. Re-run e2e.
3. **Fold reclamation into `on_consumed`**, removing the three scattered
   `reclaim_consumed_agent` calls added by the focused fix.
4. **(Optional) Replace synthetic `Message::User` delivery** of deferred
   completions with a dedicated input role / `EventMsg` so async tool
   completions are no longer conflated with user speech in the transcript.

## Trade-offs / why this is a sketch

- **Pro:** one dispatch path; new long-running tools (e.g. a streaming shell
  command) reuse `Deferred` for free; reclamation and "live status now,
  completion later" become reusable mechanisms instead of spawn-only code;
  `run_manual_turn` stops knowing what a subagent is.
- **Con:** a layer of indirection for what is currently two special tools. The
  win comes specifically because there are *already two* and the loop is hard to
  read — not from speculative generality. If the set of special tools were ever
  going to stay at one, the abstraction would not pay for itself.
- **Risk:** `Surfacing` must reproduce the current pure-batch-vs-mixed-batch
  timing exactly (the `grace = 1s`, the "pure spawn batch blocks on retained
  receivers too" rule). Step 2 is the delicate one; keep
  `mixed_spawn_and_normal_tool_results_preserve_model_order`,
  `spawn_agent_waits_for_two_children_and_finishes_in_same_parent_turn`, and
  `retained_subagents_all_finish_before_parent_continues` as the guardrails.
