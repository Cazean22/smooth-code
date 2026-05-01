use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use smooth_protocol::{Event, EventMsg, SessionConfiguredEvent};
use tokio::sync::{broadcast, watch};

use crate::{
    core::Core,
    rollout::{ResumeState, RolloutRecorder, workspace_root},
};
use crate::{provider::SessionModel, tools::DynamicToolClient};
use smooth_protocol::ThreadId;

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: PathBuf,
}

impl CoreThread {
    #[tracing::instrument(
        name = "core.thread.new",
        skip(dynamic_tool_client),
        fields(thread_id = %id)
    )]
    pub(crate) async fn new(
        id: ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
    ) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let (current_turn_id, _) = watch::channel(None);
        let current_turn_id = Arc::new(current_turn_id);
        let model = SessionModel::from_env(
            cwd.clone(),
            id,
            dynamic_tool_client.clone(),
            Arc::clone(&current_turn_id),
        )?;
        let workspace_root = workspace_root()?;
        let rollout = RolloutRecorder::create(&workspace_root, id, &cwd).await?;
        let rollout_path = rollout.path().to_path_buf();
        Ok(Self {
            core: Core::new(
                id,
                model,
                Vec::new(),
                0,
                rollout,
                current_turn_id,
                dynamic_tool_client,
            ),
            rollout_path,
        })
    }

    #[tracing::instrument(
        name = "core.thread.resume",
        skip(path, state, dynamic_tool_client),
        fields(thread_id = %state.thread_id)
    )]
    pub(crate) async fn resume(
        path: PathBuf,
        state: ResumeState,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
    ) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let (current_turn_id, _) = watch::channel(None);
        let current_turn_id = Arc::new(current_turn_id);
        let model = SessionModel::from_env(
            cwd,
            state.thread_id,
            dynamic_tool_client.clone(),
            Arc::clone(&current_turn_id),
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
                dynamic_tool_client,
            ),
            rollout_path: path,
        })
    }

    pub(crate) async fn start_user_input(&self, input: String) -> Result<String> {
        self.core.start_user_input(input).await
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
