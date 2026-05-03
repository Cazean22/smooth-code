use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use smooth_protocol::{AgentStatus, ThreadId};
use tokio::sync::watch;

use crate::agent::registry::{AgentMetadata, AgentRegistry};

#[derive(Clone)]
pub(crate) struct AgentControl {
    state: Arc<AgentControlState>,
}

struct AgentControlState {
    registry: AgentRegistry,
    statuses: Mutex<HashMap<ThreadId, watch::Sender<AgentStatus>>>,
}

impl AgentControl {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AgentControlState {
                registry: AgentRegistry::new(),
                statuses: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(crate) fn register_session_root(
        &self,
        thread_id: ThreadId,
    ) -> Result<AgentMetadata, String> {
        let metadata = self.state.registry.register_root_thread(thread_id)?;
        let mut statuses = self
            .state
            .statuses
            .lock()
            .expect("agent control status mutex should lock");
        statuses
            .entry(thread_id)
            .or_insert_with(|| watch::channel(AgentStatus::PendingInit).0);
        Ok(metadata)
    }

    pub(crate) fn get_status(&self, thread_id: ThreadId) -> AgentStatus {
        self.state
            .statuses
            .lock()
            .expect("agent control status mutex should lock")
            .get(&thread_id)
            .map(|status| status.borrow().clone())
            .unwrap_or(AgentStatus::NotFound)
    }

    pub(crate) fn subscribe_status(&self, thread_id: ThreadId) -> watch::Receiver<AgentStatus> {
        let mut statuses = self
            .state
            .statuses
            .lock()
            .expect("agent control status mutex should lock");
        statuses
            .entry(thread_id)
            .or_insert_with(|| watch::channel(AgentStatus::NotFound).0)
            .subscribe()
    }

    pub(crate) fn set_status(&self, thread_id: ThreadId, status: AgentStatus) {
        if let Some(sender) = self
            .state
            .statuses
            .lock()
            .expect("agent control status mutex should lock")
            .get(&thread_id)
            .cloned()
        {
            sender.send_replace(status);
        }
    }

    pub(crate) fn registry(&self) -> AgentRegistry {
        self.state.registry.clone()
    }
}

#[cfg(test)]
mod tests {
    use smooth_protocol::{AgentStatus, ThreadId};

    use super::AgentControl;

    #[test]
    fn clones_share_registry_and_status_state() {
        let control = AgentControl::new();
        let clone = control.clone();
        let root_id = ThreadId::new();

        control
            .register_session_root(root_id)
            .expect("root registration");
        clone.set_status(root_id, AgentStatus::Running);

        assert_eq!(control.get_status(root_id), AgentStatus::Running);
        assert_eq!(clone.registry().live_agents().len(), 1);
    }
}
