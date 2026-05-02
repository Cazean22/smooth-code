use std::sync::Arc;

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    message::{Message, Reasoning as MessageReasoning, ReasoningContent, Text, UserContent},
};
use smooth_protocol::{
    AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
    AgentReasoningDeltaEvent, EventMsg, ToolCallCompletedEvent, ToolCallStartedEvent,
};
use tokio_util::sync::CancellationToken;

use crate::{
    core::{Session, TurnContext},
    provider::{SessionAssistantContent, SessionStreamEvent},
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

#[derive(Default)]
pub(crate) struct RegularTask;

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
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

        let prompt_text = input.join("\n");
        session.record_user_message(prompt_text.clone()).await;
        session
            .emit_event(&ctx, EventMsg::UserMessage(prompt_text.clone()))
            .await;

        let prompt = Message::User {
            content: OneOrMany::one(UserContent::Text(Text {
                text: prompt_text.clone(),
            })),
        };
        let history = session.history().await;
        let mut stream = session.model().stream_turn(prompt, &history).await.ok()?;
        let mut last_assistant_message = String::new();
        let mut saw_tool_loop = false;

        while let Some(item) = stream.next().await {
            if cancellation_token.is_cancelled() {
                return None;
            }
            // tracing::debug!(?item, "received stream item");

            match item.ok()? {
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
                    SessionAssistantContent::ToolCallDelta { .. }
                    | SessionAssistantContent::Final => {}
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
                                rig::message::ToolResultContent::Text(text) => {
                                    Some(text.text.as_str())
                                }
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
        tracing::debug!(
            thread_id = %session.id,
            turn_id = %ctx.sub_id,
            input_count = input.len(),
            "finished regular task"
        );

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

    async fn abort(&self, session: Arc<Session>, ctx: Arc<TurnContext>) {
        let _ = ctx;
        session.abort_pending_dynamic_tool_requests().await;
    }
}
