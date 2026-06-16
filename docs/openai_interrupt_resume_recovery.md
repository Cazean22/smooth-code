# OpenAI WebSocket interrupt/resume recovery

This note documents the fix for a failure where interrupting an OpenAI-backed
session and then sending `continue` could repeatedly fail with:

```text
ProviderError: OpenAI WebSocket connection reset before response.completed
```

The bug was not one single retry/classification problem. It was a combination of
provider cancellation, turn-cleanup ordering, and model-history persistence.

## Symptoms

A typical log sequence looked like:

```text
processing turn cancel request
OpenAI WebSocket cancel drain deadline expired; dropping socket
finished regular task

processing turn start request input_len=8
running regular task input_count=1
OpenAI WebSocket transient failure before any assistant item; retrying retry_count=1
...
OpenAI WebSocket transient failure before any assistant item; retrying retry_count=8
session task failed; marking turn errored
ProviderError: OpenAI WebSocket connection reset before response.completed
```

On old rollouts, repeated attempts made the problem worse because each failed
`continue` could be persisted into provider/model history without a matching
assistant response.

## Root causes

### 1. Cancellation did not target the active OpenAI response

The WebSocket stream used to send a bare cancel frame:

```json
{"type":"response.cancel"}
```

The Responses/WebSocket protocol can associate generation with a response id. If
that id is known, cancellation should target it explicitly. A bare cancel can be
ambiguous for a local proxy or upstream bridge and may leave the upstream
response running or otherwise poison the next request.

### 2. `continue` could race interrupted-turn cleanup

User interrupt is intentionally fast: the UI should get an immediate response.
However, the cancelled turn still needs to drain cooperatively in the background:

- send provider-side cancellation,
- keep polling briefly so cancellation can complete,
- finish or synthesize interrupted tool results,
- persist any finalized partial turn state.

Before the fix, a user could send `continue` while that cleanup was still in
progress. The new provider request could then overlap with cancellation/history
cleanup from the previous turn.

### 3. Pre-output provider failures poisoned model history

The regular task used to persist the user prompt to model history at turn start.
If the provider then failed before producing any assistant item, the prompt was
already durable model history.

For repeated `continue` attempts, this could produce a tail like:

```text
user: make a plan to fix it
assistant: partial interrupted output
user: continue
user: continue
user: continue
```

Those `continue` messages had no matching assistant output. Resuming the thread
kept replaying that malformed/unstable tail to OpenAI, causing repeated resets.

### 4. Existing rollouts already contained unstable tails

Even after fixing new prompt persistence, already-written rollouts still had
interrupted/errored model-history tails. Resume had to sanitize those old
sessions; otherwise the same bad history would be sent again.

## Fixes

### Targeted OpenAI cancellation

`crates/core/src/provider.rs` now tracks the active response id in the WebSocket
accumulator. It captures `response.id` from lifecycle frames including:

- `response.created`,
- `response.in_progress`,
- `response.completed`,
- `response.failed`,
- `response.incomplete`,
- `response.done`.

When cancellation happens and an id is known, Cazean sends:

```json
{"type":"response.cancel","response_id":"resp_..."}
```

If no id has been observed yet, it keeps the compatibility fallback:

```json
{"type":"response.cancel"}
```

This keeps cancellation precise without breaking very-early cancellation.

### New-turn gate after user interrupt

`crates/core/src/core.rs` now has an interrupt-cleanup gate.

The interrupt response still returns immediately, but the next user turn waits
for the background drain to complete before it can:

- record its prompt,
- mutate model history,
- open a provider stream.

This preserves responsive cancellation while preventing the next request from
racing the cancelled request's cleanup.

### Persist prompts only with durable turn results

`crates/core/src/tasks/regular.rs` no longer eagerly writes the user prompt to
model history at turn start.

Instead:

- the visible `UserMessage` event is still emitted immediately for the UI;
- the model-facing `Message::User` is kept in the in-flight turn state;
- the prompt is persisted only when the turn has a durable outcome.

`crates/core/src/core.rs::persist_turn_tail` was adjusted to persist the full
finalized turn message list, including the initial user prompt. Previously it
skipped the first message because the prompt had already been written eagerly.

The resulting behavior is:

- completed turn: persist prompt plus assistant result;
- interrupted turn with finalized partial state: persist the finalized partial
  turn deliberately;
- provider failure before any assistant output: do **not** add the failed prompt
  to model history.

### Resume pruning for unstable model-history tails

`crates/core/src/rollout.rs` now sanitizes provider/model history on resume.

The rule is:

> Keep model history through the last successfully completed turn. If later
> history belongs to an interrupted or errored tail, drop it from provider
> history on resume.

This affects only model-facing history. Transcript events still replay, so the
UI can still show prior user messages, errors, and interruptions.

This is what recovers old poisoned threads: the visible transcript remains, but
the next OpenAI request no longer includes failed `continue` prompts or partial
interrupted tails after the last stable completion.

## Why not just retry more?

The failure happened before any assistant item, so pre-output retries were safe
and already existed. But retrying the same poisoned request eight times just
reproduced the same failure.

The durable fix was to make the next request valid again by:

1. cancelling the previous OpenAI response correctly;
2. waiting for interrupted-turn cleanup before starting a new turn;
3. avoiding persistence of failed pre-output prompts;
4. pruning old unstable tails from resumed provider history.

## Important invariants

- Visible transcript persistence and provider/model history persistence are not
  the same thing.
- `UserMessage` events can persist immediately for replay/UI purposes.
- Rig `Message` history should be persisted only for stable model-facing turns.
- Interrupted/errored tails after the last completed turn are unsafe to replay to
  OpenAI as provider history.
- A user interrupt may return immediately, but the next turn must wait for the
  cancelled task's cooperative cleanup.
- OpenAI WebSocket cancellation should include `response_id` when known.

## Tests

The fix is covered by tests for:

- targeted OpenAI `response.cancel` payloads;
- bare cancel fallback before a response id is known;
- immediate `continue` waiting for interrupted-turn cleanup;
- pre-output provider failures not persisting the failed prompt to model history;
- resume pruning of errored model-history tails;
- resume pruning of interrupted model-history tails;
- existing OpenAI WebSocket reuse/retry/disconnect behavior.

Validation commands used for the change:

```sh
cargo test -p cazean-core
cargo check --workspace
git diff --check
```

## Operational debugging

Useful local files when diagnosing similar failures:

- `.cazean/logs/cazean-tui.log` — runtime log and provider retry/errors;
- `.cazean/sessions/YYYY/MM/DD/*.jsonl` — rollout transcript and model-history
  records;
- `CAZEAN_OPENAI_TURN_REQUEST_JSON=/path/to/request.json` — optional dump of the
  normalized OpenAI Responses request body before `response.create` is sent.

When investigating a repeated `continue` failure, check whether the rollout has
multiple failed `continue` history messages or an interrupted partial tail after
the last completed turn. Those should no longer be replayed to the provider after
this fix.
