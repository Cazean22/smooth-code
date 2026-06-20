use std::{collections::BTreeMap, sync::Arc, time::Duration};

use app_server_protocol::{PlanApprovalDecision, RequestPlanApprovalParams};
use cazean_protocol::{
    AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
    AgentReasoningDeltaEvent, AgentStatus, ErrorEvent, ErrorInfo, EventMsg, ProjectInstructions,
    StreamErrorEvent, ThreadId, ToolCallCompletedEvent, ToolCallResultKind, ToolCallStartedEvent,
};
use futures_util::{FutureExt, StreamExt, future::select_all, stream::FuturesUnordered};
use indexmap::IndexMap;
use rig::{
    OneOrMany,
    completion::CompletionError,
    message::{
        AssistantContent, Message, Reasoning as MessageReasoning, ReasoningContent, Text,
        ToolResult, ToolResultContent, UserContent,
    },
};
use tools::{DecodedToolOutput, SkillMeta, SubagentArgs, decode_tool_output_for_tool};

use crate::{
    agent::{
        AgentControl, InlineChildCompletionReceiver, SystemPromptKind,
        control::InlineChildCompletion,
        registry::AgentMetadata,
        status::is_final,
        subagent_result::{
            CompletionEntry, completion_entries_to_user_message, encode_spawn_agent_result,
        },
    },
    core::{CancelBudget, Session, TurnCancel, TurnContext},
    provider::{
        SessionAssistantContent, SessionCompletionEvent, SessionTurnSummary,
        is_openai_websocket_transient_start_error, strip_ws_transient_tag,
        tool_error_is_interrupted,
    },
    state::TaskKind,
};

use super::SessionTask;

fn reasoning_text(reasoning: &MessageReasoning) -> String {
    reasoning
        .content
        .iter()
        .filter_map(|content| match content {
            ReasoningContent::Text { text, .. } | ReasoningContent::Summary(text) => {
                Some(text.as_str())
            }
            // `ReasoningContent` is non-exhaustive; only human-readable variants are surfaced.
            _ => None,
        })
        .collect::<String>()
}

fn id_only_reasoning(id: String) -> MessageReasoning {
    let mut reasoning = MessageReasoning::new("");
    reasoning.content.clear();
    reasoning.with_id(id)
}

/// Mark the current turn as failed: log, emit a protocol `Error` event so the
/// rollout / inbox carries the underlying provider/stream error, and publish
/// `AgentStatus::Errored` so any parent waiting on this thread (inline waiter,
/// completion watcher) unblocks with a meaningful status instead of stalling
/// on `Running` forever or being overwritten with a misleading `Completed`.
async fn fail_turn(
    session: &Arc<Session>,
    ctx: &TurnContext,
    site: &'static str,
    err: anyhow::Error,
) {
    let message = strip_ws_transient_tag(&err.to_string());
    let error_info =
        ErrorInfo::new("turn_failed", message.clone()).with_source(format!("cazean-core::{site}"));
    tracing::error!(
        thread_id = %session.id,
        turn_id = %ctx.sub_id,
        site,
        error = %message,
        "session task failed; marking turn errored"
    );
    session
        .emit_event(
            ctx,
            EventMsg::Error(ErrorEvent {
                error: error_info.clone(),
            }),
        )
        .await;
    session
        .set_agent_status(AgentStatus::Errored(error_info), Some(ctx))
        .await;
}

#[derive(Default)]
pub(crate) struct RegularTask;

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

struct ExecutedToolCall {
    index: usize,
    tool_result: ToolResult,
}

struct PendingToolCall {
    index: usize,
    assistant_tool_call: AssistantContent,
    tool_call: rig::message::ToolCall,
    internal_call_id: String,
}

/// Identity of a pending tool call, captured before the call's future takes
/// ownership of it, so a call that never resolves (cancelled turn) can still
/// be completed with a placeholder interrupted result.
struct PendingCallDescriptor {
    index: usize,
    tool_call_id: String,
    tool_call_call_id: Option<String>,
    internal_call_id: String,
    tool_name: String,
}

impl PendingCallDescriptor {
    fn of(pending: &PendingToolCall) -> Self {
        Self {
            index: pending.index,
            tool_call_id: pending.tool_call.id.clone(),
            tool_call_call_id: pending.tool_call.call_id.clone(),
            internal_call_id: pending.internal_call_id.clone(),
            tool_name: pending.tool_call.function.name.clone(),
        }
    }
}

/// Complete a tool call that will never produce real output because the turn
/// was cancelled: emits `ToolCallCompleted` with `Interrupted` and yields a
/// placeholder result so the assistant tool-call message keeps a matching
/// tool result (providers require a result for every call).
async fn complete_interrupted_tool_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    descriptor: PendingCallDescriptor,
) -> ExecutedToolCall {
    let message = format!(
        "{} was interrupted before completing (turn cancelled)",
        descriptor.tool_name
    );
    complete_tool_call_with_kind(
        session,
        ctx,
        descriptor.index,
        descriptor.tool_call_id,
        descriptor.tool_call_call_id,
        descriptor.internal_call_id,
        descriptor.tool_name,
        message.clone(),
        false,
        Some(message),
        ToolCallResultKind::Interrupted,
        None,
    )
    .await
}

/// Result of running a tool batch: whatever results exist (real or placeholder
/// interrupted ones — every call in the batch is represented) plus whether
/// cancellation was observed along the way.
struct ToolBatchOutcome {
    executed: Vec<ExecutedToolCall>,
    cancelled: bool,
}

struct StartedSpawnToolCall {
    index: usize,
    tool_call_id: String,
    tool_call_call_id: Option<String>,
    internal_call_id: String,
    metadata: AgentMetadata,
    child_thread_id: ThreadId,
    tool_name: String,
    initial_status: AgentStatus,
    waiter: Option<InlineChildCompletionReceiver>,
    completion: Option<Result<InlineChildCompletion, String>>,
}

struct RetainedSpawnCompletion {
    metadata: AgentMetadata,
    waiter: InlineChildCompletionReceiver,
}

/// Per-id buffer for `ReasoningDelta` chunks streamed alongside (or in place of)
/// authoritative `Reasoning` completion events. Tracking deltas per id lets the
/// fallback reconstruct one `MessageReasoning` per distinct block when the
/// provider never sends completions, instead of collapsing every delta into a
/// single block with the wrong id.
///
/// A `Reasoning` completion supersedes the deltas it summarizes, regardless of
/// whether an id is present. For id'd completions that means clearing the
/// matching id's bucket. For idless completions — Anthropic's signed
/// `thinking` blocks are the canonical case — the same rule applies: the
/// completion's content (with its signature) replaces the pending idless
/// deltas. Leaving them in the bucket would emit an unsigned duplicate at
/// finalize and the provider would reject the next request.
#[derive(Default)]
struct PendingReasoningDeltas {
    deltas: IndexMap<Option<String>, String>,
}

impl PendingReasoningDeltas {
    fn push_delta(&mut self, id: Option<String>, text: &str) {
        self.deltas.entry(id).or_default().push_str(text);
    }

    fn on_completion(&mut self, id: &Option<String>) {
        self.deltas.shift_remove(id);
    }

    fn finalize_into(self, accumulated: &mut Vec<MessageReasoning>) {
        for (id, text) in self.deltas {
            if text.is_empty() {
                if let Some(id) = id {
                    accumulated.push(id_only_reasoning(id));
                }
                continue;
            }
            let mut reasoning = MessageReasoning::new(&text);
            if let Some(id) = id {
                reasoning = reasoning.with_id(id);
            }
            accumulated.push(reasoning);
        }
    }
}

impl SessionTask for RegularTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<String>,
        cancel: TurnCancel,
    ) -> Option<String> {
        tracing::debug!(
            thread_id = %session.id,
            turn_id = %ctx.sub_id,
            input_count = input.len(),
            "running regular task"
        );
        let _ = self;
        if cancel.is_cancelled() {
            return None;
        }

        let input_count = input.len();
        let history_before_turn = session.history().await;
        let prompt_parts = input
            .into_iter()
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        let prompt_text = prompt_parts.join("\n");
        // Skills are advertised (and `/name` expanded) only for sessions whose
        // agent actually registers the `skill` tool; Explore agents are
        // read-only and `build_agent` excludes the tool for them, so steering
        // them toward it would only produce failed tool calls.
        let skills_enabled = !matches!(session.system_prompt_kind, SystemPromptKind::Explore);
        // Skill roots (user-global + project) resolved once and shared by the
        // `/name` slash expansion and the available-skills listing below.
        let skill_roots = tools::skill_roots(cazean_config::user_skills_dir(), &session.cwd);
        // A leading `/skill-name` is expanded to the skill's instructions for
        // the model and the persisted history, while the transcript event
        // keeps the raw text the user typed.
        let model_text = skills_enabled
            .then(|| {
                expand_skill_invocation(
                    &skill_roots,
                    &prompt_text,
                    session.agent_control.max_skill_bytes(),
                )
            })
            .flatten()
            .unwrap_or_else(|| prompt_text.clone());
        session
            .emit_event(
                &ctx,
                EventMsg::UserMessage {
                    text: prompt_text.clone(),
                },
            )
            .await;

        let prompt = Message::User {
            content: OneOrMany::one(UserContent::Text(Text {
                text: model_text,
                additional_params: None,
            })),
        };
        let skills = if skills_enabled {
            tools::list_skills(&skill_roots, session.agent_control.max_skill_bytes())
        } else {
            Vec::new()
        };
        let result = run_manual_turn(
            Arc::clone(&session),
            Arc::clone(&ctx),
            prompt,
            history_before_turn,
            skills,
            cancel.clone(),
        )
        .await;
        tracing::debug!(
            thread_id = %session.id,
            turn_id = %ctx.sub_id,
            input_count,
            "finished regular task"
        );
        result
    }

    async fn abort(&self, session: Arc<Session>, _ctx: Arc<TurnContext>) {
        session.abort_pending_server_requests().await;
    }
}

/// Per-attempt accumulation state for one provider stream attempt within a turn.
/// A fresh `AttemptState` is created for every `'turn_loop` iteration (the
/// initial attempt plus each mid-stream-retry continuation). Turn-scoped state
/// (the history tail, retained completions, consumed children, the retry
/// counter, pending stream-phase completions) deliberately stays in
/// `run_manual_turn`, not here.
struct AttemptState {
    pending_tool_calls: Vec<PendingToolCall>,
    accumulated_text: String,
    accumulated_reasoning: Vec<MessageReasoning>,
    pending_reasoning_deltas: PendingReasoningDeltas,
    saw_assistant_item_this_attempt: bool,
    saw_tool_call_this_attempt: bool,
    turn_summary: SessionTurnSummary,
}

impl AttemptState {
    fn new() -> Self {
        Self {
            pending_tool_calls: Vec::new(),
            accumulated_text: String::new(),
            accumulated_reasoning: Vec::new(),
            pending_reasoning_deltas: PendingReasoningDeltas::default(),
            saw_assistant_item_this_attempt: false,
            saw_tool_call_this_attempt: false,
            turn_summary: SessionTurnSummary {
                assistant_message_id: None,
                response: String::new(),
            },
        }
    }

    /// Fold any buffered reasoning deltas into `accumulated_reasoning`. Consumes
    /// the delta buffer (mirrors `PendingReasoningDeltas::finalize_into`, which
    /// takes `self`); a fresh attempt starts with an empty buffer again.
    fn finalize_reasoning(&mut self) {
        std::mem::take(&mut self.pending_reasoning_deltas)
            .finalize_into(&mut self.accumulated_reasoning);
    }

    /// Fold one streamed completion event into the attempt: append assistant
    /// text/reasoning, record tool calls, capture the terminal summary, and emit
    /// the matching live `EventMsg`s. `saw_tool_loop` is turn-wide and lives in
    /// `run_manual_turn`; the caller ORs it from `self.saw_tool_call_this_attempt`
    /// after each event, so this method only touches per-attempt state.
    async fn ingest_event(
        &mut self,
        event: SessionCompletionEvent,
        session: &Arc<Session>,
        ctx: &TurnContext,
        attempt_assistant_item_id: &str,
    ) {
        match event {
            SessionCompletionEvent::AssistantItem(assistant_item) => match assistant_item {
                SessionAssistantContent::Text(text) => {
                    self.saw_assistant_item_this_attempt = true;
                    let delta = text.text;
                    self.accumulated_text.push_str(&delta);
                    session
                        .emit_event(
                            ctx,
                            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                item_id: attempt_assistant_item_id.to_string(),
                                delta,
                            }),
                        )
                        .await;
                }
                SessionAssistantContent::ToolCall {
                    tool_call,
                    internal_call_id,
                } => {
                    self.saw_assistant_item_this_attempt = true;
                    self.saw_tool_call_this_attempt = true;
                    // A tool call ends the current assistant text segment, so any
                    // text streamed before it is a finished preamble message. Text
                    // only reaches the UI as (unpersisted) `AgentMessageDelta`s, and
                    // the turn's lone persisted `AgentMessageCompleted` covers only
                    // its final post-tool message — so without this, a preamble shows
                    // live (committed when the tool call starts) but is lost on
                    // resume, where the transcript is rebuilt from persisted events
                    // alone. Emit it now, before the tool call's own events, so resume
                    // replays it in order. Key the id off this call's unique internal
                    // id so it cannot collide with the turn's other assistant-message
                    // ids; a collision would make the resume/preview de-dupe
                    // (`committed_assistant_item_id`) silently drop a message.
                    if self.pending_tool_calls.is_empty() && !self.accumulated_text.is_empty() {
                        session
                            .emit_event(
                                ctx,
                                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    item_id: format!("{internal_call_id}-assistant-text"),
                                    text: self.accumulated_text.clone(),
                                }),
                            )
                            .await;
                    }
                    session
                        .emit_event(
                            ctx,
                            EventMsg::ToolCallStarted(ToolCallStartedEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                call_id: internal_call_id.clone(),
                                tool_name: tool_call.function.name.clone(),
                                args_preview: tool_call.function.arguments.to_string(),
                            }),
                        )
                        .await;
                    self.pending_tool_calls.push(PendingToolCall {
                        index: self.pending_tool_calls.len(),
                        assistant_tool_call: AssistantContent::ToolCall(tool_call.clone()),
                        tool_call,
                        internal_call_id,
                    });
                }
                SessionAssistantContent::ReasoningDelta { id, reasoning } => {
                    self.saw_assistant_item_this_attempt = true;
                    let item_id = id
                        .clone()
                        .unwrap_or_else(|| format!("{attempt_assistant_item_id}-reasoning"));
                    self.pending_reasoning_deltas.push_delta(id, &reasoning);
                    if !reasoning.is_empty() {
                        session
                            .emit_event(
                                ctx,
                                EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    item_id,
                                    delta: reasoning,
                                }),
                            )
                            .await;
                    }
                }
                SessionAssistantContent::Reasoning(reasoning) => {
                    self.saw_assistant_item_this_attempt = true;
                    // Skip only completions that carry neither content nor a
                    // provider id. OpenAI can emit an id-only reasoning item that
                    // must be roundtripped because later function/message output
                    // may depend on the `rs_*` id, even though there is no visible
                    // reasoning text for the transcript.
                    if reasoning.content.is_empty() && reasoning.id.is_none() {
                        return;
                    }
                    self.pending_reasoning_deltas.on_completion(&reasoning.id);
                    let text = reasoning_text(&reasoning);
                    let item_id = reasoning
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("{attempt_assistant_item_id}-reasoning"));
                    merge_reasoning_blocks(&mut self.accumulated_reasoning, &reasoning);
                    if !text.is_empty() {
                        session
                            .emit_event(
                                ctx,
                                EventMsg::AgentReasoningCompleted(AgentReasoningCompletedEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    item_id,
                                    text,
                                }),
                            )
                            .await;
                    }
                }
                SessionAssistantContent::ToolCallDelta { .. } => {
                    self.saw_assistant_item_this_attempt = true;
                }
                SessionAssistantContent::Final => {}
            },
            SessionCompletionEvent::Completed(summary) => {
                self.turn_summary = summary;
            }
        }
    }
}

async fn run_manual_turn(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    initial_prompt: Message,
    history_before_turn: Vec<Message>,
    skills: Vec<SkillMeta>,
    cancel: TurnCancel,
) -> Option<String> {
    let mut new_messages = vec![initial_prompt];
    // Maps an index in `new_messages` to the typed completion group that index
    // was rendered from, so the turn-end persist can write a typed
    // `SubagentCompletion` record there instead of an opaque user message while
    // the in-memory history keeps the rendered `Message::User`. Populated only
    // through `push_completion_group`, so the index can never drift.
    let mut completions_by_index: BTreeMap<usize, Vec<CompletionEntry>> = BTreeMap::new();
    let mut saw_tool_loop = false;
    let mut retained_subagents = Vec::new();
    // Children consumed during this turn are released from in-memory state
    // immediately, but their durable parent→child edges are closed only after
    // the turn's result is persisted (see the terminal arm below). Holding the
    // edge open until then keeps a consumed child rehydratable if the turn is
    // interrupted or crashes before its result reaches the rollout.
    let mut consumed_children: Vec<ThreadId> = Vec::new();
    // This mid-output continuation budget is separate from the provider's
    // before-output retry budget; provider retries handle startup churn, while
    // this counter bounds only attempts that already yielded assistant output.
    let mut stream_retries = 0;
    let mut stream_phase_completions: Vec<CompletionEntry> = Vec::new();

    'turn_loop: loop {
        if cancel.is_cancelled() {
            finalize_interrupted_turn(
                &session,
                &ctx,
                history_before_turn,
                new_messages,
                &completions_by_index,
            )
            .await;
            return None;
        }

        let attempt_assistant_item_id = assistant_item_id_for_attempt(&ctx, stream_retries);
        let (pending_prompt, request_history) = build_request_parts(
            &history_before_turn,
            session.project_instructions.as_ref(),
            &skills,
            &new_messages,
        )?;
        let model_for_stream = session.model();
        let mut stream = match model_for_stream
            .stream_completion_turn(pending_prompt, &request_history, cancel.token())
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                fail_turn(&session, &ctx, "manual.stream_completion_turn.open", err).await;
                return None;
            }
        };
        let mut attempt = AttemptState::new();

        loop {
            if cancel.is_cancelled() {
                drain_cancelled_stream(&session, &mut stream, &cancel).await;
                finalize_interrupted_attempt_and_turn(
                    &session,
                    &ctx,
                    &mut attempt,
                    history_before_turn,
                    new_messages,
                    &completions_by_index,
                )
                .await;
                return None;
            }

            let item = tokio::select! {
                _ = cancel.cancelled() => {
                    drain_cancelled_stream(&session, &mut stream, &cancel).await;
                    finalize_interrupted_attempt_and_turn(
                        &session,
                        &ctx,
                        &mut attempt,
                        history_before_turn,
                        new_messages,
                        &completions_by_index,
                    )
                    .await;
                    return None;
                }
                Some(text) = next_retained_subagent_completion(&mut retained_subagents, &session.agent_control, &mut consumed_children),
                    if !retained_subagents.is_empty() => {
                    stream_phase_completions.push(text);
                    continue;
                }
                item = stream.next() => item,
            };
            let Some(item) = item else {
                break;
            };
            let event = match item {
                Ok(event) => event,
                Err(err) => {
                    attempt.finalize_reasoning();
                    if should_continue_after_stream_error(
                        &err,
                        stream_retries,
                        session.model().websocket_retry_budget(),
                        attempt.saw_assistant_item_this_attempt,
                    ) {
                        let batch_cancelled = commit_attempt_messages(
                            &session,
                            &ctx,
                            &mut attempt,
                            &mut new_messages,
                            &mut completions_by_index,
                            &mut retained_subagents,
                            &mut consumed_children,
                            &mut stream_phase_completions,
                            &cancel,
                        )
                        .await;
                        if !attempt.accumulated_text.is_empty() {
                            session
                                .emit_event(
                                    &ctx,
                                    EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                                        thread_id: session.id.to_string(),
                                        turn_id: ctx.sub_id.clone(),
                                        item_id: attempt_assistant_item_id.clone(),
                                        text: attempt.accumulated_text.clone(),
                                    }),
                                )
                                .await;
                        }
                        if batch_cancelled {
                            finalize_interrupted_turn(
                                &session,
                                &ctx,
                                history_before_turn,
                                new_messages,
                                &completions_by_index,
                            )
                            .await;
                            return None;
                        }

                        let next_retry = stream_retries + 1;
                        let retry_budget = session.model().websocket_retry_budget();
                        session
                            .emit_event(
                                &ctx,
                                EventMsg::StreamError(StreamErrorEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    message: format!("Reconnecting… {next_retry}/{retry_budget}"),
                                }),
                            )
                            .await;
                        stream_retries = next_retry;
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                finalize_interrupted_turn(
                                    &session,
                                    &ctx,
                                    history_before_turn,
                                    new_messages,
                                    &completions_by_index,
                                )
                                .await;
                                return None;
                            }
                            _ = tokio::time::sleep(session.model().websocket_retry_delay(stream_retries)) => {}
                        }
                        continue 'turn_loop;
                    }
                    fail_turn(&session, &ctx, "manual.stream_completion_turn.item", err).await;
                    return None;
                }
            };
            attempt
                .ingest_event(event, &session, &ctx, &attempt_assistant_item_id)
                .await;
            // `saw_tool_loop` is turn-wide (it decides the empty-string return
            // below); OR it from this attempt's observation after every event.
            // `saw_tool_call_this_attempt` is reset per attempt, this is not.
            saw_tool_loop |= attempt.saw_tool_call_this_attempt;
        }

        attempt.finalize_reasoning();

        if attempt.saw_tool_call_this_attempt {
            let batch_cancelled = commit_attempt_messages(
                &session,
                &ctx,
                &mut attempt,
                &mut new_messages,
                &mut completions_by_index,
                &mut retained_subagents,
                &mut consumed_children,
                &mut stream_phase_completions,
                &cancel,
            )
            .await;
            if batch_cancelled {
                finalize_interrupted_turn(
                    &session,
                    &ctx,
                    history_before_turn,
                    new_messages,
                    &completions_by_index,
                )
                .await;
                return None;
            }
            continue;
        }

        if !stream_phase_completions.is_empty() {
            append_completion_texts_after_assistant_turn(
                &mut new_messages,
                &mut completions_by_index,
                &attempt.turn_summary,
                &attempt.accumulated_text,
                &mut attempt.accumulated_reasoning,
                session.model().requires_provider_reasoning_ids(),
                std::mem::take(&mut stream_phase_completions),
            );
            continue;
        }

        if !retained_subagents.is_empty() {
            let completion_entries = tokio::select! {
                _ = cancel.cancelled() => {
                    // The attempt streamed no tool calls (handled above), so
                    // its partial text has not been committed yet — fold it in
                    // before finalizing the interrupted turn.
                    finalize_interrupted_attempt_and_turn(
                        &session,
                        &ctx,
                        &mut attempt,
                        history_before_turn,
                        new_messages,
                        &completions_by_index,
                    )
                    .await;
                    return None;
                }
                completions = drain_retained_subagent_completions(&mut retained_subagents, &session.agent_control, &mut consumed_children) => completions,
            };
            append_completion_texts_after_assistant_turn(
                &mut new_messages,
                &mut completions_by_index,
                &attempt.turn_summary,
                &attempt.accumulated_text,
                &mut attempt.accumulated_reasoning,
                session.model().requires_provider_reasoning_ids(),
                completion_entries,
            );
            continue;
        }

        if let Some(message) = build_assistant_text_reasoning_message(
            &attempt.turn_summary,
            &attempt.accumulated_text,
            &mut attempt.accumulated_reasoning,
            session.model().requires_provider_reasoning_ids(),
        ) {
            new_messages.push(message);
        }
        let last_assistant_message =
            assistant_text_for_message(&attempt.turn_summary, &attempt.accumulated_text);
        let persisted = session
            .persist_turn_tail(&new_messages, &completions_by_index)
            .await;
        let mut final_history = history_before_turn;
        final_history.extend(new_messages);
        session.replace_history(final_history).await;
        // The turn's result (including every consumed child's completion) is now
        // durable in the parent's rollout, so it is finally safe to close those
        // children's parent→child edges. Doing this only here — never on the
        // cancel/early-return paths above, and never when persistence failed —
        // guarantees we never close an edge before the result that supersedes it
        // is durable: an interrupted, crashed, or unpersisted turn leaves the
        // edge open, and resume reaps the finished child instead.
        if !persisted {
            // The completed turn's tail could not be durably written. Surface it as
            // a failed turn and, crucially, return `None` so the runner does NOT
            // emit `TurnCompleted`: a `TurnCompleted` would clear the unstable
            // marker on resume and replay the partial, possibly-malformed tail.
            // With an errored terminal instead, resume prunes that tail. In-memory
            // history keeps the turn so the live session stays coherent, and
            // consumed-child edges stay open so resume reaps those children.
            fail_turn(
                &session,
                &ctx,
                "manual.persist_turn_tail",
                anyhow::anyhow!(
                    "turn result could not be saved to the session rollout; it will not survive a resume"
                ),
            )
            .await;
            return None;
        }
        close_consumed_child_edges(&session, &consumed_children).await;
        if !last_assistant_message.is_empty() {
            session
                .emit_event(
                    &ctx,
                    EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                        thread_id: session.id.to_string(),
                        turn_id: ctx.sub_id.clone(),
                        item_id: attempt_assistant_item_id,
                        text: last_assistant_message.clone(),
                    }),
                )
                .await;
            session
                .emit_event(
                    &ctx,
                    EventMsg::AgentMessage {
                        text: last_assistant_message.clone(),
                    },
                )
                .await;
            return Some(last_assistant_message);
        }

        if saw_tool_loop {
            return Some(String::new());
        }

        return None;
    }
}

/// Keep polling the cancelled stream briefly so the provider-side cancel can
/// make progress — the OpenAI WebSocket stream sends `response.cancel` and
/// tries to park its socket from *inside* the stream, which only runs while
/// someone polls it. No-op for providers that cancel by drop. The poll budget
/// ([`CancelBudget::stream`]) covers the provider's own `cancel_drain_ms`
/// deadline and is bounded under the turn's hard-abort grace.
async fn drain_cancelled_stream(
    session: &Arc<Session>,
    stream: &mut crate::provider::SessionCompletionStream,
    cancel: &TurnCancel,
) {
    if !session.model().requires_stream_cancel_drain() {
        return;
    }
    let budget = CancelBudget::for_reason(cancel.reason(), session.model().stream_cancel_drain());
    let _ = tokio::time::timeout(budget.stream, async {
        while stream.next().await.is_some() {}
    })
    .await;
}

/// Fold the cancelled attempt's partial output into `new_messages` and
/// finalize the interrupted turn. Buffered tool calls that were never
/// executed are completed with placeholder interrupted results so the
/// persisted assistant tool-call message keeps a matched tool result.
/// Only valid while the attempt's output has not been committed yet — paths
/// that already ran `commit_attempt_messages` for this attempt must call
/// [`finalize_interrupted_turn`] directly instead, or the attempt's text
/// would be committed twice.
async fn finalize_interrupted_attempt_and_turn(
    session: &Arc<Session>,
    ctx: &Arc<TurnContext>,
    attempt: &mut AttemptState,
    history_before_turn: Vec<Message>,
    mut new_messages: Vec<Message>,
    completions_by_index: &BTreeMap<usize, Vec<CompletionEntry>>,
) {
    attempt.finalize_reasoning();
    let requires_provider_reasoning_ids = session.model().requires_provider_reasoning_ids();
    let assistant_message = if attempt.saw_tool_call_this_attempt {
        build_assistant_tool_message(
            &attempt.turn_summary,
            &attempt.accumulated_text,
            &mut attempt.accumulated_reasoning,
            &attempt.pending_tool_calls,
            requires_provider_reasoning_ids,
        )
    } else {
        build_assistant_text_reasoning_message(
            &attempt.turn_summary,
            &attempt.accumulated_text,
            &mut attempt.accumulated_reasoning,
            requires_provider_reasoning_ids,
        )
    };
    let mut interrupted_results = Vec::new();
    for pending in std::mem::take(&mut attempt.pending_tool_calls) {
        interrupted_results.push(
            complete_interrupted_tool_call(
                Arc::clone(session),
                Arc::clone(ctx),
                PendingCallDescriptor::of(&pending),
            )
            .await,
        );
    }
    if let Some(message) = assistant_message {
        new_messages.push(message);
    }
    if let Some(message) = tool_results_to_user_message(interrupted_results) {
        new_messages.push(message);
    }
    finalize_interrupted_turn(
        session,
        ctx,
        history_before_turn,
        new_messages,
        completions_by_index,
    )
    .await;
}

/// Persist what the cancelled turn produced so far and swap it into in-memory
/// history, mirroring the clean terminal path — except that
/// `close_consumed_child_edges` is deliberately NOT called: an interrupted
/// turn leaves consumed-child edges open so resume reaps the finished
/// children (see the invariant comment on the terminal path in
/// [`run_manual_turn`]). With the partial turn persisted, the model sees its
/// own partial output and the interrupted tool results on the next turn.
async fn finalize_interrupted_turn(
    session: &Arc<Session>,
    ctx: &TurnContext,
    history_before_turn: Vec<Message>,
    new_messages: Vec<Message>,
    completions_by_index: &BTreeMap<usize, Vec<CompletionEntry>>,
) {
    let persisted = session
        .persist_turn_tail(&new_messages, completions_by_index)
        .await;
    let mut final_history = history_before_turn;
    final_history.extend(new_messages);
    session.replace_history(final_history).await;
    if !persisted {
        // Best-effort: the interrupted turn's partial tail did not fully persist.
        // Only log it — the turn's `TurnInterrupted` is emitted before this cleanup
        // runs, so an `Error` here would land after the partial tail (not pruning
        // it on resume) and would mislabel an interrupted turn as errored.
        tracing::warn!(
            thread_id = %session.id,
            turn_id = %ctx.sub_id,
            "interrupted turn-tail persistence failed; partial model history may be incomplete on resume"
        );
    }
}

/// Commit one stream attempt's output into the turn's `new_messages` (and the
/// `completions_by_index` side-table). Shared by the mid-stream-retry path and
/// the terminal tool-call path so the two cannot drift — the exact hazard this
/// extraction removes.
///
/// The caller has already finalized reasoning (`AttemptState::finalize_reasoning`)
/// before calling. This builds the attempt's assistant message, and — only when
/// the attempt issued tool calls — executes them, appends the rendered
/// `ToolResult` user message, and records the deferred-completion group. It is
/// deliberately **emission-free**: the retry path emits
/// `AgentMessageCompleted`/`StreamError` around this call, and the terminal
/// tool-call path emits nothing here. Returns whether cancellation was
/// observed while executing the batch — the committed messages are complete
/// either way (interrupted calls carry placeholder results), so the caller's
/// only job on `true` is to finalize the interrupted turn and stop.
#[allow(clippy::too_many_arguments)]
async fn commit_attempt_messages(
    session: &Arc<Session>,
    ctx: &Arc<TurnContext>,
    attempt: &mut AttemptState,
    new_messages: &mut Vec<Message>,
    completions_by_index: &mut BTreeMap<usize, Vec<CompletionEntry>>,
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
    consumed_children: &mut Vec<ThreadId>,
    stream_phase_completions: &mut Vec<CompletionEntry>,
    cancel: &TurnCancel,
) -> bool {
    let requires_provider_reasoning_ids = session.model().requires_provider_reasoning_ids();
    let assistant_message = if attempt.saw_tool_call_this_attempt {
        build_assistant_tool_message(
            &attempt.turn_summary,
            &attempt.accumulated_text,
            &mut attempt.accumulated_reasoning,
            &attempt.pending_tool_calls,
            requires_provider_reasoning_ids,
        )
    } else {
        build_assistant_text_reasoning_message(
            &attempt.turn_summary,
            &attempt.accumulated_text,
            &mut attempt.accumulated_reasoning,
            requires_provider_reasoning_ids,
        )
    };
    let (tool_results, cancelled) = if attempt.saw_tool_call_this_attempt {
        execute_pending_tool_calls_for_turn(
            Arc::clone(session),
            Arc::clone(ctx),
            std::mem::take(&mut attempt.pending_tool_calls),
            retained_subagents,
            consumed_children,
            stream_phase_completions,
            cancel,
        )
        .await
    } else {
        (Vec::new(), false)
    };
    if let Some(message) = assistant_message {
        new_messages.push(message);
    }
    if let Some(message) = tool_results_to_user_message(tool_results) {
        new_messages.push(message);
    }
    // Gated on `saw_tool_call_this_attempt`: a no-tool retry must leave
    // `stream_phase_completions` untouched for the terminal no-tool branches.
    if attempt.saw_tool_call_this_attempt {
        push_completion_group(
            new_messages,
            completions_by_index,
            std::mem::take(stream_phase_completions),
        );
    }
    cancelled
}

async fn execute_normal_tool_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending: PendingToolCall,
    cancel: &TurnCancel,
) -> ExecutedToolCall {
    let PendingToolCall {
        index,
        assistant_tool_call: _,
        tool_call,
        internal_call_id,
    } = pending;

    let model_for_call = session.model();
    // A child token scopes this one call: cancelling the turn cancels the
    // call, but the token handed to the tool can never outlive the turn it
    // belongs to.
    let call_token = cancel.child_token();
    let call_result = tools::with_tool_cancel_scope(
        call_token,
        model_for_call.call_tool(
            &tool_call.function.name,
            &tool_call.function.arguments.to_string(),
        ),
    )
    .await;
    let (tool_output, success, error, result_kind) = match call_result {
        Ok(output) => (output, true, None, ToolCallResultKind::Final),
        Err(err) => {
            let result_kind = if tool_error_is_interrupted(&err) {
                ToolCallResultKind::Interrupted
            } else {
                ToolCallResultKind::Final
            };
            let message = err.to_string();
            (message.clone(), false, Some(message), result_kind)
        }
    };

    let tool_name = tool_call.function.name.clone();
    complete_tool_call_with_kind(
        session,
        ctx,
        index,
        tool_call.id,
        tool_call.call_id,
        internal_call_id,
        tool_name,
        tool_output,
        success,
        error,
        result_kind,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn complete_tool_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    index: usize,
    tool_call_id: String,
    tool_call_call_id: Option<String>,
    internal_call_id: String,
    tool_name: String,
    tool_output: String,
    success: bool,
    error: Option<String>,
) -> ExecutedToolCall {
    complete_tool_call_with_kind(
        session,
        ctx,
        index,
        tool_call_id,
        tool_call_call_id,
        internal_call_id,
        tool_name,
        tool_output,
        success,
        error,
        ToolCallResultKind::Final,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn complete_tool_call_with_kind(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    index: usize,
    tool_call_id: String,
    tool_call_call_id: Option<String>,
    internal_call_id: String,
    tool_name: String,
    tool_output: String,
    success: bool,
    error: Option<String>,
    result_kind: ToolCallResultKind,
    related_thread_id: Option<ThreadId>,
) -> ExecutedToolCall {
    let decoded_output =
        decode_completed_tool_output(&tool_name, tool_output, success, result_kind);
    session
        .emit_event(
            &ctx,
            EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                thread_id: session.id.to_string(),
                turn_id: ctx.sub_id.clone(),
                call_id: internal_call_id,
                success,
                output_preview: Some(decoded_output.model_output.clone()),
                error,
                result_kind,
                related_thread_id,
                file_changes: decoded_output.file_changes,
                todos: decoded_output.todos,
            }),
        )
        .await;

    ExecutedToolCall {
        index,
        tool_result: tool_result(tool_call_id, tool_call_call_id, decoded_output.model_output),
    }
}

fn decode_completed_tool_output(
    tool_name: &str,
    tool_output: String,
    success: bool,
    result_kind: ToolCallResultKind,
) -> DecodedToolOutput {
    decode_tool_output_for_tool(
        tool_name,
        tool_output,
        success && matches!(result_kind, ToolCallResultKind::Final),
    )
}

/// Run the batch's immediate tools concurrently. On cancellation the in-flight
/// futures are not dropped on the spot: their per-call child tokens are already
/// cancelled, so cooperative tools (e.g. run_command killing its process group)
/// resolve within [`CancelBudget::tool`]; anything still outstanding at the
/// deadline is completed with a placeholder interrupted result so every call
/// in the batch ends with a `ToolCallCompleted` event and a tool result.
async fn execute_normal_tools_concurrently(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    normal_calls: Vec<PendingToolCall>,
    cancel: &TurnCancel,
) -> ToolBatchOutcome {
    let mut outstanding = normal_calls
        .iter()
        .map(PendingCallDescriptor::of)
        .collect::<Vec<_>>();
    let mut resolved = Vec::new();
    let mut cancelled = cancel.is_cancelled();

    if !cancelled {
        let mut pending_futures = normal_calls
            .into_iter()
            .map(|pending| {
                execute_normal_tool_call(Arc::clone(&session), Arc::clone(&ctx), pending, cancel)
            })
            .collect::<FuturesUnordered<_>>();
        while !pending_futures.is_empty() {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                next = pending_futures.next() => next,
            };
            let Some(executed) = next else {
                break;
            };
            outstanding.retain(|descriptor| descriptor.index != executed.index);
            resolved.push(executed);
        }
        if cancelled {
            let budget =
                CancelBudget::for_reason(cancel.reason(), session.model().stream_cancel_drain());
            let deadline = tokio::time::sleep(budget.tool);
            tokio::pin!(deadline);
            while !pending_futures.is_empty() {
                let next = tokio::select! {
                    _ = &mut deadline => break,
                    next = pending_futures.next() => next,
                };
                let Some(executed) = next else {
                    break;
                };
                outstanding.retain(|descriptor| descriptor.index != executed.index);
                resolved.push(executed);
            }
        }
    }

    if cancelled {
        for descriptor in outstanding {
            resolved.push(
                complete_interrupted_tool_call(Arc::clone(&session), Arc::clone(&ctx), descriptor)
                    .await,
            );
        }
    }

    ToolBatchOutcome {
        executed: resolved,
        cancelled,
    }
}

/// Execute the attempt's tool calls. Always produces one result per call —
/// real output, error, live spawn status, or a placeholder interrupted result
/// when the turn was cancelled — so the assistant tool-call message persisted
/// for this attempt keeps a matched tool result. The `cancelled` flag tells
/// the caller to finalize the interrupted turn instead of looping.
async fn execute_pending_tool_calls_for_turn(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending_tool_calls: Vec<PendingToolCall>,
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
    consumed_children: &mut Vec<ThreadId>,
    stream_phase_completions: &mut Vec<CompletionEntry>,
    cancel: &TurnCancel,
) -> (Vec<ExecutedToolCall>, bool) {
    let DispatchedTools {
        immediate,
        deferred,
        has_immediate_results,
        cancelled: dispatch_cancelled,
    } = dispatch_tool_calls(
        Arc::clone(&session),
        Arc::clone(&ctx),
        pending_tool_calls,
        cancel,
    )
    .await;

    let surfacing = Surfacing::for_batch(has_immediate_results);
    let (deferred, wait_cancelled) = wait_for_deferred(deferred, &surfacing, cancel).await;
    let deferred_results = collect_spawn_results(
        Arc::clone(&session),
        Arc::clone(&ctx),
        deferred,
        retained_subagents,
        consumed_children,
    )
    .await;

    let executed_tool_calls = merge_in_index_order(immediate, deferred_results);
    let mut cancelled = dispatch_cancelled || wait_cancelled;

    // A batch with no immediate results also blocks on every retained receiver
    // carried over from prior turns, so the model sees their completed JSON in
    // this same iteration. The drained completions ride along with anything
    // captured mid-stream and surface together as a single user-text message.
    // A cancelled batch skips the drain — the turn is ending.
    if matches!(surfacing, Surfacing::BlockInline) && !cancelled {
        tokio::select! {
            _ = cancel.cancelled() => cancelled = true,
            completions = drain_retained_subagent_completions(retained_subagents, &session.agent_control, consumed_children) => {
                stream_phase_completions.extend(completions);
            }
        }
    }

    (executed_tool_calls, cancelled)
}

struct DispatchedTools {
    /// Results ready now (re-sorted by model index on merge): normal tools,
    /// `exit_plan_mode`, and spawns that failed to start.
    immediate: Vec<ExecutedToolCall>,
    /// Spawned children whose completion is surfaced per the batch's `Surfacing`.
    deferred: Vec<StartedSpawnToolCall>,
    /// Whether the batch contained any non-deferred tool result — this, not
    /// whether a spawn happened to fail at start, is what decides `Surfacing`.
    has_immediate_results: bool,
    /// Whether cancellation was observed while executing the batch.
    cancelled: bool,
}

/// Run every tool call to the point where its outcome is known, preserving the
/// phasing the rest of the loop relies on: deferred (`spawn_agent`) starts run
/// first and are never cancelled mid-flight (their multi-await side effects must
/// fully complete or never begin — see [`start_spawn_calls_concurrently`]), then
/// the immediate tools run and observe cancellation. Every call gets a result
/// even when cancellation interrupts the batch.
async fn dispatch_tool_calls(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending_tool_calls: Vec<PendingToolCall>,
    cancel: &TurnCancel,
) -> DispatchedTools {
    let (normal_calls, spawn_calls, exit_plan_calls) =
        partition_pending_tool_calls(pending_tool_calls);

    let started_spawns =
        start_spawn_calls_concurrently(Arc::clone(&session), Arc::clone(&ctx), spawn_calls).await;

    // Spawn starts are uncancellable (above), so a cancel that arrived while
    // they ran produced children no subtree walk has seen — the cascade ran
    // before these threads existed. Cancel them here, carrying the recorded
    // reason of the abort that cancelled this turn so a shutdown/replacement
    // labels these children the same way it labelled everything else. The
    // children's drain is ALWAYS spawned, never awaited: this code runs
    // inside the turn task that is itself being drained under the same
    // deadline, so blocking on a stubborn child here would eat the parent's
    // whole grace and cost it the spawn results and partial-turn persistence
    // the grace exists to protect.
    if cancel.is_cancelled() {
        let started_child_ids = started_spawns
            .iter()
            .filter_map(|spawn| match spawn {
                SpawnToolStart::Started(call) => Some(call.child_thread_id),
                SpawnToolStart::Completed(_) => None,
            })
            .collect::<Vec<_>>();
        let reason = cancel.reason();
        let (_interrupted, child_tasks) = session
            .agent_control
            .abort_threads(&started_child_ids, reason)
            .await;
        tokio::spawn(crate::core::drain_aborted_tasks(
            child_tasks,
            reason.drain_grace(),
        ));
    }

    let normal_outcome = execute_normal_tools_concurrently(
        Arc::clone(&session),
        Arc::clone(&ctx),
        normal_calls,
        cancel,
    )
    .await;

    let exit_plan_outcome = execute_exit_plan_mode_calls(
        Arc::clone(&session),
        Arc::clone(&ctx),
        exit_plan_calls,
        cancel,
    )
    .await;

    let cancelled = normal_outcome.cancelled || exit_plan_outcome.cancelled;
    let has_immediate_results =
        !normal_outcome.executed.is_empty() || !exit_plan_outcome.executed.is_empty();
    let (deferred, immediate_spawn_results) = split_started_and_immediate(started_spawns);

    let mut immediate = normal_outcome.executed;
    immediate.extend(exit_plan_outcome.executed);
    immediate.extend(immediate_spawn_results);

    DispatchedTools {
        immediate,
        deferred,
        has_immediate_results,
        cancelled,
    }
}

/// How the turn loop runs a given tool call. The names of the built-in tools
/// that get special treatment live only here, so adding or changing one is a
/// localized edit rather than a string match scattered through the loop.
enum ToolClass {
    /// Executed for an immediate result (read, edit, write, run_command, …).
    Immediate,
    /// Starts asynchronous work whose completion is surfaced later.
    Deferred,
    /// Mutates session state, then yields an immediate result.
    SessionMutation,
}

fn classify_tool(tool_name: &str) -> ToolClass {
    match tool_name {
        "spawn_agent" => ToolClass::Deferred,
        "exit_plan_mode" => ToolClass::SessionMutation,
        _ => ToolClass::Immediate,
    }
}

fn partition_pending_tool_calls(
    pending_tool_calls: Vec<PendingToolCall>,
) -> (
    Vec<PendingToolCall>,
    Vec<PendingToolCall>,
    Vec<PendingToolCall>,
) {
    let mut normal = Vec::new();
    let mut spawn = Vec::new();
    let mut exit_plan = Vec::new();
    for pending in pending_tool_calls {
        match classify_tool(&pending.tool_call.function.name) {
            ToolClass::Deferred => spawn.push(pending),
            ToolClass::SessionMutation => exit_plan.push(pending),
            ToolClass::Immediate => normal.push(pending),
        }
    }
    (normal, spawn, exit_plan)
}

/// Handle one queued `exit_plan_mode` tool call: read the plan written by
/// `plan_write`, present it to the user for approval, and only on an explicit
/// approval flip plan mode off so the rest of this turn (and the next) sees
/// the full tool set. A rejection keeps the session in plan mode and surfaces
/// the user's feedback to the model as the tool result. Returns `None` only
/// on cancellation.
async fn execute_exit_plan_mode_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending: PendingToolCall,
    cancel: &TurnCancel,
) -> Option<ExecutedToolCall> {
    let PendingToolCall {
        index,
        assistant_tool_call: _,
        tool_call,
        internal_call_id,
    } = pending;

    // The model may attach a short `reason` to `exit_plan_mode`; surface it to
    // the user alongside the plan (it never reaches them otherwise).
    let reason = tool_call
        .function
        .arguments
        .get("reason")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    let (tool_output, success, error) =
        exit_plan_mode_outcome(&session, &ctx, internal_call_id.clone(), reason, cancel).await?;

    Some(
        complete_tool_call(
            session,
            ctx,
            index,
            tool_call.id,
            tool_call.call_id,
            internal_call_id,
            "exit_plan_mode".to_string(),
            tool_output,
            success,
            error,
        )
        .await,
    )
}

/// Decide the `exit_plan_mode` result: `(output, success, error)`. Returns
/// `None` only on cancellation while waiting for the user's decision.
async fn exit_plan_mode_outcome(
    session: &Arc<Session>,
    ctx: &Arc<TurnContext>,
    internal_call_id: String,
    reason: Option<String>,
    cancel: &TurnCancel,
) -> Option<(String, bool, Option<String>)> {
    let failure = |message: String| Some((message.clone(), false, Some(message)));

    if !session.plan_mode() {
        return failure("exit_plan_mode failed: the session is not in plan mode".to_string());
    }

    let plan_path = tools::plan_file_path(&session.cwd, session.id);
    let plan = match tokio::fs::read_to_string(&plan_path).await {
        Ok(plan) if !plan.trim().is_empty() => plan,
        Ok(_) | Err(_) => {
            return failure(
                "exit_plan_mode failed: no plan found — write your plan with `plan_write` \
                 before exiting plan mode"
                    .to_string(),
            );
        }
    };

    let Some(ask_user_client) = session.ask_user_client() else {
        return failure(
            "exit_plan_mode failed: plan approval requires an interactive client".to_string(),
        );
    };

    let params = RequestPlanApprovalParams {
        thread_id: session.id.to_string(),
        turn_id: ctx.sub_id.clone(),
        call_id: internal_call_id,
        // Keep `plan` owned for the approval fallback below.
        plan: plan.clone(),
        reason,
    };
    let decision = tokio::select! {
        _ = cancel.cancelled() => return None,
        decision = ask_user_client.request_plan_approval(params) => decision,
    };

    match decision {
        Ok(response) => match response.decision {
            PlanApprovalDecision::Approved => {
                match session.apply_plan_mode_unchecked(false).await {
                    Ok(_) => {
                        // Re-read the plan file so any edits the user made while
                        // reviewing (Ctrl+G → $EDITOR in the approval overlay)
                        // are what the model implements. Fall back to the plan
                        // as originally submitted if the re-read is empty/fails.
                        let approved_plan = tokio::fs::read_to_string(&plan_path)
                            .await
                            .ok()
                            .filter(|reread| !reread.trim().is_empty())
                            .unwrap_or(plan);
                        Some((
                            format!(
                                "Plan approved by the user. Plan mode is off — implement the \
                                 plan now with the full tool set.\n\nApproved plan:\n{approved_plan}"
                            ),
                            true,
                            None,
                        ))
                    }
                    Err(err) => failure(err.to_string()),
                }
            }
            // A rejection is a valid outcome, not a tool failure: the model
            // should revise the plan, not treat the call as errored.
            PlanApprovalDecision::Rejected => {
                let feedback = response
                    .feedback
                    .as_deref()
                    .unwrap_or("(none provided)")
                    .to_string();
                Some((
                    format!(
                        "The user rejected the plan. You are still in plan mode. Revise the \
                         plan per the feedback, update it with `plan_write`, then call \
                         `exit_plan_mode` again.\nUser feedback: {feedback}"
                    ),
                    true,
                    None,
                ))
            }
        },
        Err(err) => failure(format!("exit_plan_mode failed: {}", err.message)),
    }
}

/// Run the queued `exit_plan_mode` calls sequentially (a second call in the
/// same batch sees the state left by the first). On cancellation, the call
/// being waited on and any not yet started are completed with placeholder
/// interrupted results.
async fn execute_exit_plan_mode_calls(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    exit_plan_calls: Vec<PendingToolCall>,
    cancel: &TurnCancel,
) -> ToolBatchOutcome {
    let mut resolved = Vec::with_capacity(exit_plan_calls.len());
    let mut cancelled = false;
    for pending in exit_plan_calls {
        if cancelled || cancel.is_cancelled() {
            cancelled = true;
            resolved.push(
                complete_interrupted_tool_call(
                    Arc::clone(&session),
                    Arc::clone(&ctx),
                    PendingCallDescriptor::of(&pending),
                )
                .await,
            );
            continue;
        }
        let descriptor = PendingCallDescriptor::of(&pending);
        match execute_exit_plan_mode_call(Arc::clone(&session), Arc::clone(&ctx), pending, cancel)
            .await
        {
            Some(executed) => resolved.push(executed),
            None => {
                cancelled = true;
                resolved.push(
                    complete_interrupted_tool_call(
                        Arc::clone(&session),
                        Arc::clone(&ctx),
                        descriptor,
                    )
                    .await,
                );
            }
        }
    }
    ToolBatchOutcome {
        executed: resolved,
        cancelled,
    }
}

/// Start every queued `spawn_agent` call concurrently and wait for all starts
/// to finish before returning. We deliberately do NOT race this against the
/// cancellation token: `start_spawn_tool_call` -> `spawn_agent_for_tool`
/// performs side effects across multiple awaits (emits `CollabAgentSpawnBegin`,
/// reserves a registry path, inserts the child thread, registers status/inline
/// waiters, submits initial input, commits registry metadata, writes a DB
/// edge). Dropping any of those futures mid-flight can leak threads, leave a
/// dangling registry reservation, or drop the matching `CollabAgentSpawnEnd`.
/// The outer `run_manual_turn` loop observes cancellation at its next
/// checkpoint after all starts complete cleanly.
async fn start_spawn_calls_concurrently(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    spawn_calls: Vec<PendingToolCall>,
) -> Vec<SpawnToolStart> {
    if spawn_calls.is_empty() {
        return Vec::new();
    }
    let futures = spawn_calls
        .into_iter()
        .map(|pending| start_spawn_tool_call(Arc::clone(&session), Arc::clone(&ctx), pending));
    futures_util::future::join_all(futures).await
}

fn split_started_and_immediate(
    spawns: Vec<SpawnToolStart>,
) -> (Vec<StartedSpawnToolCall>, Vec<ExecutedToolCall>) {
    let mut started = Vec::new();
    let mut immediate = Vec::new();
    for spawn in spawns {
        match spawn {
            SpawnToolStart::Started(call) => started.push(*call),
            SpawnToolStart::Completed(executed) => immediate.push(*executed),
        }
    }
    (started, immediate)
}

/// Wait for the started spawns per the batch's surfacing policy. On
/// cancellation the wait simply stops early — the spawns themselves stay
/// valid: `collect_spawn_results` reports each child's live status as a
/// `StatusUpdate` result, so the batch still produces a result per call.
/// Returns the spawn calls plus whether cancellation cut the wait short.
async fn wait_for_deferred(
    mut deferred: Vec<StartedSpawnToolCall>,
    surfacing: &Surfacing,
    cancel: &TurnCancel,
) -> (Vec<StartedSpawnToolCall>, bool) {
    if deferred.is_empty() {
        return (deferred, false);
    }
    let cancelled = await_deferred_completions(&mut deferred, surfacing, cancel).await;
    (deferred, cancelled)
}

async fn collect_spawn_results(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    spawn_calls: Vec<StartedSpawnToolCall>,
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
    consumed_children: &mut Vec<ThreadId>,
) -> Vec<ExecutedToolCall> {
    let mut resolved = Vec::with_capacity(spawn_calls.len());
    for mut spawn_call in spawn_calls {
        let child_thread_id = spawn_call.child_thread_id;
        // Classify the spawn's outcome at collection time:
        //  (a) a completion captured during the wait window -> final;
        //  (b) no completion captured, but the child has since reached a
        //      terminal status -> final now, dropping the redundant waiter so
        //      the retained drain cannot surface the same completion twice;
        //  (c) still live -> report live status and retain the waiter.
        // A spawn resolved as final in (a)/(b) is consumed: release its
        // in-memory resources now and record it so its durable edge is closed
        // once the turn's result is persisted. A retained spawn is released when
        // its waiter is later drained.
        let (tool_output, success, error, result_kind, related_thread_id, consumed) =
            if let Some(completion) = spawn_call.completion.as_ref() {
                let (tool_output, success, error) =
                    encode_completed_spawn_result(&spawn_call.metadata, completion);
                (
                    tool_output,
                    success,
                    error,
                    ToolCallResultKind::Final,
                    Some(child_thread_id),
                    true,
                )
            } else {
                // `NotFound` means the live status is not observable yet; fall
                // back to the spawn's initial (live) status, matching how a
                // running child is reported.
                let observed = match session.agent_control.get_status(child_thread_id) {
                    AgentStatus::NotFound => spawn_call.initial_status.clone(),
                    status => status,
                };
                let (tool_output, success, error) =
                    encode_spawn_agent_result(&spawn_call.metadata, &observed, None)
                        .map(|output| (output, true, None))
                        .unwrap_or_else(|message| (message.clone(), false, Some(message)));
                if is_final(&observed) {
                    let _ = spawn_call.waiter.take();
                    (
                        tool_output,
                        success,
                        error,
                        ToolCallResultKind::Final,
                        Some(child_thread_id),
                        true,
                    )
                } else {
                    if let Some(waiter) = spawn_call.waiter.take() {
                        retained_subagents.push(RetainedSpawnCompletion {
                            metadata: spawn_call.metadata.clone(),
                            waiter,
                        });
                    }
                    (
                        tool_output,
                        success,
                        error,
                        ToolCallResultKind::StatusUpdate,
                        Some(child_thread_id),
                        false,
                    )
                }
            };
        let executed = complete_tool_call_with_kind(
            Arc::clone(&session),
            Arc::clone(&ctx),
            spawn_call.index,
            spawn_call.tool_call_id,
            spawn_call.tool_call_call_id,
            spawn_call.internal_call_id,
            spawn_call.tool_name,
            tool_output,
            success,
            error,
            result_kind,
            related_thread_id,
        )
        .await;
        resolved.push(executed);
        if consumed {
            consumed_children.push(consume_child(&session.agent_control, child_thread_id).await);
        }
    }
    resolved
}

fn merge_in_index_order(
    mut normal: Vec<ExecutedToolCall>,
    spawn: Vec<ExecutedToolCall>,
) -> Vec<ExecutedToolCall> {
    normal.extend(spawn);
    normal.sort_by_key(|executed| executed.index);
    normal
}

enum SpawnToolStart {
    Started(Box<StartedSpawnToolCall>),
    Completed(Box<ExecutedToolCall>),
}

async fn start_spawn_tool_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending: PendingToolCall,
) -> SpawnToolStart {
    let PendingToolCall {
        index,
        assistant_tool_call: _,
        tool_call,
        internal_call_id,
    } = pending;
    let tool_name = tool_call.function.name.clone();
    let args = match serde_json::from_value::<SubagentArgs>(tool_call.function.arguments.clone()) {
        Ok(args) => args,
        Err(err) => {
            let message = format!("invalid {tool_name} args: {err}");
            return SpawnToolStart::Completed(Box::new(
                complete_tool_call(
                    session,
                    ctx,
                    index,
                    tool_call.id,
                    tool_call.call_id,
                    internal_call_id,
                    tool_name,
                    message.clone(),
                    false,
                    Some(message),
                )
                .await,
            ));
        }
    };
    let SubagentArgs {
        description: _,
        prompt,
        subagent_type,
    } = args;
    // Plan mode must hold for the whole agent subtree: Explore children are
    // structurally read-only (their tool registry is read + run_command only),
    // so every spawn during plan mode is coerced to Explore regardless of the
    // requested subagent_type.
    let system_prompt_kind = if session.plan_mode() {
        SystemPromptKind::Explore
    } else {
        subagent_type_to_prompt_kind(subagent_type.as_deref())
    };

    match session
        .agent_control
        .spawn_agent_for_tool(
            session.id,
            prompt,
            system_prompt_kind,
            internal_call_id.clone(),
        )
        .await
    {
        Ok((metadata, initial_status, waiter)) => {
            let Some(child_thread_id) = metadata.agent_id else {
                let message = "spawned agent metadata should have a thread id".to_string();
                return SpawnToolStart::Completed(Box::new(
                    complete_tool_call(
                        session,
                        ctx,
                        index,
                        tool_call.id,
                        tool_call.call_id,
                        internal_call_id,
                        tool_name,
                        message.clone(),
                        false,
                        Some(message),
                    )
                    .await,
                ));
            };
            SpawnToolStart::Started(Box::new(StartedSpawnToolCall {
                index,
                tool_call_id: tool_call.id,
                tool_call_call_id: tool_call.call_id,
                internal_call_id,
                child_thread_id,
                tool_name,
                metadata,
                initial_status,
                waiter: Some(waiter),
                completion: None,
            }))
        }
        Err(err) => {
            let message = err.to_string();
            SpawnToolStart::Completed(Box::new(
                complete_tool_call(
                    session,
                    ctx,
                    index,
                    tool_call.id,
                    tool_call.call_id,
                    internal_call_id,
                    tool_name,
                    message.clone(),
                    false,
                    Some(message),
                )
                .await,
            ))
        }
    }
}

fn subagent_type_to_prompt_kind(subagent_type: Option<&str>) -> SystemPromptKind {
    match subagent_type {
        Some("Explore" | "explore") => SystemPromptKind::Explore,
        _ => SystemPromptKind::DefaultSubagent,
    }
}

/// How a batch's deferred tool effects (currently only `spawn_agent`) surface
/// this turn. A batch with no immediate tool results blocks until the deferred
/// effects finish, so the model sees their final results in the same iteration;
/// a mixed batch shows live status after a short grace period and retains the
/// rest to surface later as a follow-up user-text message.
enum Surfacing {
    BlockInline,
    GraceThenRetain(Duration),
}

impl Surfacing {
    fn for_batch(has_immediate_results: bool) -> Self {
        if has_immediate_results {
            Self::GraceThenRetain(Duration::from_secs(1))
        } else {
            Self::BlockInline
        }
    }
}

/// Wait for spawn completions per the surfacing policy. Returns whether the
/// wait was cut short by cancellation.
async fn await_deferred_completions(
    spawn_tool_calls: &mut [StartedSpawnToolCall],
    surfacing: &Surfacing,
    cancel: &TurnCancel,
) -> bool {
    match surfacing {
        Surfacing::BlockInline => {
            while spawn_tool_calls.iter().any(|call| call.waiter.is_some()) {
                let completion = tokio::select! {
                    _ = cancel.cancelled() => return true,
                    completion = next_started_spawn_completion(spawn_tool_calls) => completion,
                };
                let Some((index, completion)) = completion else {
                    break;
                };
                spawn_tool_calls[index].completion = Some(completion);
            }
        }
        Surfacing::GraceThenRetain(duration) => {
            let deadline = tokio::time::sleep(*duration);
            tokio::pin!(deadline);
            while spawn_tool_calls.iter().any(|call| call.waiter.is_some()) {
                let completion = tokio::select! {
                    _ = cancel.cancelled() => return true,
                    _ = &mut deadline => break,
                    completion = next_started_spawn_completion(spawn_tool_calls) => completion,
                };
                let Some((index, completion)) = completion else {
                    break;
                };
                spawn_tool_calls[index].completion = Some(completion);
            }
        }
    }
    false
}

async fn next_started_spawn_completion(
    spawn_tool_calls: &mut [StartedSpawnToolCall],
) -> Option<(usize, Result<InlineChildCompletion, String>)> {
    let futures = spawn_tool_calls
        .iter_mut()
        .enumerate()
        .filter_map(|(index, call)| {
            call.waiter.as_mut().map(|waiter| {
                waiter.map(move |result| {
                    (
                        index,
                        result.map_err(|err| {
                            format!("spawn_agent inline wait failed to resolve: {err}")
                        }),
                    )
                })
            })
        })
        .collect::<Vec<_>>();
    if futures.is_empty() {
        return None;
    }

    let ((index, completion), _selected_index, remaining) = select_all(futures).await;
    drop(remaining);
    spawn_tool_calls[index].waiter = None;
    Some((index, completion))
}

fn encode_completed_spawn_result(
    metadata: &AgentMetadata,
    completion: &Result<InlineChildCompletion, String>,
) -> (String, bool, Option<String>) {
    match completion {
        Ok(completion) => encode_spawn_agent_result(
            metadata,
            &completion.status,
            completion.last_assistant_message.clone(),
        )
        .map(|output| (output, true, None))
        .unwrap_or_else(|message| (message.clone(), false, Some(message))),
        Err(message) => (message.clone(), false, Some(message.clone())),
    }
}

/// Race every retained subagent receiver in parallel and resolve the first
/// completion. Returns the rendered completion JSON for whichever finished
/// first and removes it from `retained_subagents`; remaining receivers stay
/// in the vec for the next race. Returns `None` only when the vec is empty
/// (callers should already gate the await on `!retained_subagents.is_empty()`
/// because `select_all` panics on an empty iterator).
async fn next_retained_subagent_completion(
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
    agent_control: &AgentControl,
    consumed_children: &mut Vec<ThreadId>,
) -> Option<CompletionEntry> {
    if retained_subagents.is_empty() {
        return None;
    }
    let futures = retained_subagents
        .iter_mut()
        .enumerate()
        .map(|(index, entry)| (&mut entry.waiter).map(move |result| (index, result)))
        .collect::<Vec<_>>();
    let ((index, result), _selected, remaining) = select_all(futures).await;
    drop(remaining);
    let entry = retained_subagents.swap_remove(index);
    let child_thread_id = entry.metadata.agent_id;
    let completion = result.map_err(|err| {
        child_thread_id.map_or_else(
            || format!("spawn_agent inline wait failed to resolve: {err}"),
            |thread_id| {
                format!("spawn_agent inline wait failed to resolve for child {thread_id}: {err}")
            },
        )
    });
    if let Some(child_thread_id) = child_thread_id {
        consumed_children.push(consume_child(agent_control, child_thread_id).await);
    }
    Some(CompletionEntry::from_inline(&entry.metadata, completion))
}

async fn drain_retained_subagent_completions(
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
    agent_control: &AgentControl,
    consumed_children: &mut Vec<ThreadId>,
) -> Vec<CompletionEntry> {
    let retained = std::mem::take(retained_subagents);
    // Each retained child resolves and is released concurrently; the in-memory
    // release is internally synchronized, but the `consumed_children` record is
    // accumulated sequentially after the join to avoid aliasing it across tasks.
    let completions = retained.into_iter().map(|retained| async move {
        let RetainedSpawnCompletion { metadata, waiter } = retained;
        let child_thread_id = metadata.agent_id;
        let completion = waiter.await.map_err(|err| {
            child_thread_id.map_or_else(
                || format!("spawn_agent inline wait failed to resolve: {err}"),
                |thread_id| {
                    format!(
                        "spawn_agent inline wait failed to resolve for child {thread_id}: {err}"
                    )
                },
            )
        });
        if let Some(child_thread_id) = child_thread_id {
            consume_child(agent_control, child_thread_id).await;
        }
        (
            CompletionEntry::from_inline(&metadata, completion),
            child_thread_id,
        )
    });
    let resolved = futures_util::future::join_all(completions).await;
    let mut entries = Vec::with_capacity(resolved.len());
    for (entry, child_thread_id) in resolved {
        if let Some(child_thread_id) = child_thread_id {
            consumed_children.push(child_thread_id);
        }
        entries.push(entry);
    }
    entries
}

/// Release a consumed child's in-memory resources (actor, registry slot, status
/// channel). Best-effort: a failure is logged, never fatal to the turn. The
/// durable parent→child edge is deliberately left open here and closed only by
/// [`close_consumed_child_edges`] after the turn's result is persisted.
async fn release_consumed_child(agent_control: &AgentControl, child_thread_id: ThreadId) {
    if let Err(err) = agent_control.release_consumed_agent(child_thread_id).await {
        tracing::debug!(
            %child_thread_id,
            error = %err,
            "failed to release consumed subagent resources"
        );
    }
}

/// Release a consumed child's in-memory resources (via [`release_consumed_child`])
/// and return its id so the caller can record it for the post-persist edge close.
/// Returning the id — rather than taking `&mut consumed_children` — is what lets
/// the concurrent drain in [`drain_retained_subagent_completions`] call this
/// inside each per-child future while still accumulating `consumed_children`
/// sequentially after the join (a shared `&mut Vec` cannot cross those futures).
async fn consume_child(agent_control: &AgentControl, child_thread_id: ThreadId) -> ThreadId {
    release_consumed_child(agent_control, child_thread_id).await;
    child_thread_id
}

/// Close the durable parent→child edges of children consumed during this turn,
/// now that the turn's result is persisted. Called only on the clean terminal
/// path, so an interrupted/crashed turn leaves edges open and resume reaps the
/// finished children instead. Best-effort per edge.
async fn close_consumed_child_edges(session: &Arc<Session>, consumed_children: &[ThreadId]) {
    for child_thread_id in consumed_children {
        if let Err(err) = session
            .agent_control
            .close_consumed_agent_edge(session.id, *child_thread_id)
            .await
        {
            tracing::debug!(
                %child_thread_id,
                error = %err,
                "failed to close consumed subagent edge after persisting turn"
            );
        }
    }
}

fn append_completion_texts_after_assistant_turn(
    new_messages: &mut Vec<Message>,
    completions_by_index: &mut BTreeMap<usize, Vec<CompletionEntry>>,
    turn_summary: &SessionTurnSummary,
    accumulated_text: &str,
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    requires_provider_reasoning_ids: bool,
    completion_entries: Vec<CompletionEntry>,
) {
    if let Some(message) = build_assistant_text_reasoning_message(
        turn_summary,
        accumulated_text,
        accumulated_reasoning,
        requires_provider_reasoning_ids,
    ) {
        new_messages.push(message);
    }
    push_completion_group(new_messages, completions_by_index, completion_entries);
}

/// Render a completion group into the model-facing `Message::User` and push it to
/// `new_messages`, recording its index so the turn-end persist writes a typed
/// `SubagentCompletion` record there. The index is recorded immediately before
/// the matching push and `new_messages` is append-only within a turn, so the two
/// cannot drift. An empty group pushes nothing (and records nothing).
fn push_completion_group(
    new_messages: &mut Vec<Message>,
    completions_by_index: &mut BTreeMap<usize, Vec<CompletionEntry>>,
    entries: Vec<CompletionEntry>,
) {
    if let Some(message) = completion_entries_to_user_message(&entries) {
        completions_by_index.insert(new_messages.len(), entries);
        new_messages.push(message);
    }
}

fn assistant_item_id_for_attempt(ctx: &TurnContext, stream_retries: usize) -> String {
    if stream_retries == 0 {
        ctx.assistant_item_id.clone()
    } else {
        format!("{}#{stream_retries}", ctx.assistant_item_id)
    }
}

fn should_continue_after_stream_error(
    err: &anyhow::Error,
    stream_retries: usize,
    retry_budget: usize,
    saw_assistant_item_this_attempt: bool,
) -> bool {
    saw_assistant_item_this_attempt
        && stream_retries < retry_budget
        && err
            .downcast_ref::<CompletionError>()
            .is_some_and(is_openai_websocket_transient_start_error)
}

fn assistant_text_for_message(turn_summary: &SessionTurnSummary, accumulated_text: &str) -> String {
    if turn_summary.response.is_empty() {
        accumulated_text.to_string()
    } else {
        turn_summary.response.clone()
    }
}

fn build_assistant_text_reasoning_message(
    turn_summary: &SessionTurnSummary,
    accumulated_text: &str,
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    requires_provider_reasoning_ids: bool,
) -> Option<Message> {
    let mut content_items = Vec::new();
    let assistant_text = assistant_text_for_message(turn_summary, accumulated_text);
    if !assistant_text.is_empty() {
        content_items.push(AssistantContent::text(assistant_text));
    }
    for reasoning in accumulated_reasoning.drain(..) {
        if should_roundtrip_reasoning(requires_provider_reasoning_ids, &reasoning) {
            content_items.push(AssistantContent::Reasoning(reasoning));
        }
    }
    if content_items.is_empty() {
        return None;
    }
    Some(Message::Assistant {
        id: turn_summary.assistant_message_id.clone(),
        content: OneOrMany::many(content_items).ok()?,
    })
}

fn build_request_parts(
    history_before_turn: &[Message],
    project_instructions: Option<&ProjectInstructions>,
    skills: &[SkillMeta],
    new_messages: &[Message],
) -> Option<(Message, Vec<Message>)> {
    let (pending_prompt, new_history) = new_messages.split_last()?;
    let mut request_history = Vec::new();
    if let Some(message) = project_instructions_message(project_instructions) {
        request_history.push(message);
    }
    if let Some(message) = skills_context_message(skills) {
        request_history.push(message);
    }
    request_history.extend_from_slice(history_before_turn);
    request_history.extend_from_slice(new_history);
    Some((pending_prompt.clone(), request_history))
}

/// Synthetic request-only user message advertising the project's skills, like
/// `project_instructions_message` it is rebuilt per request and never
/// persisted to history, so the list self-refreshes every turn.
fn skills_context_message(skills: &[SkillMeta]) -> Option<Message> {
    if skills.is_empty() {
        return None;
    }
    let listing = skills
        .iter()
        .map(|skill| format!("- {}: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!(
        "# Available skills\n\nThe following user-defined skills are available, drawn from your user-global skills directory and this project (a project skill overrides a same-named global one). When a request matches a skill's purpose, invoke the `skill` tool with its name and follow the returned instructions.\n\n{listing}"
    );
    Some(Message::User {
        content: OneOrMany::one(UserContent::Text(Text {
            text,
            additional_params: None,
        })),
    })
}

/// If the prompt starts with `/name` where `name` is a skill found in
/// `skill_roots` (the user-global and project skill directories, project last
/// so it wins), return the expanded model-facing text (skill instructions +
/// remaining text as arguments). Any other text — including unknown `/foo`
/// commands — passes through unchanged.
fn expand_skill_invocation(
    skill_roots: &[std::path::PathBuf],
    prompt_text: &str,
    max_skill_bytes: usize,
) -> Option<String> {
    let trimmed = prompt_text.trim_start();
    let candidate = trimmed.strip_prefix('/')?;
    let name_end = candidate
        .find(char::is_whitespace)
        .unwrap_or(candidate.len());
    let (name, args) = candidate.split_at(name_end);
    let skill = tools::load_skill(skill_roots, name, max_skill_bytes)?;
    Some(tools::render_skill_invocation(&skill, Some(args)))
}

fn project_instructions_message(
    project_instructions: Option<&ProjectInstructions>,
) -> Option<Message> {
    let instructions = project_instructions?;
    let text = instructions
        .entries
        .iter()
        .map(|entry| {
            // `source_path` remains persisted metadata for resume/debug; the
            // model-facing header names the scoped directory like Codex.
            format!(
                "# AGENTS.md instructions for {}\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
                entry.directory, entry.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(Message::User {
        content: OneOrMany::one(UserContent::Text(Text {
            text,
            additional_params: None,
        })),
    })
}

fn merge_reasoning_blocks(
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    incoming: &MessageReasoning,
) {
    let ids_match = |existing: &MessageReasoning| {
        matches!(
            (&existing.id, &incoming.id),
            (Some(existing_id), Some(incoming_id)) if existing_id == incoming_id
        )
    };

    if let Some(existing) = accumulated_reasoning
        .iter_mut()
        .rev()
        .find(|existing| ids_match(existing))
    {
        existing.content.extend(incoming.content.clone());
    } else {
        accumulated_reasoning.push(incoming.clone());
    }
}

fn should_roundtrip_reasoning(
    requires_provider_reasoning_ids: bool,
    reasoning: &MessageReasoning,
) -> bool {
    !requires_provider_reasoning_ids || reasoning.id.is_some()
}

fn build_assistant_tool_message(
    turn_summary: &SessionTurnSummary,
    accumulated_text: &str,
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    pending_tool_calls: &[PendingToolCall],
    requires_provider_reasoning_ids: bool,
) -> Option<Message> {
    let mut content_items = Vec::new();
    let assistant_text = assistant_text_for_message(turn_summary, accumulated_text);
    if !assistant_text.is_empty() {
        content_items.push(AssistantContent::text(assistant_text));
    }
    for reasoning in accumulated_reasoning.drain(..) {
        if should_roundtrip_reasoning(requires_provider_reasoning_ids, &reasoning) {
            content_items.push(AssistantContent::Reasoning(reasoning));
        }
    }
    content_items.extend(
        pending_tool_calls
            .iter()
            .map(|pending| pending.assistant_tool_call.clone()),
    );
    if content_items.is_empty() {
        return None;
    }
    Some(Message::Assistant {
        id: turn_summary.assistant_message_id.clone(),
        content: OneOrMany::many(content_items).ok()?,
    })
}

fn tool_results_to_user_message(executed_tool_calls: Vec<ExecutedToolCall>) -> Option<Message> {
    let content = executed_tool_calls
        .into_iter()
        .map(|executed| UserContent::ToolResult(executed.tool_result))
        .collect::<Vec<_>>();
    OneOrMany::many(content)
        .ok()
        .map(|content| Message::User { content })
}

fn tool_result(id: String, call_id: Option<String>, tool_result: String) -> ToolResult {
    ToolResult {
        id,
        call_id,
        content: ToolResultContent::from_tool_output(tool_result),
    }
}

#[cfg(test)]
mod tests {
    use cazean_protocol::{FileChange, FileChangeOutput, ToolCallResultKind};
    use rig::message::Reasoning as MessageReasoning;
    use tools::{SubagentArgs, encode_tool_output};

    use crate::agent::SystemPromptKind;

    use super::{
        PendingReasoningDeltas, decode_completed_tool_output, expand_skill_invocation,
        should_roundtrip_reasoning, skills_context_message, subagent_type_to_prompt_kind,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn write_skill(root: &std::path::Path, name: &str, content: &str) -> TestResult {
        let dir = tools::project_skills_dir(root).join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), content)?;
        Ok(())
    }

    #[test]
    fn expand_skill_invocation_expands_matching_skill() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(
            temp.path(),
            "deploy",
            "---\ndescription: Deploy the app\n---\nRun make deploy.",
        )?;
        let roots = vec![tools::project_skills_dir(temp.path())];

        let Some(expanded) = expand_skill_invocation(&roots, "/deploy to staging", 64 * 1024)
        else {
            panic!("expected /deploy to expand");
        };
        assert!(expanded.starts_with("<skill-invocation skill=\"deploy\">"));
        assert!(expanded.contains("Run make deploy."));
        assert!(expanded.ends_with("to staging"));

        let Some(expanded) = expand_skill_invocation(&roots, "/deploy", 64 * 1024) else {
            panic!("expected bare /deploy to expand");
        };
        assert!(expanded.ends_with("(no additional arguments)"));
        Ok(())
    }

    #[test]
    fn expand_skill_invocation_keeps_multiline_args() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(temp.path(), "deploy", "---\ndescription: Deploy\n---\nbody")?;
        let roots = vec![tools::project_skills_dir(temp.path())];

        let Some(expanded) =
            expand_skill_invocation(&roots, "/deploy to staging\nwith care", 64 * 1024)
        else {
            panic!("expected /deploy to expand");
        };
        assert!(expanded.ends_with("to staging\nwith care"));
        Ok(())
    }

    #[test]
    fn expand_skill_invocation_passes_through_non_matches() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(temp.path(), "deploy", "---\ndescription: Deploy\n---\nbody")?;
        let roots = vec![tools::project_skills_dir(temp.path())];

        // Unknown skill, plain text, and bare slash all pass through.
        assert!(expand_skill_invocation(&roots, "/unknown args", 64 * 1024).is_none());
        assert!(expand_skill_invocation(&roots, "deploy the app", 64 * 1024).is_none());
        assert!(expand_skill_invocation(&roots, "/", 64 * 1024).is_none());
        // A slash later in the text does not trigger expansion.
        assert!(expand_skill_invocation(&roots, "run /deploy", 64 * 1024).is_none());
        Ok(())
    }

    #[test]
    fn skills_context_message_lists_skills_and_omits_when_empty() {
        assert!(skills_context_message(&[]).is_none());

        let skills = vec![tools::SkillMeta {
            name: "deploy".to_string(),
            description: "Deploy the app".to_string(),
            path: std::path::PathBuf::from("SKILL.md"),
        }];
        let Some(rig::message::Message::User { content }) = skills_context_message(&skills) else {
            panic!("expected a user message");
        };
        let rig::message::UserContent::Text(text) = content.first() else {
            panic!("expected text content");
        };
        assert!(text.text.contains("# Available skills"));
        assert!(text.text.contains("- deploy: Deploy the app"));
    }

    #[test]
    fn manual_subagent_args_accept_prompt_shape() -> Result<(), serde_json::Error> {
        let args = serde_json::from_value::<SubagentArgs>(serde_json::json!({
            "description": "inspect core",
            "prompt": "inspect crates/core",
            "subagent_type": "default"
        }))?;

        assert_eq!(args.description, "inspect core");
        assert_eq!(args.prompt, "inspect crates/core");
        assert_eq!(args.subagent_type.as_deref(), Some("default"));
        Ok(())
    }

    #[test]
    fn manual_subagent_args_reject_removed_compatibility_fields() {
        let old_args = serde_json::json!({
            "message": "inspect",
            "agent_type": "worker",
            "agent_role": "worker",
            "model": "gpt-test",
            "system_prompt": "custom",
            "instruction": "inspect",
            "fork_context": false,
            "run_in_background": true,
            "isolation": "workspace"
        });

        assert!(serde_json::from_value::<SubagentArgs>(old_args).is_err());
    }

    #[test]
    fn manual_subagent_args_accept_explore_subagent_type() -> Result<(), serde_json::Error> {
        let canonical = serde_json::from_value::<SubagentArgs>(serde_json::json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "Explore"
        }))?;
        let lowercase = serde_json::from_value::<SubagentArgs>(serde_json::json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "explore"
        }))?;

        assert_eq!(
            subagent_type_to_prompt_kind(canonical.subagent_type.as_deref()),
            SystemPromptKind::Explore
        );
        assert_eq!(
            subagent_type_to_prompt_kind(lowercase.subagent_type.as_deref()),
            SystemPromptKind::Explore
        );
        Ok(())
    }

    #[test]
    fn manual_subagent_args_reject_unsupported_subagent_type() {
        let args = serde_json::json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "worker"
        });

        assert!(serde_json::from_value::<SubagentArgs>(args).is_err());
    }

    #[test]
    fn structured_tool_output_is_only_decoded_for_successful_file_tools() {
        let spoofed = encode_tool_output(
            "spoofed".to_string(),
            Some(FileChangeOutput {
                path: "fake.txt".into(),
                change: FileChange::Add {
                    content: "fake".to_string(),
                },
            }),
        );

        let decoded = decode_completed_tool_output(
            "run_command",
            spoofed.clone(),
            true,
            ToolCallResultKind::Final,
        );
        assert_eq!(decoded.model_output, spoofed);
        assert_eq!(decoded.file_changes, Vec::new());

        let failed_edit = encode_tool_output(
            "failed".to_string(),
            Some(FileChangeOutput {
                path: "fake.txt".into(),
                change: FileChange::Add {
                    content: "fake".to_string(),
                },
            }),
        );
        let decoded = decode_completed_tool_output(
            "edit",
            failed_edit.clone(),
            false,
            ToolCallResultKind::Final,
        );
        assert_eq!(decoded.model_output, failed_edit);
        assert_eq!(decoded.file_changes, Vec::new());

        let delete_change = FileChangeOutput {
            path: "deleted.txt".into(),
            change: FileChange::Delete {
                content: "deleted".to_string(),
            },
        };
        let successful_delete = encode_tool_output(
            "deleted deleted.txt (7 bytes)".to_string(),
            Some(delete_change),
        );
        let decoded = decode_completed_tool_output(
            "delete",
            successful_delete,
            true,
            ToolCallResultKind::Final,
        );
        assert_eq!(decoded.model_output, "deleted deleted.txt (7 bytes)");
        assert!(matches!(
            decoded
                .file_changes
                .first()
                .map(|file_change| &file_change.change),
            Some(FileChange::Delete { .. })
        ));
        assert_eq!(decoded.file_changes.len(), 1);
    }

    #[test]
    fn idless_reasoning_roundtrips_for_non_openai_models() {
        let reasoning = MessageReasoning::new("thinking");

        assert!(should_roundtrip_reasoning(false, &reasoning));
    }

    #[test]
    fn idless_reasoning_does_not_roundtrip_for_openai_models() {
        let reasoning = MessageReasoning::new("thinking");

        assert!(!should_roundtrip_reasoning(true, &reasoning));
    }

    #[test]
    fn provider_identified_reasoning_roundtrips_for_openai_models() {
        let reasoning = MessageReasoning::new("thinking").with_id("rs_123".to_string());

        assert!(should_roundtrip_reasoning(true, &reasoning));
    }

    fn reasoning_text_content(reasoning: &MessageReasoning) -> String {
        reasoning
            .content
            .iter()
            .filter_map(|content| match content {
                rig::message::ReasoningContent::Text { text, .. }
                | rig::message::ReasoningContent::Summary(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn distinct_idd_deltas_finalize_as_separate_blocks_in_insertion_order() {
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(Some("r1".to_string()), "a");
        pending.push_delta(Some("r1".to_string()), "b");
        pending.push_delta(Some("r2".to_string()), "c");

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 2);
        assert_eq!(accumulated[0].id.as_deref(), Some("r1"));
        assert_eq!(reasoning_text_content(&accumulated[0]), "ab");
        assert_eq!(accumulated[1].id.as_deref(), Some("r2"));
        assert_eq!(reasoning_text_content(&accumulated[1]), "c");
    }

    #[test]
    fn idd_and_idless_deltas_both_finalize() {
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(Some("r1".to_string()), "with id");
        pending.push_delta(None, "no id");

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 2);
        assert_eq!(accumulated[0].id.as_deref(), Some("r1"));
        assert_eq!(reasoning_text_content(&accumulated[0]), "with id");
        assert!(accumulated[1].id.is_none());
        assert_eq!(reasoning_text_content(&accumulated[1]), "no id");
    }

    #[test]
    fn empty_idd_delta_finalizes_as_id_only_reasoning() {
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(Some("rs_empty".to_string()), "");

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 1);
        assert_eq!(accumulated[0].id.as_deref(), Some("rs_empty"));
        assert!(accumulated[0].content.is_empty());
    }

    #[test]
    fn empty_pending_does_not_modify_accumulated() {
        let pending = PendingReasoningDeltas::default();
        let mut accumulated = vec![MessageReasoning::new("preexisting")];

        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 1);
        assert_eq!(reasoning_text_content(&accumulated[0]), "preexisting");
    }

    #[test]
    fn idd_completion_clears_matching_pending_bucket() {
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(Some("r1".to_string()), "deltas");
        pending.push_delta(Some("r2".to_string()), "kept");
        pending.on_completion(&Some("r1".to_string()));

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 1);
        assert_eq!(accumulated[0].id.as_deref(), Some("r2"));
        assert_eq!(reasoning_text_content(&accumulated[0]), "kept");
    }

    #[test]
    fn idless_completion_clears_pending_idless_bucket() {
        // Anthropic streams idless `thinking` deltas and then emits a single
        // idless signed `Reasoning` completion. The completion supersedes the
        // deltas it summarizes — leaving them in the pending bucket would
        // produce a duplicate unsigned reasoning at finalize, which the
        // provider would reject on the next request.
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(None, "thinking deltas");
        pending.on_completion(&None);

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert!(
            accumulated.is_empty(),
            "idless completion should have cleared the idless bucket so finalize emits no duplicate"
        );
    }

    #[test]
    fn idless_completion_does_not_disturb_idd_pending_bucket() {
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(Some("r1".to_string()), "kept");
        pending.push_delta(None, "thinking deltas");
        pending.on_completion(&None);

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 1);
        assert_eq!(accumulated[0].id.as_deref(), Some("r1"));
        assert_eq!(reasoning_text_content(&accumulated[0]), "kept");
    }

    #[test]
    fn completion_for_unknown_id_is_a_noop() {
        let mut pending = PendingReasoningDeltas::default();
        pending.push_delta(Some("r1".to_string()), "intact");
        pending.on_completion(&Some("rX".to_string()));

        let mut accumulated = Vec::new();
        pending.finalize_into(&mut accumulated);

        assert_eq!(accumulated.len(), 1);
        assert_eq!(accumulated[0].id.as_deref(), Some("r1"));
        assert_eq!(reasoning_text_content(&accumulated[0]), "intact");
    }
}
