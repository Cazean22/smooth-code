use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rig::message::Message;
use smooth_protocol::{Event, EventMsg, SessionConfiguredEvent, SessionSource};
use tokio::sync::{RwLock, broadcast};
use tools::DynamicToolClient;

use crate::provider::{SessionModelFactory, default_session_model_factory};
use crate::{
    agent::{AgentControl, role::role_override_from_source},
    core::Core,
    rollout::{ResumeState, RolloutRecorder, workspace_root},
};
use smooth_protocol::{Op, ThreadId};

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: PathBuf,
    dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
}

impl CoreThread {
    #[tracing::instrument(
        name = "core.thread.new",
        skip(dynamic_tool_client, model_factory, agent_control),
        fields(thread_id = %id)
    )]
    pub(crate) async fn new(
        id: ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        session_source: SessionSource,
        agent_control: AgentControl,
    ) -> Result<Self> {
        Self::new_with_history(
            id,
            dynamic_tool_client,
            model_factory,
            session_source,
            agent_control,
            Vec::new(),
        )
        .await
    }

    #[tracing::instrument(
        name = "core.thread.new_with_history",
        skip(dynamic_tool_client, model_factory, agent_control, initial_history),
        fields(thread_id = %id, history_items = initial_history.len())
    )]
    pub(crate) async fn new_with_history(
        id: ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        session_source: SessionSource,
        agent_control: AgentControl,
        initial_history: Vec<Message>,
    ) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let current_turn_id = Arc::new(RwLock::new(None));
        let role_override = role_override_from_source(&session_source);
        let resolved_factory = model_factory.unwrap_or_else(default_session_model_factory);
        let plan_mode = false;
        let model = resolved_factory.build(
            cwd.clone(),
            id,
            dynamic_tool_client.clone(),
            Arc::clone(&current_turn_id),
            role_override,
            agent_control.clone(),
            plan_mode,
        )?;
        let workspace_root = workspace_root()?;
        let rollout = RolloutRecorder::create(&workspace_root, id, &cwd).await?;
        for message in &initial_history {
            if let Some(history_message) = history_message_from_message(message) {
                rollout
                    .append(crate::rollout::PersistedItem::HistoryMessage(
                        history_message,
                    ))
                    .await?;
            }
        }
        let rollout_path = rollout.path().to_path_buf();
        Ok(Self {
            core: Core::new(
                id,
                model,
                initial_history,
                0,
                rollout,
                current_turn_id,
                dynamic_tool_client.clone(),
                session_source,
                agent_control,
                plan_mode,
                resolved_factory.clone(),
            ),
            rollout_path,
            dynamic_tool_client,
        })
    }

    #[tracing::instrument(
        name = "core.thread.resume",
        skip(path, state, dynamic_tool_client, model_factory, agent_control),
        fields(thread_id = %state.thread_id)
    )]
    pub(crate) async fn resume(
        path: PathBuf,
        state: ResumeState,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        session_source: SessionSource,
        agent_control: AgentControl,
    ) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let current_turn_id = Arc::new(RwLock::new(None));
        let role_override = role_override_from_source(&session_source);
        let resolved_factory = model_factory.unwrap_or_else(default_session_model_factory);
        let plan_mode = false;
        let model = resolved_factory.build(
            cwd,
            state.thread_id,
            dynamic_tool_client.clone(),
            Arc::clone(&current_turn_id),
            role_override,
            agent_control.clone(),
            plan_mode,
        )?;
        let rollout = RolloutRecorder::resume(path.clone()).await?;
        Ok(Self {
            core: Core::new(
                state.thread_id,
                model,
                state.history,
                state.next_turn_index,
                rollout,
                current_turn_id,
                dynamic_tool_client.clone(),
                session_source,
                agent_control,
                plan_mode,
                resolved_factory.clone(),
            ),
            rollout_path: path,
            dynamic_tool_client,
        })
    }

    pub(crate) async fn start_user_input(&self, input: String) -> Result<String> {
        self.core.start_user_input(input).await
    }

    pub(crate) async fn submit(&self, op: Op) -> Result<String> {
        self.core.submit(op).await
    }

    /// Toggle plan mode for this thread. Returns the new effective plan-mode state.
    pub(crate) async fn set_plan_mode(&self, enabled: bool) -> Result<bool> {
        self.core
            .session
            .apply_plan_mode(enabled, self.dynamic_tool_client.clone())
            .await
    }

    #[tracing::instrument(name = "core.thread.emit_session_configured", skip(self), fields(thread_id = %self.core.session.id))]
    pub(crate) async fn emit_session_configured(&self) {
        self.core
            .emit_session_event(EventMsg::SessionConfigured(SessionConfiguredEvent {
                thread_id: self.core.session.id.to_string(),
                rollout_path: Some(self.rollout_path.display().to_string()),
            }))
            .await;
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.core.subscribe()
    }

    pub(crate) fn rollout_path(&self) -> &PathBuf {
        &self.rollout_path
    }

    pub(crate) async fn flush_rollout(&self) -> Result<()> {
        self.core.flush_rollout().await
    }
}

fn history_message_from_message(message: &Message) -> Option<crate::rollout::HistoryMessage> {
    match message {
        Message::System { .. } => None,
        Message::User { content } => content.iter().find_map(|content| match content {
            rig::message::UserContent::Text(text) => {
                Some(crate::rollout::HistoryMessage::UserText {
                    text: text.text.clone(),
                })
            }
            rig::message::UserContent::Image(_) | rig::message::UserContent::Audio(_) => None,
            _ => None,
        }),
        Message::Assistant { content, .. } => content.iter().find_map(|content| match content {
            rig::message::AssistantContent::Text(text) => {
                Some(crate::rollout::HistoryMessage::AssistantText {
                    text: text.text.clone(),
                })
            }
            _ => None,
        }),
    }
}
