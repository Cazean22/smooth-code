use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::RwLock;
use tools::DynamicToolClient;

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
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        role_override: RoleOverride,
        agent_control: AgentControl,
    ) -> Result<SessionModel> {
        self.inner.build(
            cwd,
            thread_id,
            dynamic_tool_client,
            current_turn_id,
            role_override,
            agent_control,
        )
    }
}

#[cfg(test)]
pub(crate) fn cwd_test_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{LazyLock, Mutex};

    static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    &CWD_LOCK
}
