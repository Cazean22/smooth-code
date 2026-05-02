# Agent Family — Design

This document specifies the agent family architecture for the code assistant. It is intended to be concrete enough to hand to an engineer and start building.

## 1. Core concepts

**Agent.** A self-contained, reusable unit defined independently of any family. Has identity, declared dependencies, declared I/O, a prompt, and a permission profile. Within a running task, an agent is a persistent stateful instance (actor) — not a stateless function.

**Family.** A closed assembly of agents wired together for a specific stance (e.g. `default-coder`, `tdd-first`). The family is the unit of replacement at configuration time. Most agents in a family are reusable across families; some can be family-private.

**Task.** One coherent piece of work, possibly spanning multiple user turns. Has a task ID, a context envelope, a trace, and a set of agent instances scoped to its lifetime.

**Tool.** An external capability available to agents — file read, edit, exec, network, etc. Tools are shared globally; access is gated per-agent.

**Inter-agent calls are tool calls.** When agent A invokes agent B, it goes through the same tool-dispatch pipeline as any other tool. B's full transcript is hidden; only B's result envelope returns.

## 2. Agent definition

Each agent lives in its own definition (file, registry entry, etc.):

```
Agent {
  id:           string             # stable identifier, e.g. "exploration"
  version:      semver
  description:  string             # what it does, when to use it (LLM-readable)

  prompt:       string             # system prompt
  model_hint:   string?            # default model class (optional override at family level)

  inputs:       JSONSchema         # schema for invocation args
  outputs:      JSONSchema         # schema for result envelope's `result` field

  requires_tools:    [tool_id]     # tools this agent declares it needs
  requires_agents:   [agent_id]    # sub-agents this agent declares it needs

  tool_policy: {                   # agent's self-declared restrictions
    deny_tools:      [tool_id],
    deny_categories: [category_id],
    mode:            "read-only" | "default"   # shorthand
  }

  state_policy: {                  # token-growth strategy for retained context
    max_context_tokens: int,
    strategy: "self-summarize" | "sliding-window",
    summarize_threshold: int       # for self-summarize
  }
}
```

Notes:

- `requires_tools` / `requires_agents` are validated at family load time. If a family denies a required tool or omits a required sub-agent, family load fails with a clear error.
- `tool_policy.mode: "read-only"` is shorthand that expands to denying every category that mutates (`edit`, `exec`, `git-write`, etc.).
- Agents are independent of any family. The same `exploration` agent can appear in multiple families.

## 3. Family definition

```
Family {
  id:             string                  # e.g. "default-coder"
  version:        semver
  description:    string

  entry_point:    agent_id                # the orchestrator
  user_callable:  [agent_id]              # agents the host/user can invoke directly
  roster:         [agent_id]              # all agents in this family, by reference

  wiring: {                               # who-can-call-whom
    [caller_agent_id]: [callee_agent_id]
  }

  permissions: {                          # family overlay on tool policy
    granted_tools: [tool_id]              # family's overall budget (subset of host budget)
    per_agent: {
      [agent_id]: {
        deny_tools: [tool_id],
        deny_categories: [category_id]
      }
    }
  }

  overrides: {                            # per-agent prompt/config tweaks (escape hatch)
    [agent_id]: {
      prompt_suffix: string?,
      model_hint:    string?,
      state_policy:  {...}?
    }
  }

  external_contract: {                    # what's exposed outside the family
    io_format:      "text" | "structured-events",
    capabilities:   [agent_id]            # which user_callable names are part of the contract
  }
}
```

Key rules:

- `entry_point` must be in `roster`.
- All agents in `wiring` (callers and callees) must be in `roster`.
- `user_callable` is a subset of `roster`.
- Family load runs static validation (see §11).

## 4. Tool system

### 4.1 Tools and categories

A **tool** is the leaf primitive. Tools belong to one or more **tool categories** (registered globally, not per-family). Categories exist for ergonomic permission rules.

Standard categories (initial):

- `read` — read files, list directories, show structure
- `edit` — write, modify, delete files
- `exec` — run commands, shell access
- `network` — fetch external resources
- `git-read` — git status, log, diff
- `git-write` — git commit, push, branch ops
- `meta` — agent invocation, scheduling, etc.

Each tool declares which categories it belongs to. Categories are referenced in deny rules.

Note: this is "tool **category**," not "tool **family**" — reserve "family" for agent families to avoid the naming collision.

### 4.2 Permission layers (strictest wins)

Three layers stack:

1. **Host budget.** When a family is loaded, the host grants it a set of tools/categories. Family cannot grant beyond this.
2. **Family overlay.** Per-agent denies in `family.permissions.per_agent`. This is *policy*, not identity.
3. **Agent self-declaration.** `agent.tool_policy`. This is *identity* — the agent's own statement of what it never needs.

A tool is allowed for an agent iff it is granted at all three layers. Any layer can deny; no layer can override another's deny. **A family cannot relax an agent's self-declared deny.**

### 4.3 Workspace scoping

Path-scoped restrictions are *not* expressed as per-agent policy. They're enforced at the tool implementation:

- Host knows the workspace root.
- Every read/edit tool resolves paths to absolute canonical form, refuses symlinks across the boundary, refuses `..` traversal outside the workspace.
- This is invariant for the tool, not configurable per agent.

This keeps the per-agent permission model simple (allow/deny by tool or category, no arguments) while still preventing escape.

### 4.4 Sub-tool restrictions: don't

Don't build "bash but no rm" or "edit but only in scratch/" as a general capability DSL. Instead, ship narrower tools (`run-tests`, `format`, `git-status`). When the urge to restrict arguments arises, that's a signal to add a more specific tool.

## 5. Inter-agent invocation (agents-as-tools)

### 5.1 Mechanism

Agent invocation is dispatched through the same pipeline as tool calls. From the calling agent's perspective:

```
result = call_tool("exploration", { query: "...", paths: ["src/"] })
// result is the result envelope (§6)
```

The framework:

1. Checks the caller's permissions (is `exploration` in caller's allowed agents per family wiring?)
2. Looks up or instantiates the `exploration` agent (per-task instance, §8)
3. Routes the call as a new message in `exploration`'s conversation
4. Awaits the result envelope
5. Returns the envelope to the caller

### 5.2 Visibility rule

**The caller sees only the result envelope. Never the callee's internal transcript.**

The callee's full conversation stays in the callee's context (and in the trace). The caller's context grows by one envelope per call, not by the callee's transcript. This is the rule that makes the architecture viable past 2-3 levels of delegation.

If a caller needs to debug a callee's reasoning, that's an observability concern, not a context concern.

### 5.3 Tool descriptor for an agent

When the framework presents an agent as a tool to a calling LLM, the descriptor is:

```
{
  name:         agent.id,
  description:  agent.description,
  input_schema:  agent.inputs,
  output_schema: <result_envelope_schema>   # see §6
}
```

### 5.4 Dual-callable agents

A specialist like `planner` can be both user-callable (in `family.user_callable`) and tool-callable (in some other agent's `wiring` allowlist). It's the same agent definition either way. The agent does not need to know which path called it.

## 6. Result envelope

Every agent call returns:

```
ResultEnvelope {
  status:    "ok" | "partial" | "failed",
  result:    <agent's output, conforms to agent.outputs schema>,
  reason:    string?,                  # required for partial/failed, optional for ok
  completed: [step_descriptor]?,       # required for partial
  remaining: [step_descriptor]?,       # required for partial
  metadata: {
    tokens_used:   int,
    duration_ms:   int,
    agent_version: semver
  }
}
```

### 6.1 Status semantics

- **ok** — completed as asked. `result` is the answer.
- **partial** — did some work, hit a wall. `completed`/`remaining` describe what's done vs. not. The caller decides whether to continue, retry the remainder with adjustment, or give up.
- **failed** — couldn't produce a useful result. `reason` explains why.

### 6.2 Refusals are failures

When an agent declines (out of scope, unsafe, missing prerequisite), it returns `failed` with a reason. Never `ok` with a refusal text in `result`. This prevents callers from acting on refusal text as if it were a real result.

### 6.3 Partial requires structure

`partial` is only valid with non-null, structured `completed` and `remaining`. "I mostly did it" is not partial — it's `failed` with reason. This rule prevents quiet degradation.

## 7. Error model

Two layers, owned at different levels.

### 7.1 Framework layer (auto-retried, invisible to agents)

The framework auto-retries transient infrastructure errors with bounded retries (e.g., 3 attempts, exponential backoff):

- Network timeouts
- Rate limits (respecting `Retry-After`)
- Tool execution glitches that look transient

If retries exhaust, the failure surfaces to the caller as `failed` with a clear reason.

### 7.2 Application layer (caller decides)

Application failures (refusal, malformed output, semantic error, partial completion) are *never* auto-retried. They reach the calling agent as a result envelope, and the caller chooses the next move:

- Try a different formulation
- Try a fallback agent
- Continue with partial results
- Surface to user/orchestrator

The calling agent's prompt teaches it to branch on `status`. The framework provides the structured signal.

### 7.3 Cancellation

The framework owns cancellation. When a parent task is cancelled, all in-flight sub-agent calls are cancelled. Agents do not handle cancellation themselves.

### 7.4 Limits

Framework enforces:

- **Per-call timeout** (e.g., 5 minutes default, configurable per agent). Hitting it returns `failed` with reason `"timeout"`.
- **Delegation depth cap** (e.g., 5 levels deep). Exceeding it returns `failed` with reason `"depth_exceeded"`.

## 8. Agent lifecycle and state

### 8.1 Per-task instances (actor model)

Each agent within a task is a persistent stateful instance:

- **Created lazily.** Instantiated on first invocation in the task.
- **Lives for the task.** Subsequent calls within the same task append to the same instance's conversation history.
- **Destroyed at task end.** Instance and its history are released.

This means follow-up turns on the same task benefit from prior context — `exploration` remembers what it found earlier in the task.

### 8.2 No shared state between agents

There is no blackboard. State flows only through call args and result envelopes. Two refinements that aren't shared state:

- **Task context envelope** (§9): read-only ambient configuration available to all agents.
- **Tool-level caching**: if a tool's output is expensive (e.g., file read), the tool itself caches within the task. Invisible to agents.

### 8.3 Token growth strategy

Each agent declares its `state_policy`:

- `max_context_tokens` — hard cap.
- `strategy: "self-summarize"` — when threshold is reached, the agent compresses its history into a summary; older messages are dropped.
- `strategy: "sliding-window"` — keep last N messages plus system prompt; older messages are dropped without summarization.

Default: `self-summarize` for stateful specialists; `sliding-window` (small N) for sub-agents whose calls are mostly independent.

### 8.4 Reset

The orchestrator can request `reset(agent_id)` — the framework destroys the instance and re-instantiates a fresh one on next call. Used when an agent's accumulated state has gone bad.

## 9. Task context envelope

Set at task start, passed to every agent invocation. **Read-only. Nobody writes to it.**

```
TaskContext {
  task_id:           string,
  workspace_root:    absolute_path,
  user_request:      string,                # the original prompt that started the task
  conversation_ref:  conversation_id?,      # reference to user-facing conversation
  project_hints: {                          # optional, host-provided
    language:        string?,
    framework:       string?,
    test_command:    string?
  },
  permissions: {                            # effective family-level permissions
    tools: [tool_id]
  }
}
```

This is configuration, not state. It carries the things every agent legitimately needs to know about the environment.

## 10. Concurrency

### 10.1 Parallel dispatch primitive

The orchestrator (and any agent allowed to fan out) can call:

```
[r1, r2] = parallel([
  call_tool("exploration", {...}),
  call_tool("librarian", {...})
])
```

- Each parallel branch returns its own envelope.
- The framework awaits all branches before returning.
- Partial-failure policy: pass all envelopes through; the orchestrator decides what to do. The framework does NOT collapse a parallel block to `failed` just because one branch failed.

### 10.2 Cancellation in parallel

If the orchestrator cancels a parallel block (or a parent cancels the orchestrator), all branches receive cancellation. Branches that complete after cancellation has fired have their results discarded.

## 11. Family load and validation

When a family is loaded, the framework runs static validation. Loading fails with a clear error if any check fails.

Checks:

- All agents in `roster` resolve to a known agent definition.
- `entry_point` is in `roster`.
- All agents in `wiring` (callers and callees) and `user_callable` are in `roster`.
- Every agent's `requires_tools` is granted at the host + family layers.
- Every agent's `requires_agents` is in `roster` AND in that agent's wiring allowlist.
- No agent's self-declared `deny_tools` overlaps with its `requires_tools` (would be a broken agent).
- No circular calling rules in `wiring` (or, if circles are allowed, depth limits will catch runaway).
- `external_contract.capabilities` is a subset of `user_callable`.

Validation output is human-readable and actionable.

## 12. External contract

What the family exposes outside its boundary (the swap interface):

1. **One canonical entry point** — the orchestrator. All user input enters here; all output exits here.
2. **Named user-callable capabilities** — `family.user_callable` listed in `external_contract.capabilities`. A replacement family must provide these names with compatible semantics.
3. **Declared tool/permission surface** — what the host grants the family.
4. **One I/O format at the boundary** — text or structured events. Pick one.
5. **Lifecycle hooks**:
   - `start(task_context)` → initializes a task; returns task_id
   - `send(task_id, message)` → delivers a user message; returns response (sync or streamed)
   - `cancel(task_id)` → cancels in-flight work
   - `end(task_id)` → tears down instances, flushes traces

That's the swap interface. Everything inside is family-private.

## 13. Observability

### 13.1 Span model

Every meaningful operation is a span:

```
Span {
  trace_id:       string,                 # one per task
  span_id:        string,
  parent_span_id: string?,
  kind:           "agent" | "tool" | "parallel",
  name:           string,                 # agent or tool id
  start_time:     timestamp,
  end_time:       timestamp,
  status:         "ok" | "partial" | "failed",
  attributes: {
    input:         <redactable>,
    output:        <redactable>,
    reason:        string?,
    tokens_used:   int?,
    agent_version: semver?
  },
  events: [                               # mid-span activity events
    { timestamp, name, message }
  ]
}
```

- Every agent invocation = one span.
- Every tool call inside an agent = a child span.
- Every parallel block = a span with multiple child spans.
- Spans nest into a tree per task.

### 13.2 Storage

Two tiers:

- **In-memory ring buffer** — last N traces (configurable). Powers live UI and recent debug.
- **On-disk JSON Lines** — append-only per task, one file per task or rotated. Powers post-mortem analysis. Rotated by size or age.

### 13.3 Single backbone for UX and debug

- The activity-stream UI shortcut reads the live span stream.
- The `trace show <task-id>` CLI reads the on-disk log.
- Same schema, same data — only the consumer differs.

This is the highest-leverage piece of doing observability right. The activity UX comes for free.

### 13.4 Redaction

A redaction policy applied at write time:

- Secret-pattern detection (e.g., `AKIA*`, `sk-*`, common JWT shapes) → hashed.
- Large blobs (file contents, tool outputs over a size threshold) → truncated with elision marker.
- Configurable allowlist of attributes that should never be redacted (status, durations).

### 13.5 Inspection ergonomics

V1 must include:

- `trace show <task-id>` — pretty-prints the call tree with status, durations, key inputs/outputs.
- `trace tail` — streams live as a task runs.

V2+ additions:

- `trace search` — find all failed runs of agent X, slow runs, etc.
- OTLP export to standard observability backends.

## 14. Implementation roadmap

Suggested order, smallest viable cut first:

**Phase 1 — Skeleton.**

- Agent + family definition schemas, family loader with static validation.
- One reference agent (`exploration`) and one reference family (`default-coder`).
- Tool registry with categories, three-layer permission resolution.
- Synchronous agent-as-tool dispatch. No parallel yet.
- Result envelope with all three statuses.
- Basic per-task trace (in-memory ring + JSONL flush at task end).
- `trace show` CLI.

**Phase 2 — Real workload.**

- Add `librarian`, `planner`, `reviewer`. Wire them into `default-coder`.
- Per-task agent instances with conversation retention.
- Token-growth strategy implementations.
- Cancellation propagation, depth and timeout enforcement.

**Phase 3 — Concurrency and UX.**

- `parallel()` primitive.
- Activity-stream view (reads live span stream).
- Live `trace tail`.

**Phase 4 — Configurable swap.**

- Family selection via configuration.
- Multiple shipped families (e.g., `tdd-first`).
- `trace search`, OTLP export.

## 15. Open items deliberately deferred

These were discussed and intentionally pushed past v1:

- Streaming intermediate progress from agents (extension of tool primitive).
- Cross-task memory and persistent learning.
- Per-agent token budgets enforced by framework.
- Per-agent model selection beyond `model_hint`.
- Family configuration UI / runtime swap.
- Capability-with-arguments DSL (we explicitly chose narrow tools instead).

## 16. Quick reference — every decision in one place

| Concern | Decision |
|---|---|
| Architecture | Orchestrator + specialists + sub-agents |
| Family scope | Closed; swappable as a unit (deferred) |
| Agent ↔ family relationship | Decoupled; agents reusable, families compose |
| Tool sharing | Global registry; per-agent denies |
| Permission policy | Three layers (host / family / agent); strictest wins |
| Sub-tool restrictions | No — prefer narrow tools; workspace scoped at tool level |
| Inter-agent calls | Agents-as-tools; caller sees only result envelope |
| Shared state between agents | None (no blackboard); read-only task context envelope; tool-level caching |
| Result envelope | `{status, result, reason, completed?, remaining?}` |
| Error retry | Framework auto-retries infra errors; never auto-retries app errors |
| Refusals | Always `failed` |
| Cancellation | Framework-owned; propagates |
| Depth cap | Hard limit (e.g. 5) |
| Concurrency | `parallel()` primitive; pass all envelopes through |
| Agent lifecycle | Per-task instances (actor model); lazy create, task-scoped destroy |
| Conversation continuity | Retained within task |
| Token growth | Per-agent `state_policy`: self-summarize or sliding-window |
| Observability | OpenTelemetry-style spans; one trace per task; in-memory + on-disk |
| Activity UI | Reads live span stream — same backbone as observability |
| External contract | Entry point + named capabilities + I/O format + lifecycle hooks |
