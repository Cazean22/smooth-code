use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use smooth_protocol::{
    AgentPath, AgentStatus, CollabResumeBeginEvent, CollabResumeEndEvent, Event, EventMsg, Op,
    SessionSource, SubAgentSource, ThreadId,
};
use smooth_state_db::StateDbHandle;
use tokio::sync::{RwLock, broadcast};
use tools::{DynamicToolClient, DynamicToolClientFactory};
use uuid::Uuid;

use crate::{
    ThreadSummary,
    agent::AgentControl,
    core_thread::CoreThread,
    provider::SessionModelFactory,
    rollout::{find_thread_path, list_threads, load_resume_state, workspace_root},
};

pub struct StartedThread {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
}

pub struct ResumedThread {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
    pub initial_messages: Vec<smooth_protocol::EventMsg>,
}

pub struct ThreadManagerState {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
    dynamic_tool_client_factory: Option<Arc<dyn DynamicToolClientFactory>>,
    model_factory: Option<Arc<dyn SessionModelFactory>>,
    agent_control: AgentControl,
    state_db: StateDbHandle,
}

impl ThreadManagerState {
    pub async fn new(
        dynamic_tool_client_factory: Option<Arc<dyn DynamicToolClientFactory>>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
    ) -> Result<Self> {
        let threads = Arc::new(RwLock::new(HashMap::new()));
        let workspace_root = workspace_root()?;
        let state_db =
            StateDbHandle::open(workspace_root.join(".smooth-code").join("state.db")).await?;
        let agent_control = AgentControl::new();
        agent_control.attach_runtime(
            Arc::clone(&threads),
            dynamic_tool_client_factory.clone(),
            model_factory.clone(),
            state_db.clone(),
        );
        Ok(Self {
            threads,
            dynamic_tool_client_factory,
            model_factory,
            agent_control,
            state_db,
        })
    }

    #[tracing::instrument(name = "core.thread_manager.start_thread", skip(self))]
    pub async fn start_thread(&self) -> Result<StartedThread> {
        let thread_id = ThreadId::new();
        let thread = Arc::new(
            CoreThread::new(
                thread_id,
                self.dynamic_tool_client(thread_id),
                self.model_factory.clone(),
                SessionSource::Cli,
                self.agent_control.clone(),
            )
            .await?,
        );
        let rollout_path = thread.rollout_path().clone();

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        let _ = self.agent_control.register_session_root(thread_id);
        self.state_db
            .upsert_thread(&thread_id.to_string(), None, None, None)
            .await?;
        Ok(StartedThread {
            thread_id,
            rollout_path,
        })
    }

    #[tracing::instrument(name = "core.thread_manager.resume_thread", skip(self), fields(thread_id = %thread_id))]
    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ResumedThread> {
        if let Some(thread) = self.threads.read().await.get(&thread_id).cloned() {
            return Ok(ResumedThread {
                thread_id,
                rollout_path: thread.rollout_path().clone(),
                initial_messages: Vec::new(),
            });
        }

        let workspace_root = workspace_root()?;
        let rollout_path = find_thread_path(&workspace_root, thread_id).await?;
        let resume_state = load_resume_state(&rollout_path).await?;
        let mut initial_messages = resume_state.initial_messages.clone();
        let thread = Arc::new(
            CoreThread::resume(
                rollout_path.clone(),
                resume_state,
                self.dynamic_tool_client(thread_id),
                self.model_factory.clone(),
                SessionSource::Cli,
                self.agent_control.clone(),
            )
            .await?,
        );

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        let _ = self.agent_control.register_session_root(thread_id);
        drop(threads);
        self.state_db
            .upsert_thread(&thread_id.to_string(), None, None, None)
            .await?;
        initial_messages.extend(self.resume_child_subtree(thread_id).await?);
        Ok(ResumedThread {
            thread_id,
            rollout_path,
            initial_messages,
        })
    }

    #[tracing::instrument(name = "core.thread_manager.list_threads", skip(self))]
    pub async fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let workspace_root = workspace_root()?;
        list_threads(&workspace_root).await
    }

    #[tracing::instrument(name = "core.thread_manager.emit_session_configured", skip(self), fields(thread_id = %thread_id))]
    pub async fn emit_session_configured(&self, thread_id: ThreadId) -> Result<()> {
        self.get(thread_id).await?.emit_session_configured().await;
        Ok(())
    }

    pub async fn start_user_input(&self, thread_id: ThreadId, input: String) -> Result<String> {
        let thread = self.get(thread_id).await?;
        thread.start_user_input(input).await
    }

    pub async fn submit(&self, thread_id: ThreadId, op: Op) -> Result<String> {
        let thread = self.get(thread_id).await?;
        thread.submit(op).await
    }

    pub(crate) async fn send_op(&self, thread_id: ThreadId, op: Op) -> Result<String> {
        self.submit(thread_id, op).await
    }

    pub async fn subscribe(&self, thread_id: ThreadId) -> Result<broadcast::Receiver<Event>> {
        let thread = self.get(thread_id).await?;
        Ok(thread.subscribe())
    }

    pub(crate) fn agent_control(&self) -> AgentControl {
        self.agent_control.clone()
    }

    pub(crate) async fn remove_thread(&self, thread_id: ThreadId) -> Option<Arc<CoreThread>> {
        self.threads.write().await.remove(&thread_id)
    }

    pub(crate) async fn shutdown_and_remove_thread(
        &self,
        thread_id: ThreadId,
        reason: &str,
    ) -> Result<()> {
        if let Some(thread) = self.get(thread_id).await.ok() {
            let _ = thread.submit(Op::Shutdown).await;
            thread.core.session.abort_all_tasks(reason).await;
        }
        self.remove_thread(thread_id).await;
        Ok(())
    }

    async fn get(&self, thread_id: ThreadId) -> Result<Arc<CoreThread>> {
        self.threads
            .read()
            .await
            .get(&thread_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown thread id: {thread_id}"))
    }

    fn dynamic_tool_client(&self, thread_id: ThreadId) -> Option<Arc<dyn DynamicToolClient>> {
        self.dynamic_tool_client_factory
            .as_ref()
            .map(|factory| factory.build(thread_id))
    }

    async fn resume_child_subtree(&self, root_thread_id: ThreadId) -> Result<Vec<EventMsg>> {
        let mut queue = VecDeque::from([root_thread_id]);
        let workspace_root = workspace_root()?;
        let mut events = Vec::new();

        while let Some(parent_thread_id) = queue.pop_front() {
            let child_edges = self
                .state_db
                .list_open_children(&parent_thread_id.to_string())
                .await?;
            for edge in child_edges {
                let child_thread_id =
                    edge.child_thread_id.parse::<ThreadId>().with_context(|| {
                        format!(
                            "invalid child thread id `{}` in state db",
                            edge.child_thread_id
                        )
                    })?;
                let call_id = Uuid::now_v7().to_string();
                let thread_row = match self.state_db.get_thread(&edge.child_thread_id).await {
                    Ok(Some(row)) => row,
                    Ok(None) => {
                        tracing::warn!(
                            parent_thread_id = %parent_thread_id,
                            child_thread_id = %child_thread_id,
                            "missing thread metadata for persisted child edge"
                        );
                        events.push(EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                            call_id,
                            sender_thread_id: parent_thread_id,
                            receiver_thread_id: child_thread_id,
                            receiver_agent_nickname: None,
                            receiver_agent_role: None,
                            status: AgentStatus::Errored("missing thread metadata".to_string()),
                        }));
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(
                            parent_thread_id = %parent_thread_id,
                            child_thread_id = %child_thread_id,
                            error = %err,
                            "failed to load thread metadata for persisted child edge"
                        );
                        continue;
                    }
                };
                events.push(EventMsg::CollabResumeBegin(CollabResumeBeginEvent {
                    call_id: call_id.clone(),
                    sender_thread_id: parent_thread_id,
                    receiver_thread_id: child_thread_id,
                    receiver_agent_nickname: thread_row.agent_nickname.clone(),
                    receiver_agent_role: thread_row.agent_role.clone(),
                }));

                let result = self
                    .resume_child_thread(
                        &workspace_root,
                        parent_thread_id,
                        child_thread_id,
                        thread_row.agent_path.as_deref(),
                        thread_row.agent_nickname.clone(),
                        thread_row.agent_role.clone(),
                    )
                    .await;
                match result {
                    Ok(()) => {
                        queue.push_back(child_thread_id);
                        events.push(EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                            call_id,
                            sender_thread_id: parent_thread_id,
                            receiver_thread_id: child_thread_id,
                            receiver_agent_nickname: thread_row.agent_nickname,
                            receiver_agent_role: thread_row.agent_role,
                            status: self.agent_control.get_status(child_thread_id),
                        }));
                    }
                    Err(err) => {
                        tracing::warn!(
                            parent_thread_id = %parent_thread_id,
                            child_thread_id = %child_thread_id,
                            error = %err,
                            "failed to resume child thread from persisted edge"
                        );
                        events.push(EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                            call_id,
                            sender_thread_id: parent_thread_id,
                            receiver_thread_id: child_thread_id,
                            receiver_agent_nickname: thread_row.agent_nickname,
                            receiver_agent_role: thread_row.agent_role,
                            status: AgentStatus::Errored(err.to_string()),
                        }));
                    }
                }
            }
        }

        Ok(events)
    }

    async fn resume_child_thread(
        &self,
        workspace_root: &std::path::Path,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        agent_path: Option<&str>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    ) -> Result<()> {
        if self.threads.read().await.contains_key(&child_thread_id) {
            return Ok(());
        }
        let parent_depth = self
            .agent_control
            .registry()
            .agent_metadata_for_thread(parent_thread_id)
            .map(|metadata| metadata.depth)
            .ok_or_else(|| anyhow::anyhow!("parent thread not registered: {parent_thread_id}"))?;
        let depth = parent_depth + 1;
        if depth > 8 {
            anyhow::bail!("agent depth limit exceeded during resume: {depth} > 8");
        }
        let agent_path = agent_path.ok_or_else(|| {
            anyhow::anyhow!("missing agent_path for child thread {child_thread_id}")
        })?;
        let agent_path = AgentPath::try_from(agent_path)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("invalid agent path `{agent_path}` for thread {child_thread_id}"))?;
        let rollout_path = find_thread_path(workspace_root, child_thread_id).await?;
        let resume_state = load_resume_state(&rollout_path).await?;
        let initial_messages = resume_state.initial_messages.clone();
        let thread = Arc::new(
            CoreThread::resume(
                rollout_path,
                resume_state,
                self.dynamic_tool_client(child_thread_id),
                self.model_factory.clone(),
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth,
                    agent_path: Some(agent_path.clone()),
                    agent_nickname: agent_nickname.clone(),
                    agent_role: agent_role.clone(),
                }),
                self.agent_control.clone(),
            )
            .await?,
        );
        self.threads.write().await.insert(child_thread_id, thread);
        self.agent_control.register_existing_agent(
            crate::agent::registry::AgentMetadata {
                agent_id: Some(child_thread_id),
                agent_path,
                agent_nickname,
                agent_role,
                parent_thread_id: Some(parent_thread_id),
                depth,
            },
            &initial_messages,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use anyhow::Result;
    use futures_util::stream;
    use rig::{
        agent::FinalResponse,
        message::{Message, Text},
    };
    use tempfile::TempDir;
    use tokio::sync::watch;
    use tools::DynamicToolClient;

    use super::ThreadManagerState;
    use crate::{
        SessionModel, SessionModelDriver, SessionModelFactory, SessionStream,
        agent::{AgentControl, role::RoleOverride},
        provider::SessionStreamEvent,
        rollout::find_thread_path,
        test_support::cwd_test_lock,
    };
    use smooth_protocol::{AgentStatus, EventMsg, ThreadId};

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
            _thread_id: ThreadId,
            _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
            _current_turn_id: Arc<watch::Sender<Option<String>>>,
            _role_override: RoleOverride,
            _agent_control: AgentControl,
        ) -> Result<SessionModel> {
            Ok(self.model.clone())
        }
    }

    #[tokio::test]
    async fn resume_thread_rehydrates_open_subtree() {
        let _cwd_guard = cwd_test_lock().lock().expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await
            .expect("spawn child");
        let child_id = child.agent_id.expect("child id");
        let _grandchild = control
            .spawn_agent(child_id, "grandchild task".to_string())
            .await
            .expect("spawn grandchild");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(manager);

        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let resumed = resumed_manager
            .resume_thread(root_id)
            .await
            .expect("resume root");
        let resume_events = resumed
            .initial_messages
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    EventMsg::CollabResumeBegin(_) | EventMsg::CollabResumeEnd(_)
                )
            })
            .count();
        assert_eq!(resume_events, 4);
        assert_eq!(
            resumed_manager
                .agent_control()
                .registry()
                .live_agents()
                .len(),
            3
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn resume_thread_skips_closed_edges() {
        let _cwd_guard = cwd_test_lock().lock().expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await
            .expect("spawn child");
        control
            .close_agent(root_id, child.agent_path.as_str())
            .await
            .expect("close child");
        drop(manager);

        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let resumed = resumed_manager
            .resume_thread(root_id)
            .await
            .expect("resume root");
        assert!(
            resumed
                .initial_messages
                .iter()
                .all(|event| !matches!(event, EventMsg::CollabResumeBegin(_)))
        );
        assert_eq!(
            resumed_manager
                .agent_control()
                .registry()
                .live_agents()
                .len(),
            1
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn resume_thread_warns_and_leaves_edge_open_when_child_rollout_is_missing() {
        let _cwd_guard = cwd_test_lock().lock().expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await
            .expect("spawn child");
        let child_id = child.agent_id.expect("child id");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rollout_path = find_thread_path(workspace.path(), child_id)
            .await
            .expect("find child rollout");
        tokio::fs::remove_file(&rollout_path)
            .await
            .expect("remove child rollout");
        drop(manager);

        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let resumed = resumed_manager
            .resume_thread(root_id)
            .await
            .expect("resume root");
        assert!(resumed.initial_messages.iter().any(|event| {
            matches!(
                event,
                EventMsg::CollabResumeEnd(end)
                    if end.receiver_thread_id == child_id
                        && matches!(end.status, AgentStatus::Errored(_))
            )
        }));
        assert_eq!(
            resumed_manager
                .agent_control()
                .registry()
                .live_agents()
                .len(),
            1
        );
        assert_eq!(
            resumed_manager
                .state_db
                .list_open_children(&root_id.to_string())
                .await
                .expect("list open children")
                .len(),
            1
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }
}
