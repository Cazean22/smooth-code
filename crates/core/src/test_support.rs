use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::RwLock;
use tools::AskUserClient;

use crate::{
    SessionModel, SessionModelFactory,
    agent::{AgentControl, role::RoleOverride},
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
        ask_user_client: Option<Arc<dyn AskUserClient>>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        role_override: RoleOverride,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        self.inner.build(
            cwd,
            thread_id,
            ask_user_client,
            current_turn_id,
            role_override,
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
