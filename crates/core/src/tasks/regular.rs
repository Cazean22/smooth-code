use std::sync::Arc;

use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use tokio_util::sync::CancellationToken;

use crate::{
    core::{Session, TurnContext},
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
        session
            .set_agent_status(smooth_protocol::AgentStatus::Running)
            .await;
        session.record_user_message(prompt_text.clone()).await;
        session
            .emit_event(
                &ctx,
                smooth_protocol::EventMsg::UserMessage(prompt_text.clone()),
            )
            .await;

        let prompt = Message::User {
            content: OneOrMany::one(UserContent::Text(Text {
                text: prompt_text.clone(),
            })),
        };
        let history = session.history().await;
        let assistant = session
            .model()
            .complete_turn(prompt, &history, |delta| {
                let _ = &delta;
            })
            .await
            .ok()?;

        session.record_assistant_message(assistant.clone()).await;
        session
            .emit_event(
                &ctx,
                smooth_protocol::EventMsg::AgentMessage(assistant.clone()),
            )
            .await;
        Some(assistant)
    }
}
