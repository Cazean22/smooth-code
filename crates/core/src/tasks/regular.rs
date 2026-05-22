use std::{sync::Arc, time::Duration};

use futures_util::{FutureExt, StreamExt, future::select_all, stream::FuturesUnordered};
use indexmap::IndexMap;
use rig::{
    OneOrMany,
    message::{
        AssistantContent, Message, Reasoning as MessageReasoning, ReasoningContent, Text,
        ToolResult, ToolResultContent, UserContent,
    },
};
use serde::{Deserialize, Serialize};
use smooth_protocol::{
    AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
    AgentReasoningDeltaEvent, AgentStatus, ErrorEvent, EventMsg, ThreadId, ToolCallCompletedEvent,
    ToolCallResultKind, ToolCallStartedEvent,
};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{
        InlineChildCompletionReceiver, control::InlineChildCompletion, registry::AgentMetadata,
        status::last_assistant_message,
    },
    core::{Session, TurnContext},
    provider::{
        SessionAssistantContent, SessionCompletionEvent, SessionStreamEvent, SessionTurnSummary,
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
    let message = err.to_string();
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
                message: message.clone(),
                codex_error_info: None,
            }),
        )
        .await;
    session
        .set_agent_status(AgentStatus::Errored(message), Some(ctx))
        .await;
}

#[derive(Default)]
pub(crate) struct RegularTask;

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct ManualSpawnAgentArgs {
    message: String,
    agent_type: Option<String>,
    model: Option<String>,
    #[serde(default)]
    fork_context: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SpawnAgentResult {
    event: String,
    thread_id: String,
    agent_path: String,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    status: Option<String>,
    #[serde(default)]
    status_detail: Option<AgentStatus>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    next_action: String,
    instructions: String,
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

struct StartedSpawnToolCall {
    index: usize,
    tool_call_id: String,
    tool_call_call_id: Option<String>,
    internal_call_id: String,
    metadata: AgentMetadata,
    child_thread_id: ThreadId,
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
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        tracing::debug!(
            thread_id = %session.id,
            turn_id = %ctx.sub_id,
            input_count = input.len(),
            "running regular task"
        );
        let _ = self;
        if cancellation_token.is_cancelled() {
            return None;
        }

        let input_count = input.len();
        let history_before_turn = session.history().await;
        let prompt_parts = input
            .into_iter()
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        let prompt_text = prompt_parts.join("\n");
        session.record_user_message(prompt_text.clone()).await;
        session
            .emit_event(&ctx, EventMsg::UserMessage(prompt_text.clone()))
            .await;

        let prompt = Message::User {
            content: OneOrMany::one(UserContent::Text(Text {
                text: prompt_text.clone(),
            })),
        };
        let result = if session.model().supports_manual_tool_loop() {
            run_manual_turn(
                Arc::clone(&session),
                Arc::clone(&ctx),
                prompt,
                history_before_turn,
                cancellation_token.clone(),
            )
            .await
        } else {
            run_opaque_turn(
                Arc::clone(&session),
                Arc::clone(&ctx),
                prompt,
                history_before_turn,
                cancellation_token.clone(),
            )
            .await
        };
        tracing::debug!(
            thread_id = %session.id,
            turn_id = %ctx.sub_id,
            input_count,
            "finished regular task"
        );
        result
    }

    async fn abort(&self, session: Arc<Session>, ctx: Arc<TurnContext>) {
        let _ = ctx;
        session.abort_pending_dynamic_tool_requests().await;
    }
}

async fn run_manual_turn(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    initial_prompt: Message,
    history_before_turn: Vec<Message>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    let mut new_messages = vec![initial_prompt];
    let mut saw_tool_loop = false;
    let mut retained_subagents = Vec::new();

    loop {
        if cancellation_token.is_cancelled() {
            return None;
        }

        let (pending_prompt, request_history) =
            build_request_parts(&history_before_turn, &new_messages)?;
        let mut stream = match session
            .model()
            .stream_completion_turn(pending_prompt, &request_history)
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                fail_turn(&session, &ctx, "manual.stream_completion_turn.open", err).await;
                return None;
            }
        };
        let mut pending_tool_calls = Vec::new();
        let mut accumulated_reasoning = Vec::new();
        let mut pending_reasoning_deltas = PendingReasoningDeltas::default();
        let mut saw_tool_call_this_turn = false;
        let mut stream_phase_completions: Vec<String> = Vec::new();
        let mut turn_summary = SessionTurnSummary {
            assistant_message_id: None,
            response: String::new(),
        };

        loop {
            if cancellation_token.is_cancelled() {
                return None;
            }

            let item = tokio::select! {
                _ = cancellation_token.cancelled() => return None,
                Some(text) = next_retained_subagent_completion(&mut retained_subagents),
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
                    fail_turn(&session, &ctx, "manual.stream_completion_turn.item", err).await;
                    return None;
                }
            };
            match event {
                SessionCompletionEvent::AssistantItem(assistant_item) => match assistant_item {
                    SessionAssistantContent::Text(text) => {
                        session
                            .emit_event(
                                &ctx,
                                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    item_id: ctx.assistant_item_id.clone(),
                                    delta: text.text,
                                }),
                            )
                            .await;
                    }
                    SessionAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id,
                    } => {
                        saw_tool_loop = true;
                        saw_tool_call_this_turn = true;
                        session
                            .emit_event(
                                &ctx,
                                EventMsg::ToolCallStarted(ToolCallStartedEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    call_id: internal_call_id.clone(),
                                    tool_name: tool_call.function.name.clone(),
                                    args_preview: tool_call.function.arguments.to_string(),
                                }),
                            )
                            .await;
                        pending_tool_calls.push(PendingToolCall {
                            index: pending_tool_calls.len(),
                            assistant_tool_call: AssistantContent::ToolCall(tool_call.clone()),
                            tool_call,
                            internal_call_id,
                        });
                    }
                    SessionAssistantContent::ReasoningDelta { id, reasoning } => {
                        let item_id = id
                            .clone()
                            .unwrap_or_else(|| format!("{}-reasoning", ctx.assistant_item_id));
                        pending_reasoning_deltas.push_delta(id, &reasoning);
                        session
                            .emit_event(
                                &ctx,
                                EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                                    thread_id: session.id.to_string(),
                                    turn_id: ctx.sub_id.clone(),
                                    item_id,
                                    delta: reasoning,
                                }),
                            )
                            .await;
                    }
                    SessionAssistantContent::Reasoning(reasoning) => {
                        // Skip only truly empty completions. A block carrying
                        // `Encrypted` or `Redacted` content has empty
                        // human-readable text but still must be roundtripped
                        // to the provider on the next turn — for OpenAI's
                        // o-series, the encrypted chain-of-thought is what
                        // preserves reasoning continuity, and dropping it
                        // can produce refusals or degraded responses.
                        if reasoning.content.is_empty() {
                            continue;
                        }
                        pending_reasoning_deltas.on_completion(&reasoning.id);
                        let text = reasoning_text(&reasoning);
                        let item_id = reasoning
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("{}-reasoning", ctx.assistant_item_id));
                        merge_reasoning_blocks(&mut accumulated_reasoning, &reasoning);
                        if !text.is_empty() {
                            session
                                .emit_event(
                                    &ctx,
                                    EventMsg::AgentReasoningCompleted(
                                        AgentReasoningCompletedEvent {
                                            thread_id: session.id.to_string(),
                                            turn_id: ctx.sub_id.clone(),
                                            item_id,
                                            text,
                                        },
                                    ),
                                )
                                .await;
                        }
                    }
                    SessionAssistantContent::ToolCallDelta { .. }
                    | SessionAssistantContent::Final => {}
                },
                SessionCompletionEvent::Completed(summary) => {
                    turn_summary = summary;
                }
            }
        }

        pending_reasoning_deltas.finalize_into(&mut accumulated_reasoning);

        if saw_tool_call_this_turn {
            let assistant_message = build_assistant_tool_message(
                &turn_summary,
                &mut accumulated_reasoning,
                &pending_tool_calls,
                session.model().requires_provider_reasoning_ids(),
            );

            let (normal_calls, spawn_calls) = partition_pending_tool_calls(pending_tool_calls);

            let started_spawns =
                start_spawn_calls_concurrently(Arc::clone(&session), Arc::clone(&ctx), spawn_calls)
                    .await;

            let normal_results = execute_normal_tools_concurrently(
                Arc::clone(&session),
                Arc::clone(&ctx),
                normal_calls,
                &cancellation_token,
            )
            .await?;

            let has_normal_tools = !normal_results.is_empty();
            let (started_spawns, immediate_spawn_results) =
                split_started_and_immediate(started_spawns);
            let started_spawns =
                wait_for_spawn_batch(started_spawns, has_normal_tools, &cancellation_token).await?;

            let mut spawn_results = collect_spawn_results(
                Arc::clone(&session),
                Arc::clone(&ctx),
                started_spawns,
                &mut retained_subagents,
            )
            .await;
            spawn_results.extend(immediate_spawn_results);

            let executed_tool_calls = merge_in_index_order(normal_results, spawn_results);

            // Pure `spawn_agent` batch: also block on every retained receiver
            // carried over from prior turns so the model sees their completed
            // JSON in this same iteration. The drained completions ride along
            // with anything captured mid-stream and surface together as a
            // single user-text message below.
            if !has_normal_tools {
                let drained = tokio::select! {
                    _ = cancellation_token.cancelled() => return None,
                    completions = drain_retained_subagent_completions(&mut retained_subagents) => completions,
                };
                stream_phase_completions.extend(drained);
            }

            if let Some(message) = assistant_message {
                new_messages.push(message);
            }
            if let Some(message) = tool_results_to_user_message(executed_tool_calls) {
                new_messages.push(message);
            }
            if let Some(message) = text_items_to_user_message(stream_phase_completions) {
                new_messages.push(message);
            }
            continue;
        }

        if !stream_phase_completions.is_empty() {
            append_completion_texts_after_assistant_turn(
                &mut new_messages,
                &turn_summary,
                &mut accumulated_reasoning,
                session.model().requires_provider_reasoning_ids(),
                stream_phase_completions,
            );
            continue;
        }

        if !retained_subagents.is_empty() {
            let completion_texts = tokio::select! {
                _ = cancellation_token.cancelled() => return None,
                completions = drain_retained_subagent_completions(&mut retained_subagents) => completions,
            };
            append_completion_texts_after_assistant_turn(
                &mut new_messages,
                &turn_summary,
                &mut accumulated_reasoning,
                session.model().requires_provider_reasoning_ids(),
                completion_texts,
            );
            continue;
        }

        if let Some(message) = build_assistant_text_reasoning_message(
            &turn_summary,
            &mut accumulated_reasoning,
            session.model().requires_provider_reasoning_ids(),
        ) {
            new_messages.push(message);
        }
        let last_assistant_message = turn_summary.response.clone();
        let mut final_history = history_before_turn;
        final_history.extend(new_messages);
        session.replace_history(final_history).await;
        if !last_assistant_message.is_empty() {
            session
                .persist_assistant_message(last_assistant_message.clone())
                .await;
            session
                .emit_event(
                    &ctx,
                    EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                        thread_id: session.id.to_string(),
                        turn_id: ctx.sub_id.clone(),
                        item_id: ctx.assistant_item_id.clone(),
                        text: last_assistant_message.clone(),
                    }),
                )
                .await;
            session
                .emit_event(&ctx, EventMsg::AgentMessage(last_assistant_message.clone()))
                .await;
            return Some(last_assistant_message);
        }

        if saw_tool_loop {
            return Some(String::new());
        }

        return None;
    }
}

async fn run_opaque_turn(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    prompt: Message,
    history_before_turn: Vec<Message>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    let mut stream = match session
        .model()
        .stream_turn(prompt, &history_before_turn)
        .await
    {
        Ok(stream) => stream,
        Err(err) => {
            fail_turn(&session, &ctx, "opaque.stream_turn.open", err).await;
            return None;
        }
    };
    let mut last_assistant_message = String::new();
    let mut saw_tool_loop = false;

    while let Some(item) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return None;
        }

        let event = match item {
            Ok(event) => event,
            Err(err) => {
                fail_turn(&session, &ctx, "opaque.stream_turn.item", err).await;
                return None;
            }
        };
        match event {
            SessionStreamEvent::StreamAssistantItem(assistant_item) => match assistant_item {
                SessionAssistantContent::Text(text) => {
                    last_assistant_message.push_str(&text.text);
                    session
                        .emit_event(
                            &ctx,
                            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                item_id: ctx.assistant_item_id.clone(),
                                delta: text.text,
                            }),
                        )
                        .await;
                }
                SessionAssistantContent::ToolCall {
                    tool_call,
                    internal_call_id,
                } => {
                    saw_tool_loop = true;
                    session
                        .emit_event(
                            &ctx,
                            EventMsg::ToolCallStarted(ToolCallStartedEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                call_id: internal_call_id,
                                tool_name: tool_call.function.name,
                                args_preview: tool_call.function.arguments.to_string(),
                            }),
                        )
                        .await;
                }
                SessionAssistantContent::ReasoningDelta { id, reasoning } => {
                    let item_id =
                        id.unwrap_or_else(|| format!("{}-reasoning", ctx.assistant_item_id));
                    session
                        .emit_event(
                            &ctx,
                            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                item_id,
                                delta: reasoning,
                            }),
                        )
                        .await;
                }
                SessionAssistantContent::Reasoning(reasoning) => {
                    let text = reasoning_text(&reasoning);
                    if text.is_empty() {
                        continue;
                    }

                    session
                        .emit_event(
                            &ctx,
                            EventMsg::AgentReasoningCompleted(AgentReasoningCompletedEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                item_id: reasoning.id.unwrap_or_else(|| {
                                    format!("{}-reasoning", ctx.assistant_item_id)
                                }),
                                text,
                            }),
                        )
                        .await;
                }
                SessionAssistantContent::ToolCallDelta { .. } | SessionAssistantContent::Final => {}
            },
            SessionStreamEvent::StreamUserItem(user_item) => match user_item {
                rig::streaming::StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                } => {
                    saw_tool_loop = true;
                    let output_preview = tool_result
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            rig::message::ToolResultContent::Text(text) => Some(text.text.as_str()),
                            rig::message::ToolResultContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    session
                        .emit_event(
                            &ctx,
                            EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                call_id: internal_call_id,
                                success: true,
                                output_preview: Some(output_preview),
                                error: None,
                                result_kind: ToolCallResultKind::Final,
                                related_thread_id: None,
                            }),
                        )
                        .await;
                }
            },
            SessionStreamEvent::FinalResponse(final_response) => {
                session
                    .replace_history(final_response.history().unwrap_or(&[]).to_vec())
                    .await;
                if !final_response.response().is_empty() {
                    last_assistant_message = final_response.response().to_string();
                    session
                        .persist_assistant_message(last_assistant_message.clone())
                        .await;
                }
            }
        }
    }

    if last_assistant_message.is_empty() && saw_tool_loop {
        return Some(String::new());
    }

    if last_assistant_message.is_empty() {
        return None;
    }

    session
        .emit_event(
            &ctx,
            EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                thread_id: session.id.to_string(),
                turn_id: ctx.sub_id.clone(),
                item_id: ctx.assistant_item_id.clone(),
                text: last_assistant_message.clone(),
            }),
        )
        .await;
    session
        .emit_event(&ctx, EventMsg::AgentMessage(last_assistant_message.clone()))
        .await;
    Some(last_assistant_message)
}

async fn execute_normal_tool_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending: PendingToolCall,
) -> ExecutedToolCall {
    let PendingToolCall {
        index,
        assistant_tool_call: _,
        tool_call,
        internal_call_id,
    } = pending;

    let (tool_output, success, error) = match session
        .model()
        .call_tool(
            &tool_call.function.name,
            &tool_call.function.arguments.to_string(),
        )
        .await
    {
        Ok(output) => (output, true, None),
        Err(err) => {
            let message = err.to_string();
            (message.clone(), false, Some(message))
        }
    };

    complete_tool_call(
        session,
        ctx,
        index,
        tool_call.id,
        tool_call.call_id,
        internal_call_id,
        tool_output,
        success,
        error,
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
    tool_output: String,
    success: bool,
    error: Option<String>,
    result_kind: ToolCallResultKind,
    related_thread_id: Option<ThreadId>,
) -> ExecutedToolCall {
    session
        .emit_event(
            &ctx,
            EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                thread_id: session.id.to_string(),
                turn_id: ctx.sub_id.clone(),
                call_id: internal_call_id,
                success,
                output_preview: Some(tool_output.clone()),
                error,
                result_kind,
                related_thread_id,
            }),
        )
        .await;

    ExecutedToolCall {
        index,
        tool_result: tool_result(tool_call_id, tool_call_call_id, tool_output),
    }
}

async fn execute_normal_tools_concurrently(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    normal_calls: Vec<PendingToolCall>,
    cancellation_token: &CancellationToken,
) -> Option<Vec<ExecutedToolCall>> {
    let mut pending_futures = normal_calls
        .into_iter()
        .map(|pending| execute_normal_tool_call(Arc::clone(&session), Arc::clone(&ctx), pending))
        .collect::<FuturesUnordered<_>>();
    let mut resolved = Vec::new();
    while !pending_futures.is_empty() {
        let next = tokio::select! {
            _ = cancellation_token.cancelled() => return None,
            next = pending_futures.next() => next,
        };
        let Some(executed) = next else {
            break;
        };
        resolved.push(executed);
    }
    Some(resolved)
}

fn partition_pending_tool_calls(
    pending_tool_calls: Vec<PendingToolCall>,
) -> (Vec<PendingToolCall>, Vec<PendingToolCall>) {
    let mut normal = Vec::new();
    let mut spawn = Vec::new();
    for pending in pending_tool_calls {
        if pending.tool_call.function.name == "spawn_agent" {
            spawn.push(pending);
        } else {
            normal.push(pending);
        }
    }
    (normal, spawn)
}

/// Start every queued `spawn_agent` call concurrently and wait for all starts
/// to finish before returning. We deliberately do NOT race this against the
/// cancellation token: `start_spawn_tool_call` -> `spawn_agent_with_role_for_tool`
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

async fn wait_for_spawn_batch(
    mut spawn_calls: Vec<StartedSpawnToolCall>,
    has_normal_tools: bool,
    cancellation_token: &CancellationToken,
) -> Option<Vec<StartedSpawnToolCall>> {
    if spawn_calls.is_empty() {
        return Some(spawn_calls);
    }
    let wait_mode = if has_normal_tools {
        SpawnWaitMode::GracePeriod(Duration::from_secs(1))
    } else {
        SpawnWaitMode::UntilAllComplete
    };
    wait_for_spawn_tool_calls(&mut spawn_calls, wait_mode, cancellation_token).await?;
    Some(spawn_calls)
}

async fn collect_spawn_results(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    spawn_calls: Vec<StartedSpawnToolCall>,
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
) -> Vec<ExecutedToolCall> {
    let mut resolved = Vec::with_capacity(spawn_calls.len());
    for mut spawn_call in spawn_calls {
        let (tool_output, success, error, result_kind, related_thread_id) =
            if let Some(completion) = spawn_call.completion.as_ref() {
                let (tool_output, success, error) =
                    encode_completed_spawn_result(&spawn_call.metadata, completion);
                (tool_output, success, error, ToolCallResultKind::Final, None)
            } else {
                if let Some(waiter) = spawn_call.waiter.take() {
                    retained_subagents.push(RetainedSpawnCompletion {
                        metadata: spawn_call.metadata.clone(),
                        waiter,
                    });
                }
                let (tool_output, success, error) = encode_live_spawn_status(
                    &session,
                    &spawn_call.metadata,
                    &spawn_call.initial_status,
                    spawn_call.child_thread_id,
                );
                (
                    tool_output,
                    success,
                    error,
                    ToolCallResultKind::StatusUpdate,
                    Some(spawn_call.child_thread_id),
                )
            };
        let executed = complete_tool_call_with_kind(
            Arc::clone(&session),
            Arc::clone(&ctx),
            spawn_call.index,
            spawn_call.tool_call_id,
            spawn_call.tool_call_call_id,
            spawn_call.internal_call_id,
            tool_output,
            success,
            error,
            result_kind,
            related_thread_id,
        )
        .await;
        resolved.push(executed);
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
    let args = match serde_json::from_value::<ManualSpawnAgentArgs>(
        tool_call.function.arguments.clone(),
    ) {
        Ok(args) => args,
        Err(err) => {
            let message = format!("invalid spawn_agent args: {err}");
            return SpawnToolStart::Completed(Box::new(
                complete_tool_call(
                    session,
                    ctx,
                    index,
                    tool_call.id,
                    tool_call.call_id,
                    internal_call_id,
                    message.clone(),
                    false,
                    Some(message),
                )
                .await,
            ));
        }
    };

    match session
        .agent_control
        .spawn_agent_with_role_for_tool(
            session.id,
            args.message,
            args.agent_type,
            args.model,
            args.fork_context,
        )
        .await
    {
        Ok((metadata, initial_status, waiter)) => {
            SpawnToolStart::Started(Box::new(StartedSpawnToolCall {
                index,
                tool_call_id: tool_call.id,
                tool_call_call_id: tool_call.call_id,
                internal_call_id,
                child_thread_id: metadata
                    .agent_id
                    .expect("spawned agent metadata should have a thread id"),
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
                    message.clone(),
                    false,
                    Some(message),
                )
                .await,
            ))
        }
    }
}

enum SpawnWaitMode {
    UntilAllComplete,
    GracePeriod(Duration),
}

async fn wait_for_spawn_tool_calls(
    spawn_tool_calls: &mut [StartedSpawnToolCall],
    wait_mode: SpawnWaitMode,
    cancellation_token: &CancellationToken,
) -> Option<()> {
    match wait_mode {
        SpawnWaitMode::UntilAllComplete => {
            while spawn_tool_calls.iter().any(|call| call.waiter.is_some()) {
                let completion = tokio::select! {
                    _ = cancellation_token.cancelled() => return None,
                    completion = next_started_spawn_completion(spawn_tool_calls) => completion,
                };
                let Some((index, completion)) = completion else {
                    break;
                };
                spawn_tool_calls[index].completion = Some(completion);
            }
        }
        SpawnWaitMode::GracePeriod(duration) => {
            let deadline = tokio::time::sleep(duration);
            tokio::pin!(deadline);
            while spawn_tool_calls.iter().any(|call| call.waiter.is_some()) {
                let completion = tokio::select! {
                    _ = cancellation_token.cancelled() => return None,
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
    Some(())
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

fn encode_live_spawn_status(
    session: &Session,
    metadata: &AgentMetadata,
    initial_status: &AgentStatus,
    child_thread_id: ThreadId,
) -> (String, bool, Option<String>) {
    let status = match session.agent_control.get_status(child_thread_id) {
        AgentStatus::NotFound => initial_status.clone(),
        status => status,
    };
    encode_spawn_agent_result(metadata, &status, None)
        .map(|output| (output, true, None))
        .unwrap_or_else(|message| (message.clone(), false, Some(message)))
}

/// Race every retained subagent receiver in parallel and resolve the first
/// completion. Returns the rendered completion JSON for whichever finished
/// first and removes it from `retained_subagents`; remaining receivers stay
/// in the vec for the next race. Returns `None` only when the vec is empty
/// (callers should already gate the await on `!retained_subagents.is_empty()`
/// because `select_all` panics on an empty iterator).
async fn next_retained_subagent_completion(
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
) -> Option<String> {
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
    Some(retained_completion_text(entry.metadata, completion))
}

async fn drain_retained_subagent_completions(
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
) -> Vec<String> {
    let retained = std::mem::take(retained_subagents);
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
        retained_completion_text(metadata, completion)
    });
    futures_util::future::join_all(completions).await
}

fn retained_completion_text(
    metadata: AgentMetadata,
    completion: Result<InlineChildCompletion, String>,
) -> String {
    let (status, last_assistant_message) = match completion {
        Ok(completion) => (completion.status, completion.last_assistant_message),
        Err(message) => (AgentStatus::Errored(message), None),
    };
    encode_spawn_agent_result(&metadata, &status, last_assistant_message)
        .unwrap_or_else(|message| message)
}

fn append_completion_texts_after_assistant_turn(
    new_messages: &mut Vec<Message>,
    turn_summary: &SessionTurnSummary,
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    requires_provider_reasoning_ids: bool,
    completion_texts: Vec<String>,
) {
    if let Some(message) = build_assistant_text_reasoning_message(
        turn_summary,
        accumulated_reasoning,
        requires_provider_reasoning_ids,
    ) {
        new_messages.push(message);
    }
    if let Some(message) = text_items_to_user_message(completion_texts) {
        new_messages.push(message);
    }
}

fn build_assistant_text_reasoning_message(
    turn_summary: &SessionTurnSummary,
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    requires_provider_reasoning_ids: bool,
) -> Option<Message> {
    let mut content_items = Vec::new();
    if !turn_summary.response.is_empty() {
        content_items.push(AssistantContent::text(&turn_summary.response));
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
        content: OneOrMany::many(content_items).expect("assistant content should not be empty"),
    })
}

fn encode_spawn_agent_result(
    metadata: &AgentMetadata,
    status: &AgentStatus,
    last_assistant_message_override: Option<String>,
) -> Result<String, String> {
    serde_json::to_string(&spawn_agent_result_from_metadata(
        metadata,
        status,
        last_assistant_message_override,
    ))
    .map_err(|err| format!("failed to encode spawn_agent output: {err}"))
}

fn spawn_agent_result_from_metadata(
    metadata: &AgentMetadata,
    status: &AgentStatus,
    last_assistant_message_override: Option<String>,
) -> SpawnAgentResult {
    let is_live = matches!(status, AgentStatus::PendingInit | AgentStatus::Running);
    SpawnAgentResult {
        event: if is_live {
            String::from("agent_status")
        } else {
            String::from("agent_completed")
        },
        thread_id: metadata
            .agent_id
            .map(|thread_id| thread_id.to_string())
            .unwrap_or_default(),
        agent_path: metadata.agent_path.to_string(),
        agent_nickname: metadata.agent_nickname.clone(),
        agent_role: metadata.agent_role.clone(),
        status: Some(agent_status_label(status).to_string()),
        status_detail: Some(status.clone()),
        last_assistant_message: last_assistant_message_override
            .or_else(|| last_assistant_message(status)),
        next_action: if is_live {
            String::from("wait_for_agent_completed")
        } else {
            String::from("use_agent_result")
        },
        instructions: if is_live {
            String::from(
                "This sub-agent is still running. Do not answer or guess from this status. No wait tool is needed; wait for a later user message with event=\"agent_completed\" and the same thread_id.",
            )
        } else {
            String::from(
                "This sub-agent has finished. Use last_assistant_message and status_detail as the sub-agent result.",
            )
        },
    }
}

fn build_request_parts(
    history_before_turn: &[Message],
    new_messages: &[Message],
) -> Option<(Message, Vec<Message>)> {
    let (pending_prompt, new_history) = new_messages.split_last()?;
    let mut request_history = history_before_turn.to_vec();
    request_history.extend_from_slice(new_history);
    Some((pending_prompt.clone(), request_history))
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
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    pending_tool_calls: &[PendingToolCall],
    requires_provider_reasoning_ids: bool,
) -> Option<Message> {
    let mut content_items = Vec::new();
    if !turn_summary.response.is_empty() {
        content_items.push(AssistantContent::text(&turn_summary.response));
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
        content: OneOrMany::many(content_items)
            .expect("tool phase assistant content should not be empty"),
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

fn text_items_to_user_message(texts: Vec<String>) -> Option<Message> {
    let content = texts
        .into_iter()
        .map(|text| UserContent::Text(Text { text }))
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

fn agent_status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::PendingInit => "pending_init",
        AgentStatus::Running => "running",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Completed(_) => "completed",
        AgentStatus::Errored(_) => "errored",
        AgentStatus::Shutdown => "shutdown",
        AgentStatus::NotFound => "not_found",
    }
}

#[cfg(test)]
mod tests {
    use rig::message::Reasoning as MessageReasoning;

    use super::{PendingReasoningDeltas, should_roundtrip_reasoning};

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
