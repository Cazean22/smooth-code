use std::path::PathBuf;
use std::sync::Arc;

use rig::message::Message;
use smooth_protocol::{
    Event, EventMsg, ProjectInstructions, SessionConfiguredEvent, SessionSource,
};
use tokio::sync::{RwLock, broadcast};
use tools::AskUserClient;

use crate::{
    agent::{AgentControl, SystemPromptKind},
    core::Core,
    error::{CoreError, CoreResult},
    provider::{SessionModelFactory, default_session_model_factory},
    rollout::{ResumeState, RolloutRecorder, workspace_root},
};
use smooth_protocol::{Op, ThreadId};

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: PathBuf,
}

impl CoreThread {
    #[tracing::instrument(
        name = "core.thread.new",
        skip(ask_user_client, model_factory, agent_control),
        fields(thread_id = %id)
    )]
    pub(crate) async fn new(
        id: ThreadId,
        ask_user_client: Option<AskUserClient>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        session_source: SessionSource,
        system_prompt_kind: SystemPromptKind,
        project_instructions: Option<ProjectInstructions>,
        agent_control: AgentControl,
    ) -> CoreResult<Self> {
        Self::new_with_history(
            id,
            ask_user_client,
            model_factory,
            session_source,
            system_prompt_kind,
            project_instructions,
            agent_control,
            Vec::new(),
        )
        .await
    }

    #[tracing::instrument(
        name = "core.thread.new_with_history",
        skip(
            ask_user_client,
            model_factory,
            agent_control,
            initial_history
        ),
        fields(thread_id = %id, history_items = initial_history.len())
    )]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_with_history(
        id: ThreadId,
        ask_user_client: Option<AskUserClient>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        session_source: SessionSource,
        system_prompt_kind: SystemPromptKind,
        project_instructions: Option<ProjectInstructions>,
        agent_control: AgentControl,
        initial_history: Vec<Message>,
    ) -> CoreResult<Self> {
        let cwd = std::env::current_dir()?;
        let current_turn_id = Arc::new(RwLock::new(None));
        let resolved_factory = model_factory.unwrap_or_else(default_session_model_factory);
        let plan_mode = false;
        let model = resolved_factory
            .build(
                cwd.clone(),
                id,
                ask_user_client.clone(),
                Arc::clone(&current_turn_id),
                system_prompt_kind,
                agent_control.clone(),
                plan_mode,
            )
            .map_err(CoreError::provider)?;
        let workspace_root = workspace_root().map_err(CoreError::rollout)?;
        let rollout = RolloutRecorder::create_with_project_instructions(
            &workspace_root,
            id,
            &cwd,
            project_instructions.clone(),
        )
        .await
        .map_err(CoreError::rollout)?;
        for message in &initial_history {
            rollout
                .append(crate::rollout::PersistedItem::HistoryMessage(
                    crate::rollout::HistoryMessage::Full {
                        message: message.clone(),
                    },
                ))
                .await
                .map_err(CoreError::rollout)?;
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
                ask_user_client.clone(),
                session_source,
                system_prompt_kind,
                project_instructions,
                agent_control,
                plan_mode,
                resolved_factory.clone(),
            ),
            rollout_path,
        })
    }

    #[tracing::instrument(
        name = "core.thread.resume",
        skip(
            path,
            state,
            ask_user_client,
            model_factory,
            agent_control
        ),
        fields(thread_id = %state.thread_id)
    )]
    pub(crate) async fn resume(
        path: PathBuf,
        state: ResumeState,
        ask_user_client: Option<AskUserClient>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        session_source: SessionSource,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
    ) -> CoreResult<Self> {
        let cwd = std::env::current_dir()?;
        let current_turn_id = Arc::new(RwLock::new(None));
        let resolved_factory = model_factory.unwrap_or_else(default_session_model_factory);
        let plan_mode = false;
        let model = resolved_factory
            .build(
                cwd,
                state.thread_id,
                ask_user_client.clone(),
                Arc::clone(&current_turn_id),
                system_prompt_kind,
                agent_control.clone(),
                plan_mode,
            )
            .map_err(CoreError::provider)?;
        let rollout = RolloutRecorder::resume(path.clone())
            .await
            .map_err(CoreError::rollout)?;
        Ok(Self {
            core: Core::new(
                state.thread_id,
                model,
                state.history,
                state.next_turn_index,
                rollout,
                current_turn_id,
                ask_user_client.clone(),
                session_source,
                system_prompt_kind,
                state.project_instructions,
                agent_control,
                plan_mode,
                resolved_factory.clone(),
            ),
            rollout_path: path,
        })
    }

    pub(crate) async fn start_user_input(&self, input: String) -> CoreResult<String> {
        self.core.start_user_input(input).await
    }

    pub(crate) async fn submit(&self, op: Op) -> CoreResult<String> {
        self.core.submit(op).await
    }

    /// Toggle plan mode for this thread. Returns the new effective plan-mode state.
    pub(crate) async fn set_plan_mode(&self, enabled: bool) -> CoreResult<bool> {
        self.core.session.apply_plan_mode(enabled).await
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
}
