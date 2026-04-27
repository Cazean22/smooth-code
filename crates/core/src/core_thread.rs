use std::path::PathBuf;

use anyhow::Result;
use smooth_protocol::{Event, EventMsg, SessionConfiguredEvent};
use tokio::sync::broadcast;

use crate::core::Core;
use crate::provider::SessionModel;
use smooth_protocol::ThreadId;

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: Option<PathBuf>,
}

impl CoreThread {
    pub(crate) fn new(id: ThreadId) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let model = SessionModel::from_env(cwd)?;
        Ok(Self {
            core: Core::new(id, model),
            rollout_path: None,
        })
    }

    pub(crate) async fn start_user_input(&self, input: String) -> Result<String> {
        self.core.start_user_input(input).await
    }

    pub(crate) async fn emit_session_configured(&self) {
        self.core
            .emit_session_event(EventMsg::SessionConfigured(SessionConfiguredEvent {
                thread_id: self.core.session.id.to_string(),
            }))
            .await;
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.core.subscribe()
    }
}
