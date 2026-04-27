use std::path::PathBuf;

use anyhow::Result;
use smooth_protocol::{Event, EventMsg, SessionConfiguredEvent};
use tokio::sync::broadcast;

use crate::{
    core::Core,
    rollout::{ResumeState, RolloutRecorder, workspace_root},
};
use crate::provider::SessionModel;
use smooth_protocol::ThreadId;

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: PathBuf,
}

impl CoreThread {
    pub(crate) async fn new(id: ThreadId) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let model = SessionModel::from_env(cwd.clone())?;
        let workspace_root = workspace_root()?;
        let rollout = RolloutRecorder::create(&workspace_root, id, &cwd).await?;
        let rollout_path = rollout.path().to_path_buf();
        Ok(Self {
            core: Core::new(id, model, Vec::new(), 0, rollout),
            rollout_path,
        })
    }

    pub(crate) async fn resume(path: PathBuf, state: ResumeState) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let model = SessionModel::from_env(cwd)?;
        let rollout = RolloutRecorder::resume(path.clone()).await?;
        Ok(Self {
            core: Core::new(
                state.thread_id,
                model,
                state.history,
                state.next_turn_index,
                rollout,
            ),
            rollout_path: path,
        })
    }

    pub(crate) async fn start_user_input(&self, input: String) -> Result<String> {
        self.core.start_user_input(input).await
    }

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
