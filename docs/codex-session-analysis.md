# Codex Session Analysis For `smooth-code`

## What Codex Actually Does In One Session

Codex is not "prompt in, string out". Its runtime is a session orchestrator with four distinct layers:

1. Submission loop
   The outer loop receives `Submission { id, op, trace }` values and dispatches them by operation kind.

2. Session state
   A session owns thread-scoped mutable state: message history, rate/token metadata, active tool/input waiters, current settings, and the currently running turn.

3. Turn task
   A user turn becomes a concrete task object such as `RegularTask`, `ReviewTask`, or `CompactTask`. The task runs in the background, can be cancelled, and owns the turn lifecycle.

4. Sampling loop
   `RegularTask` calls `run_turn`, which repeatedly:
   - builds model input from history plus pending input
   - streams model output
   - executes tool calls
   - records new response items into history
   - decides whether another model follow-up request is needed

That shape matters because `AnySessionTask` is not just "async fn prompt()". It is the runtime seam between session orchestration and a concrete turn workflow.

## Minimal Codex Behaviors Worth Copying First

For `smooth-code`, the smallest useful subset is:

1. One session per thread.
2. One active turn at a time per session.
3. History persisted as model-native messages, not ad hoc strings.
4. A concrete `RegularTask` that owns a full turn.
5. Provider-specific model execution delegated to `rig`.
6. Cancellation hooks on the task boundary, even if tool cancellation is deferred.

Everything else in Codex is important later, but not required for the first end-to-end turn:

- review tasks
- compact tasks
- approvals
- MCP
- sandboxed exec
- hooks
- realtime conversation
- multi-turn pending-input steering

## Why `AnySessionTask` Is The Right Seam

Codex uses two task traits:

- a concrete ergonomic trait for implementers
- a boxed/object-safe trait for storage in `ActiveTurn`

That split is the correct pattern for `smooth-code` too.

`AnySessionTask` should remain runtime-facing and object safe:

- `kind()`
- `span_name()`
- `run(session, ctx, input, cancellation_token)`
- `abort(session, ctx)`

Concrete tasks such as `RegularTask` should implement a non-object-safe `SessionTask`, with a blanket impl to `AnySessionTask`.

## What A Complete MVP Turn Needs

The MVP `RegularTask` should do exactly this:

1. Accept raw user input.
2. Convert it into `rig::message::Message::User`.
3. Read prior session history as `Vec<rig::message::Message>`.
4. Call a provider-backed `rig::agent::Agent`.
5. Stream `rig::streaming::StreamedAssistantContent<_>`.
6. Accumulate assistant text.
7. Record both user and assistant messages back into history.
8. Return the final assistant message.

This is enough to say `smooth-code` can run a complete turn.

## Provider Design Guidance

Do not invent a parallel chat schema. Reuse Rig's types directly:

- `rig::message::Message`
- `rig::message::UserContent`
- `rig::message::AssistantContent`
- `rig::message::Text`
- `rig::streaming::StreamedAssistantContent<_>`

The provider abstraction should only choose which Rig client/agent to build:

- OpenAI
- OpenRouter
- Anthropic
- Gemini

That keeps `smooth-code` aligned with the user's requirement: support multiple LLM providers by using Rig, not by rebuilding Rig internally.

## Implementation Tasks

### Task 1: Stabilize The Session Runtime

- Keep `Session` as the owner of history, agent status, and active turn state.
- Keep `ActiveTurn` storing `Arc<dyn AnySessionTask>`.
- Add a `run_task` helper on `Session` that:
  - cancels prior work
  - registers the new running task
  - waits for completion
  - clears the active turn slot

### Task 2: Use Rig Message Types In History

- Replace string-only history with `Vec<rig::message::Message>`.
- Record user turns as `Message::User`.
- Record assistant turns as `Message::Assistant`.

This prevents schema drift between the runtime and the provider adapter.

### Task 3: Implement `RegularTask`

- Implement `SessionTask` for `RegularTask`.
- In `run`:
  - join input
  - record user message
  - build Rig prompt
  - call the provider adapter
  - record assistant message
  - return final text

### Task 4: Add A Rig Provider Adapter

- Build one `SessionModel` enum with provider variants.
- Construct the concrete Rig `Agent<...>` once per session.
- Match on the provider enum to stream a completion.
- Reject tool calls for now with an explicit error instead of pretending they work.

### Task 5: Wire App Server To Core

- `turn/start` should resolve thread id
- lazily create the thread session
- call `Core::run_user_input`
- return a typed `TurnStartResponse`

### Task 6: Add Tool-Loop Support Next

To imitate Codex more closely, the next real milestone after MVP is not UI polish. It is model follow-up after tool calls.

That means:

1. accept `StreamedAssistantContent::ToolCall`
2. map the tool call into an app-server request or local tool executor
3. convert tool output back into Rig `ToolResult`
4. append the tool result into history
5. issue another model request in the same turn

That is the point where `smooth-code` moves from "chat agent" to "code agent".
