use std::{sync::Arc, time::Duration};

use futures_util::{FutureExt, StreamExt, future::select_all, stream::FuturesUnordered};
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
    ToolCallStartedEvent,
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
    thread_id: String,
    agent_path: String,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    status: Option<String>,
    #[serde(default)]
    status_detail: Option<AgentStatus>,
    #[serde(default)]
    last_assistant_message: Option<String>,
}

struct ExecutedToolCall {
    index: usize,
    tool_result_message: Message,
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
    child_thread_id: ThreadId,
    waiter: InlineChildCompletionReceiver,
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
    let mut request_history = history_before_turn;
    let mut pending_prompt = initial_prompt;
    let mut saw_tool_loop = false;
    let mut retained_subagents = Vec::new();

    loop {
        if cancellation_token.is_cancelled() {
            return None;
        }
        let ready_completion_texts = drain_ready_retained_subagents(&mut retained_subagents);
        if !ready_completion_texts.is_empty() {
            append_user_texts_to_context(
                &mut request_history,
                &mut pending_prompt,
                ready_completion_texts,
            );
            continue;
        }

        let mut stream = match session
            .model()
            .stream_completion_turn(pending_prompt.clone(), &request_history)
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
        let mut pending_reasoning_delta_text = String::new();
        let mut pending_reasoning_delta_id = None;
        let mut saw_tool_call_this_turn = false;
        let mut late_spawn_completion_texts = Vec::new();
        let mut turn_summary = SessionTurnSummary {
            assistant_message_id: None,
            response: String::new(),
        };

        loop {
            if cancellation_token.is_cancelled() {
                return None;
            }

            let item = if retained_subagents.is_empty() {
                tokio::select! {
                    _ = cancellation_token.cancelled() => return None,
                    item = stream.next() => item,
                }
            } else {
                tokio::select! {
                    _ = cancellation_token.cancelled() => return None,
                    completion = next_retained_subagent_completion(&mut retained_subagents) => {
                        if let Some(completion_text) = completion {
                            late_spawn_completion_texts.push(completion_text);
                        }
                        continue;
                    }
                    item = stream.next() => item,
                }
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
                        pending_reasoning_delta_text.push_str(&reasoning);
                        if pending_reasoning_delta_id.is_none() {
                            pending_reasoning_delta_id = id;
                        }
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
                        merge_reasoning_blocks(&mut accumulated_reasoning, &reasoning);
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
                    SessionAssistantContent::ToolCallDelta { .. }
                    | SessionAssistantContent::Final => {}
                },
                SessionCompletionEvent::Completed(summary) => {
                    turn_summary = summary;
                }
            }
        }

        if accumulated_reasoning.is_empty() && !pending_reasoning_delta_text.is_empty() {
            let mut reasoning = MessageReasoning::new(&pending_reasoning_delta_text);
            if let Some(id) = pending_reasoning_delta_id.take() {
                reasoning = reasoning.with_id(id);
            }
            accumulated_reasoning.push(reasoning);
        }

        if saw_tool_call_this_turn {
            let mut continuation_messages = vec![pending_prompt];
            let mut content_items = Vec::new();
            if !turn_summary.response.is_empty() {
                content_items.push(AssistantContent::text(&turn_summary.response));
            }
            let requires_provider_reasoning_ids = session.model().requires_provider_reasoning_ids();
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

            if !content_items.is_empty() {
                continuation_messages.push(Message::Assistant {
                    id: turn_summary.assistant_message_id.clone(),
                    content: OneOrMany::many(content_items)
                        .expect("tool phase assistant content should not be empty"),
                });
            }

            let executed_tool_calls = execute_tool_calls_concurrently(
                Arc::clone(&session),
                Arc::clone(&ctx),
                pending_tool_calls,
                cancellation_token.clone(),
                &mut retained_subagents,
            )
            .await?;
            for executed in executed_tool_calls {
                continuation_messages.push(executed.tool_result_message);
            }

            for completion_text in late_spawn_completion_texts {
                continuation_messages.push(Message::user(completion_text));
            }

            pending_prompt = continuation_messages
                .pop()
                .expect("tool loop continuation should produce a follow-up prompt");
            request_history.extend(continuation_messages);
            continue;
        }

        if !late_spawn_completion_texts.is_empty() {
            append_completion_texts_after_assistant_turn(
                &mut request_history,
                &mut pending_prompt,
                &turn_summary,
                &mut accumulated_reasoning,
                session.model().requires_provider_reasoning_ids(),
                late_spawn_completion_texts,
            );
            continue;
        }

        if !retained_subagents.is_empty() {
            let completion_text = tokio::select! {
                _ = cancellation_token.cancelled() => return None,
                completion = next_retained_subagent_completion(&mut retained_subagents) => completion,
            };
            if let Some(completion_text) = completion_text {
                append_completion_texts_after_assistant_turn(
                    &mut request_history,
                    &mut pending_prompt,
                    &turn_summary,
                    &mut accumulated_reasoning,
                    session.model().requires_provider_reasoning_ids(),
                    vec![completion_text],
                );
                continue;
            }
            continue;
        }

        let last_assistant_message = turn_summary.response.clone();
        let final_history = build_full_history(request_history, pending_prompt, &turn_summary);
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
            }),
        )
        .await;

    ExecutedToolCall {
        index,
        tool_result_message: tool_result_to_user_message(
            tool_call_id,
            tool_call_call_id,
            tool_output,
        ),
    }
}

async fn execute_tool_calls_concurrently(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending_tool_calls: Vec<PendingToolCall>,
    cancellation_token: CancellationToken,
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
) -> Option<Vec<ExecutedToolCall>> {
    let mut normal_tool_calls = Vec::new();
    let mut spawn_tool_calls = Vec::new();
    let mut resolved = Vec::new();

    for pending in pending_tool_calls {
        if pending.tool_call.function.name == "spawn_agent" {
            match start_spawn_tool_call(Arc::clone(&session), Arc::clone(&ctx), pending).await {
                SpawnToolStart::Started(started) => spawn_tool_calls.push(*started),
                SpawnToolStart::Completed(executed) => resolved.push(*executed),
            }
        } else {
            normal_tool_calls.push(pending);
        }
    }

    let has_normal_tools = !normal_tool_calls.is_empty();
    let mut pending_futures = normal_tool_calls
        .into_iter()
        .map(|pending| execute_normal_tool_call(Arc::clone(&session), Arc::clone(&ctx), pending))
        .collect::<FuturesUnordered<_>>();
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

    if !spawn_tool_calls.is_empty() {
        let wait_mode = if has_normal_tools {
            SpawnWaitMode::GracePeriod(Duration::from_secs(1))
        } else {
            SpawnWaitMode::UntilAllComplete
        };
        wait_for_spawn_tool_calls(&mut spawn_tool_calls, wait_mode, &cancellation_token).await?;

        for mut spawn_call in spawn_tool_calls {
            if spawn_call.completion.is_none()
                && let Some(waiter) = spawn_call.waiter.take()
            {
                retained_subagents.push(RetainedSpawnCompletion {
                    metadata: spawn_call.metadata.clone(),
                    child_thread_id: spawn_call.child_thread_id,
                    waiter,
                });
            }
            let executed =
                complete_spawn_tool_call(Arc::clone(&session), Arc::clone(&ctx), spawn_call).await;
            resolved.push(executed);
        }
    }

    resolved.sort_by_key(|executed| executed.index);
    Some(resolved)
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

async fn complete_spawn_tool_call(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    spawn_call: StartedSpawnToolCall,
) -> ExecutedToolCall {
    let (tool_output, success, error) = spawn_tool_call_output(&session, &spawn_call);
    complete_tool_call(
        session,
        ctx,
        spawn_call.index,
        spawn_call.tool_call_id,
        spawn_call.tool_call_call_id,
        spawn_call.internal_call_id,
        tool_output,
        success,
        error,
    )
    .await
}

fn spawn_tool_call_output(
    session: &Session,
    spawn_call: &StartedSpawnToolCall,
) -> (String, bool, Option<String>) {
    match spawn_call.completion.as_ref() {
        Some(Ok(completion)) => encode_spawn_agent_result(
            &spawn_call.metadata,
            &completion.status,
            completion.last_assistant_message.clone(),
        )
        .map(|output| (output, true, None))
        .unwrap_or_else(|message| (message.clone(), false, Some(message))),
        Some(Err(message)) => (message.clone(), false, Some(message.clone())),
        None => {
            let status = match session.agent_control.get_status(spawn_call.child_thread_id) {
                AgentStatus::NotFound => spawn_call.initial_status.clone(),
                status => status,
            };
            encode_spawn_agent_result(&spawn_call.metadata, &status, None)
                .map(|output| (output, true, None))
                .unwrap_or_else(|message| (message.clone(), false, Some(message)))
        }
    }
}

fn drain_ready_retained_subagents(
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
) -> Vec<String> {
    let mut ready = Vec::new();
    let mut index = 0;
    while index < retained_subagents.len() {
        let completion = match retained_subagents[index].waiter.try_recv() {
            Ok(completion) => Some(Ok(completion)),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => Some(Err(format!(
                "spawn_agent inline wait failed to resolve for child {}",
                retained_subagents[index].child_thread_id
            ))),
        };
        if let Some(completion) = completion {
            let retained = retained_subagents.remove(index);
            ready.push(retained_completion_text(retained, completion));
        } else {
            index += 1;
        }
    }
    ready
}

async fn next_retained_subagent_completion(
    retained_subagents: &mut Vec<RetainedSpawnCompletion>,
) -> Option<String> {
    let futures = retained_subagents
        .iter_mut()
        .enumerate()
        .map(|(index, retained)| {
            (&mut retained.waiter).map(move |result| {
                (
                    index,
                    result
                        .map_err(|err| format!("spawn_agent inline wait failed to resolve: {err}")),
                )
            })
        })
        .collect::<Vec<_>>();
    if futures.is_empty() {
        return None;
    }

    let ((index, completion), _selected_index, remaining) = select_all(futures).await;
    drop(remaining);
    let retained = retained_subagents.remove(index);
    Some(retained_completion_text(retained, completion))
}

fn retained_completion_text(
    retained: RetainedSpawnCompletion,
    completion: Result<InlineChildCompletion, String>,
) -> String {
    let (status, last_assistant_message) = match completion {
        Ok(completion) => (completion.status, completion.last_assistant_message),
        Err(message) => (AgentStatus::Errored(message), None),
    };
    encode_spawn_agent_result(&retained.metadata, &status, last_assistant_message)
        .unwrap_or_else(|message| message)
}

fn append_user_texts_to_context(
    request_history: &mut Vec<Message>,
    pending_prompt: &mut Message,
    user_texts: Vec<String>,
) {
    let mut continuation_messages = vec![pending_prompt.clone()];
    continuation_messages.extend(user_texts.into_iter().map(Message::user));
    *pending_prompt = continuation_messages
        .pop()
        .expect("user-text continuation should produce a follow-up prompt");
    request_history.extend(continuation_messages);
}

fn append_completion_texts_after_assistant_turn(
    request_history: &mut Vec<Message>,
    pending_prompt: &mut Message,
    turn_summary: &SessionTurnSummary,
    accumulated_reasoning: &mut Vec<MessageReasoning>,
    requires_provider_reasoning_ids: bool,
    completion_texts: Vec<String>,
) {
    let mut continuation_messages = vec![pending_prompt.clone()];
    let mut content_items = Vec::new();
    if !turn_summary.response.is_empty() {
        content_items.push(AssistantContent::text(&turn_summary.response));
    }
    for reasoning in accumulated_reasoning.drain(..) {
        if should_roundtrip_reasoning(requires_provider_reasoning_ids, &reasoning) {
            content_items.push(AssistantContent::Reasoning(reasoning));
        }
    }
    if !content_items.is_empty() {
        continuation_messages.push(Message::Assistant {
            id: turn_summary.assistant_message_id.clone(),
            content: OneOrMany::many(content_items)
                .expect("assistant continuation content should not be empty"),
        });
    }
    continuation_messages.extend(completion_texts.into_iter().map(Message::user));
    *pending_prompt = continuation_messages
        .pop()
        .expect("completion continuation should produce a follow-up prompt");
    request_history.extend(continuation_messages);
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
    SpawnAgentResult {
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
    }
}

fn build_full_history(
    mut history_before_turn: Vec<Message>,
    pending_prompt: Message,
    summary: &SessionTurnSummary,
) -> Vec<Message> {
    history_before_turn.push(pending_prompt);
    if !summary.response.is_empty() {
        history_before_turn.push(Message::assistant(&summary.response));
    }
    history_before_turn
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

fn tool_result_to_user_message(
    id: String,
    call_id: Option<String>,
    tool_result: String,
) -> Message {
    Message::User {
        content: OneOrMany::one(UserContent::ToolResult(ToolResult {
            id,
            call_id,
            content: ToolResultContent::from_tool_output(tool_result),
        })),
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

    use super::should_roundtrip_reasoning;

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
}
