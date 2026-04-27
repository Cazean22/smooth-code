use std::sync::Arc;

use futures_util::StreamExt;
use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use smooth_protocol::{
    AgentMessageCompletedEvent, AgentMessageDeltaEvent, EventMsg, ToolCallCompletedEvent,
    ToolCallStartedEvent,
};
use tokio_util::sync::CancellationToken;

use crate::{
    core::{Session, TurnContext},
    provider::SessionStreamEvent,
    state::TaskKind,
};

use super::SessionTask;

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

            match item.ok()? {
                SessionStreamEvent::TextDelta(delta) => {
                    last_assistant_message.push_str(&delta);
                    session
                        .emit_event(
                            &ctx,
                            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                                thread_id: session.id.to_string(),
                                turn_id: ctx.sub_id.clone(),
                                item_id: ctx.assistant_item_id.clone(),
                                delta,
                            }),
                        )
                        .await;
                }
                SessionStreamEvent::ToolCall {
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
                SessionStreamEvent::ToolResult {
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
                SessionStreamEvent::Final { response, history } => {
                    session.replace_history(history).await;
                    if !response.is_empty() {
                        last_assistant_message = response;
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
}
