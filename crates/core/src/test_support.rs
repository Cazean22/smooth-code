use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::RwLock;
use tools::AskUserClient;

use crate::{
    SessionModel, SessionModelFactory,
    agent::{AgentControl, SystemPromptKind},
    provider::stub_session_model_factory,
};

pub struct StubSessionModelFactory {
    inner: Arc<dyn SessionModelFactory>,
}

impl StubSessionModelFactory {
    pub fn new(models: HashMap<smooth_protocol::ThreadId, SessionModel>) -> Self {
        Self {
            inner: stub_session_model_factory(models),
        }
    }
}

impl SessionModelFactory for StubSessionModelFactory {
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        self.inner.build(
            cwd,
            thread_id,
            ask_user_client,
            current_turn_id,
            system_prompt_kind,
            agent_control,
            plan_mode,
        )
    }
}

#[cfg(test)]
pub(crate) fn cwd_test_lock() -> &'static tokio::sync::Mutex<()> {
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    &CWD_LOCK
}
