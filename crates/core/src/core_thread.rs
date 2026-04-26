use std::path::PathBuf;

use anyhow::Result;

use crate::core::Core;
use crate::provider::SessionModel;
use smooth_protocol::ThreadId;

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: Option<PathBuf>,
}

impl CoreThread {
    pub(crate) fn new(id: ThreadId) -> Result<Self> {
        let model = SessionModel::from_env()?;
        Ok(Self {
            core: Core::new(id, model),
            rollout_path: None,
        })
    }

    pub(crate) async fn run_user_input(&self, input: String) -> Result<String> {
        self.core.run_user_input(input).await
    }
}
