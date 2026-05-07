use std::sync::Arc;

use futures_util::{StreamExt, stream::FuturesUnordered};
use rig::{
    OneOrMany,
    message::{
        AssistantContent, Message, Reasoning as MessageReasoning, ReasoningContent, Text,
        ToolResult, ToolResultContent, UserContent,
    },
};
use serde::Deserialize;
use smooth_protocol::{
    AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
    AgentReasoningDeltaEvent, AgentStatus, ErrorEvent, EventMsg, ThreadId, ToolCallCompletedEvent,
    ToolCallStartedEvent,
};
use tokio_util::sync::CancellationToken;
use tools::SpawnAgentParams;

use crate::{
    agent::InProcessMultiAgentClient,
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

struct ExecutedToolCall {
    index: usize,
    tool_result_message: Message,
    inline_notices: Vec<smooth_protocol::InterAgentCommunication>,
}

struct PendingToolCall {
    index: usize,
    assistant_tool_call: AssistantContent,
    tool_call: rig::message::ToolCall,
    internal_call_id: String,
}

struct InlineWaitGuard {
    control: crate::agent::AgentControl,
    child_thread_id: ThreadId,
    armed: bool,
}

impl InlineWaitGuard {
    fn new(control: crate::agent::AgentControl, child_thread_id: ThreadId) -> Self {
        Self {
            control,
            child_thread_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InlineWaitGuard {
    fn drop(&mut self) {
        if self.armed {
            self.control
                .unregister_inline_child_completion_waiter(self.child_thread_id);
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
        let mailbox_messages = session.drain_mailbox().await;
        let mut prompt_parts = mailbox_messages
            .iter()
            .map(render_mailbox_message)
            .collect::<Vec<_>>();
        prompt_parts.extend(input.into_iter().filter(|item| !item.is_empty()));
        let prompt_text = prompt_parts.join("\n");
        session.record_user_message(prompt_text.clone()).await;
        session
            .emit_event(&ctx, EventMsg::UserMessage(prompt_text.clone()))
            .await;
        for communication in mailbox_messages {
            session
                .emit_event(
                    &ctx,
                    EventMsg::InterAgentMessage(smooth_protocol::InterAgentCommunicationEvent {
                        communication,
                    }),
                )
                .await;
        }

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

fn render_mailbox_message(communication: &smooth_protocol::InterAgentCommunication) -> String {
    format!(
        "<inter_agent_message from=\"{}\">{}</inter_agent_message>",
        communication.author, communication.content
    )
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

    loop {
        if cancellation_token.is_cancelled() {
            return None;
        }

        let current_prompt = new_messages
            .last()
            .cloned()
            .expect("manual turn loop should keep a pending prompt");
        let history_snapshot = build_history_for_request(
            &history_before_turn,
            &new_messages[..new_messages.len().saturating_sub(1)],
        );
        let mut stream = match session
            .model()
            .stream_completion_turn(current_prompt, &history_snapshot)
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                fail_turn(&session, &ctx, "manual.stream_completion_turn.open", err).await;
                return None;
            }
        };
        let mut pending_tool_calls = Vec::new();
        let mut tool_result_messages = Vec::new();
        let mut inline_notices = Vec::new();
        let mut accumulated_reasoning = Vec::new();
        let mut pending_reasoning_delta_text = String::new();
        let mut pending_reasoning_delta_id = None;
        let mut saw_tool_call_this_turn = false;
        let mut turn_summary = SessionTurnSummary {
            assistant_message_id: None,
            response: String::new(),
        };

        while let Some(item) = stream.next().await {
            if cancellation_token.is_cancelled() {
                return None;
            }

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
            let mut content_items = Vec::new();
            if !turn_summary.response.is_empty() {
                content_items.push(AssistantContent::text(&turn_summary.response));
            }
            let requires_provider_reasoning_ids =
                session.model().requires_provider_reasoning_ids();
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
                new_messages.push(Message::Assistant {
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
                )
                .await?;
            for executed in executed_tool_calls {
                tool_result_messages.push(executed.tool_result_message);
                inline_notices.extend(executed.inline_notices);
            }
            new_messages.extend(tool_result_messages);

            for communication in inline_notices {
                session
                    .emit_event(
                        &ctx,
                        EventMsg::InterAgentMessage(
                            smooth_protocol::InterAgentCommunicationEvent {
                                communication: communication.clone(),
                            },
                        ),
                    )
                    .await;
                new_messages.push(Message::user(render_mailbox_message(&communication)));
            }
            continue;
        }

        let last_assistant_message = turn_summary.response.clone();
        let final_history =
            build_full_history(&history_before_turn, new_messages.clone(), &turn_summary);
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

async fn execute_tool_call(
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

    let (tool_output, success, error, inline_notices) = if tool_call.function.name == "spawn_agent" {
        resolve_spawn_tool_call(Arc::clone(&session), tool_call.function.arguments.clone()).await
    } else {
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
        (tool_output, success, error, Vec::new())
    };

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
            tool_call.id,
            tool_call.call_id,
            tool_output,
        ),
        inline_notices,
    }
}

async fn execute_tool_calls_concurrently(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    pending_tool_calls: Vec<PendingToolCall>,
    cancellation_token: CancellationToken,
) -> Option<Vec<ExecutedToolCall>> {
    let mut pending_futures = pending_tool_calls
        .into_iter()
        .map(|pending| execute_tool_call(Arc::clone(&session), Arc::clone(&ctx), pending))
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
    resolved.sort_by_key(|executed| executed.index);
    Some(resolved)
}

async fn resolve_spawn_tool_call(
    session: Arc<Session>,
    arguments: serde_json::Value,
) -> (
    String,
    bool,
    Option<String>,
    Vec<smooth_protocol::InterAgentCommunication>,
) {
    let args = match serde_json::from_value::<ManualSpawnAgentArgs>(arguments) {
        Ok(args) => args,
        Err(err) => {
            let message = format!("invalid spawn_agent args: {err}");
            return (message.clone(), false, Some(message), Vec::new());
        }
    };
    let client = InProcessMultiAgentClient::new(session.id, session.agent_control.clone());
    match client
        .spawn_with_inline_wait(SpawnAgentParams {
            message: args.message,
            agent_type: args.agent_type,
            model: args.model,
            fork_context: args.fork_context,
        })
        .await
    {
        Ok((mut agent_info, waiter)) => {
            let child_thread_id = match agent_info.thread_id.parse::<ThreadId>() {
                Ok(thread_id) => thread_id,
                Err(err) => {
                    let message = format!("invalid spawned thread id: {err}");
                    return (message.clone(), false, Some(message), Vec::new());
                }
            };
            let mut guard = InlineWaitGuard::new(session.agent_control.clone(), child_thread_id);
            match waiter.await {
                Ok(completion) => {
                    guard.disarm();
                    agent_info.status = Some(agent_status_label(&completion.status).to_string());
                    agent_info.status_detail = Some(completion.status.clone());
                    agent_info.last_assistant_message = completion.last_assistant_message.clone();
                    match serde_json::to_string(&agent_info) {
                        Ok(output) => (output, true, None, vec![completion.communication]),
                        Err(err) => {
                            let message = format!("failed to encode spawn_agent output: {err}");
                            (message.clone(), false, Some(message), Vec::new())
                        }
                    }
                }
                Err(err) => {
                    guard.disarm();
                    let message = format!("spawn_agent inline wait failed to resolve: {err}");
                    (message.clone(), false, Some(message), Vec::new())
                }
            }
        }
        Err(err) => {
            let message = err.to_string();
            (message.clone(), false, Some(message), Vec::new())
        }
    }
}

fn build_history_for_request(history: &[Message], new_messages: &[Message]) -> Vec<Message> {
    history.iter().chain(new_messages.iter()).cloned().collect()
}

fn build_full_history(
    history_before_turn: &[Message],
    mut new_messages: Vec<Message>,
    summary: &SessionTurnSummary,
) -> Vec<Message> {
    if !summary.response.is_empty() {
        new_messages.push(Message::assistant(&summary.response));
    }
    history_before_turn
        .iter()
        .cloned()
        .chain(new_messages)
        .collect()
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
