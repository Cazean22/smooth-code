use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow};
use smooth_protocol::{AgentStatus, Op, SessionSource, SubAgentSource, ThreadId};
use tokio::sync::{RwLock, watch};
use tools::DynamicToolClientFactory;

use crate::{
    agent::registry::{AgentMetadata, AgentRegistry},
    core_thread::CoreThread,
    provider::SessionModelFactory,
};

const AGENT_MAX_DEPTH: i32 = 8;
const AGENT_MAX_THREADS: usize = 16;

#[derive(Clone)]
pub(crate) struct AgentControl {
    state: Arc<AgentControlState>,
}

struct AgentControlState {
    registry: AgentRegistry,
    statuses: Mutex<HashMap<ThreadId, watch::Sender<AgentStatus>>>,
    runtime: Mutex<Option<AgentControlRuntime>>,
}

#[derive(Clone)]
struct AgentControlRuntime {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
    dynamic_tool_client_factory: Option<Arc<dyn DynamicToolClientFactory>>,
    model_factory: Option<Arc<dyn SessionModelFactory>>,
}

impl AgentControl {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AgentControlState {
                registry: AgentRegistry::new(),
                statuses: Mutex::new(HashMap::new()),
                runtime: Mutex::new(None),
            }),
        }
    }

    pub(crate) fn attach_runtime(
        &self,
        threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
        dynamic_tool_client_factory: Option<Arc<dyn DynamicToolClientFactory>>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
    ) {
        *self
            .state
            .runtime
            .lock()
            .expect("agent control runtime mutex should lock") = Some(AgentControlRuntime {
            threads,
            dynamic_tool_client_factory,
            model_factory,
        });
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

    pub(crate) async fn spawn_agent(
        &self,
        parent_thread_id: ThreadId,
        message: String,
    ) -> Result<AgentMetadata> {
        let runtime = self
            .state
            .runtime
            .lock()
            .expect("agent control runtime mutex should lock")
            .clone()
            .ok_or_else(|| anyhow!("agent control runtime is not attached"))?;
        let reservation = self
            .state
            .registry
            .reserve_spawn_slot(parent_thread_id, AGENT_MAX_DEPTH, AGENT_MAX_THREADS)
            .map_err(anyhow::Error::msg)?;

        let child_thread_id = ThreadId::new();
        let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: reservation.depth(),
            agent_path: Some(reservation.agent_path().clone()),
            agent_nickname: Some(reservation.agent_path().name().to_string()),
            agent_role: None,
        });
        let dynamic_tool_client = runtime
            .dynamic_tool_client_factory
            .as_ref()
            .map(|factory| factory.build(child_thread_id));
        let child_thread = Arc::new(
            CoreThread::new(
                child_thread_id,
                dynamic_tool_client,
                runtime.model_factory.clone(),
                child_source,
                self.clone(),
            )
            .await?,
        );

        {
            let mut threads = runtime.threads.write().await;
            threads.insert(child_thread_id, Arc::clone(&child_thread));
        }
        self.ensure_status_sender(child_thread_id, AgentStatus::PendingInit);

        if let Err(err) = child_thread.submit(Op::UserInput(message)).await {
            runtime.threads.write().await.remove(&child_thread_id);
            self.remove_status_sender(child_thread_id);
            return Err(err);
        }

        let agent_path = reservation.agent_path().clone();
        let agent_nickname = reservation.agent_path().name().to_string();
        let depth = reservation.depth();

        reservation
            .commit(AgentMetadata {
                agent_id: Some(child_thread_id),
                agent_path,
                agent_nickname: Some(agent_nickname),
                agent_role: None,
                parent_thread_id: Some(parent_thread_id),
                depth,
            })
            .map_err(anyhow::Error::msg)
    }

    fn ensure_status_sender(&self, thread_id: ThreadId, status: AgentStatus) {
        self.state
            .statuses
            .lock()
            .expect("agent control status mutex should lock")
            .entry(thread_id)
            .or_insert_with(|| watch::channel(status).0);
    }

    fn remove_status_sender(&self, thread_id: ThreadId) {
        self.state
            .statuses
            .lock()
            .expect("agent control status mutex should lock")
            .remove(&thread_id);
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use anyhow::Result;
    use rig::{
        agent::FinalResponse,
        message::{Message, Text},
    };
    use tempfile::TempDir;
    use tokio::sync::watch;

    use super::AgentControl;
    use crate::{
        SessionModel, SessionModelDriver, SessionModelFactory, SessionStream,
        provider::SessionStreamEvent, thread_manager::ThreadManagerState,
    };
    use futures_util::stream;
    use smooth_protocol::{AgentStatus, ThreadId};
    use tools::DynamicToolClient;

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

    struct StubDriver {
        text: String,
    }

    impl SessionModelDriver for StubDriver {
        fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream> {
            let _ = (prompt, history);
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionStreamEvent::StreamAssistantItem(
                    crate::SessionAssistantContent::Text(Text {
                        text: self.text.clone(),
                    }),
                )),
                Ok(SessionStreamEvent::FinalResponse(FinalResponse::empty())),
            ])))
        }
    }

    struct StubFactory {
        model: SessionModel,
    }

    impl SessionModelFactory for StubFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            thread_id: ThreadId,
            _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
            _current_turn_id: Arc<watch::Sender<Option<String>>>,
        ) -> Result<SessionModel> {
            let _ = thread_id;
            Ok(self.model.clone())
        }
    }

    #[tokio::test]
    async fn spawn_agent_creates_child_and_tracks_it_live() {
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "child".into(),
                })),
            })),
        );
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;

        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await
            .expect("spawn child");

        assert!(child.agent_path.as_str().starts_with("/root/"));
        assert_eq!(control.registry().live_agents().len(), 2);

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }
}
