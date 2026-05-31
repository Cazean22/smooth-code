use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use smooth_protocol::{
    AgentPath, AgentStatus, CollabAgentStatusEntry, CollabResumeBeginEvent, CollabResumeEndEvent,
    ErrorInfo, Event, EventMsg, Op, SessionSource, SubAgentSource, ThreadId,
};
use smooth_state_db::StateDbHandle;
use tokio::sync::{RwLock, broadcast};
use tools::{AskUserClient, AskUserClientFactory};
use uuid::Uuid;

use crate::{
    ThreadSummary,
    agent::{AgentControl, registry::AgentMetadata, status::last_assistant_message},
    core_thread::CoreThread,
    error::{CoreError, CoreResult},
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
    ask_user_client_factory: Option<AskUserClientFactory>,
    model_factory: Option<Arc<dyn SessionModelFactory>>,
    agent_control: AgentControl,
    state_db: StateDbHandle,
}

impl ThreadManagerState {
    pub async fn new(
        ask_user_client_factory: Option<AskUserClientFactory>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
    ) -> CoreResult<Self> {
        let threads = Arc::new(RwLock::new(HashMap::new()));
        let workspace_root = workspace_root().map_err(CoreError::rollout)?;
        let state_db =
            StateDbHandle::open(workspace_root.join(".smooth-code").join("state.db")).await?;
        let agent_control = AgentControl::new();
        agent_control.attach_runtime(
            Arc::clone(&threads),
            ask_user_client_factory.clone(),
            model_factory.clone(),
            state_db.clone(),
        )?;
        Ok(Self {
            threads,
            ask_user_client_factory,
            model_factory,
            agent_control,
            state_db,
        })
    }

    #[tracing::instrument(name = "core.thread_manager.start_thread", skip(self))]
    pub async fn start_thread(&self) -> CoreResult<StartedThread> {
        let thread_id = ThreadId::new();
        let thread = Arc::new(
            CoreThread::new(
                thread_id,
                self.ask_user_client(thread_id),
                self.model_factory.clone(),
                SessionSource::Cli,
                self.agent_control.clone(),
            )
            .await?,
        );
        let rollout_path = thread.rollout_path().clone();

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        self.agent_control.register_session_root(thread_id)?;
        self.state_db
            .upsert_thread(&thread_id.to_string(), None, None, None)
            .await?;
        Ok(StartedThread {
            thread_id,
            rollout_path,
        })
    }

    #[tracing::instrument(name = "core.thread_manager.resume_thread", skip(self), fields(thread_id = %thread_id))]
    pub async fn resume_thread(&self, thread_id: ThreadId) -> CoreResult<ResumedThread> {
        if let Some(thread) = self.threads.read().await.get(&thread_id).cloned() {
            return Ok(ResumedThread {
                thread_id,
                rollout_path: thread.rollout_path().clone(),
                initial_messages: Vec::new(),
            });
        }

        let workspace_root = workspace_root().map_err(CoreError::rollout)?;
        let rollout_path = find_thread_path(&workspace_root, thread_id)
            .await
            .map_err(CoreError::rollout)?;
        let resume_state = load_resume_state(&rollout_path)
            .await
            .map_err(CoreError::rollout)?;
        let mut initial_messages = resume_state.initial_messages.clone();
        let thread = Arc::new(
            CoreThread::resume(
                rollout_path.clone(),
                resume_state,
                self.ask_user_client(thread_id),
                self.model_factory.clone(),
                SessionSource::Cli,
                self.agent_control.clone(),
            )
            .await?,
        );

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        self.agent_control.register_session_root(thread_id)?;
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
    pub async fn list_threads(&self) -> CoreResult<Vec<ThreadSummary>> {
        let workspace_root = workspace_root().map_err(CoreError::rollout)?;
        list_threads(&workspace_root)
            .await
            .map_err(CoreError::rollout)
    }

    #[tracing::instrument(name = "core.thread_manager.emit_session_configured", skip(self), fields(thread_id = %thread_id))]
    pub async fn emit_session_configured(&self, thread_id: ThreadId) -> CoreResult<()> {
        self.get(thread_id).await?.emit_session_configured().await;
        Ok(())
    }

    pub async fn start_user_input(&self, thread_id: ThreadId, input: String) -> CoreResult<String> {
        let thread = self.get(thread_id).await?;
        thread.start_user_input(input).await
    }

    pub async fn submit(&self, thread_id: ThreadId, op: Op) -> CoreResult<String> {
        let thread = self.get(thread_id).await?;
        thread.submit(op).await
    }

    /// Toggle plan mode for the given thread. Returns the new effective state.
    pub async fn set_plan_mode(&self, thread_id: ThreadId, enabled: bool) -> CoreResult<bool> {
        let thread = self.get(thread_id).await?;
        thread.set_plan_mode(enabled).await
    }

    #[allow(dead_code)]
    pub(crate) async fn send_op(&self, thread_id: ThreadId, op: Op) -> CoreResult<String> {
        self.submit(thread_id, op).await
    }

    pub async fn subscribe(&self, thread_id: ThreadId) -> CoreResult<broadcast::Receiver<Event>> {
        let thread = self.get(thread_id).await?;
        Ok(thread.subscribe())
    }

    #[allow(dead_code)]
    pub(crate) fn agent_control(&self) -> AgentControl {
        self.agent_control.clone()
    }

    pub async fn spawn_agent_with_role(
        &self,
        parent_thread_id: ThreadId,
        message: String,
        agent_role: Option<String>,
        model: Option<String>,
        fork_context: bool,
    ) -> CoreResult<CollabAgentStatusEntry> {
        let metadata = self
            .agent_control
            .spawn_agent_with_role(parent_thread_id, message, agent_role, model, fork_context)
            .await?;
        agent_status_entry(&self.agent_control, metadata)
    }

    pub fn list_agents(
        &self,
        author_thread_id: ThreadId,
        path_prefix: Option<&str>,
    ) -> CoreResult<Vec<CollabAgentStatusEntry>> {
        self.agent_control
            .list_agents(author_thread_id, path_prefix)
            .and_then(|agents| {
                agents
                    .into_iter()
                    .map(|agent| agent_status_entry(&self.agent_control, agent))
                    .collect::<CoreResult<Vec<_>>>()
            })
    }

    pub async fn close_agent(
        &self,
        author_thread_id: ThreadId,
        target: &str,
    ) -> CoreResult<AgentStatus> {
        self.agent_control
            .close_agent(author_thread_id, target)
            .await
    }

    #[allow(dead_code)]
    pub(crate) async fn remove_thread(&self, thread_id: ThreadId) -> Option<Arc<CoreThread>> {
        self.threads.write().await.remove(&thread_id)
    }

    #[allow(dead_code)]
    pub(crate) async fn shutdown_and_remove_thread(
        &self,
        thread_id: ThreadId,
        reason: &str,
    ) -> CoreResult<()> {
        if let Ok(thread) = self.get(thread_id).await {
            let _ = thread.submit(Op::Shutdown).await;
            thread.core.session.abort_all_tasks(reason).await;
        }
        self.remove_thread(thread_id).await;
        Ok(())
    }

    async fn get(&self, thread_id: ThreadId) -> CoreResult<Arc<CoreThread>> {
        self.threads
            .read()
            .await
            .get(&thread_id)
            .cloned()
            .ok_or(CoreError::UnknownThread { thread_id })
    }

    fn ask_user_client(&self, thread_id: ThreadId) -> Option<AskUserClient> {
        self.ask_user_client_factory
            .as_ref()
            .map(|factory| factory.build(thread_id))
    }

    async fn resume_child_subtree(&self, root_thread_id: ThreadId) -> CoreResult<Vec<EventMsg>> {
        let mut queue = VecDeque::from([root_thread_id]);
        let workspace_root = workspace_root().map_err(CoreError::rollout)?;
        let mut events = Vec::new();

        while let Some(parent_thread_id) = queue.pop_front() {
            let child_edges = self
                .state_db
                .list_open_children(&parent_thread_id.to_string())
                .await?;
            for edge in child_edges {
                let child_thread_id = edge.child_thread_id.parse::<ThreadId>()?;
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
                            status: AgentStatus::Errored(
                                ErrorInfo::new(
                                    "missing_thread_metadata",
                                    "missing thread metadata",
                                )
                                .with_source("smooth-core"),
                            ),
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
                            status: AgentStatus::Errored(err.to_error_info()),
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
    ) -> CoreResult<()> {
        if self.threads.read().await.contains_key(&child_thread_id) {
            return Ok(());
        }
        let parent_depth = self
            .agent_control
            .registry()
            .agent_metadata_for_thread(parent_thread_id)
            .map(|metadata| metadata.depth)
            .ok_or(CoreError::ParentThreadNotRegistered {
                thread_id: parent_thread_id,
            })?;
        let depth = parent_depth + 1;
        if depth > 8 {
            return Err(CoreError::AgentDepthLimitExceeded {
                depth,
                max_depth: 8,
            });
        }
        let agent_path = agent_path.ok_or(CoreError::MissingAgentPath {
            thread_id: child_thread_id,
        })?;
        let agent_path =
            AgentPath::try_from(agent_path).map_err(|source| CoreError::InvalidAgentPath {
                thread_id: child_thread_id,
                path: agent_path.to_string(),
                source,
            })?;
        let rollout_path = find_thread_path(workspace_root, child_thread_id)
            .await
            .map_err(CoreError::rollout)?;
        let resume_state = load_resume_state(&rollout_path)
            .await
            .map_err(CoreError::rollout)?;
        let initial_messages = resume_state.initial_messages.clone();
        let thread = Arc::new(
            CoreThread::resume(
                rollout_path,
                resume_state,
                self.ask_user_client(child_thread_id),
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

fn agent_status_entry(
    control: &AgentControl,
    metadata: AgentMetadata,
) -> CoreResult<CollabAgentStatusEntry> {
    let thread_id = metadata
        .agent_id
        .ok_or_else(|| CoreError::invariant("listed agent metadata should include a thread id"))?;
    let status = control.get_status(thread_id);
    Ok(CollabAgentStatusEntry {
        thread_id,
        agent_path: metadata.agent_path,
        agent_nickname: metadata.agent_nickname,
        agent_role: metadata.agent_role,
        last_assistant_message: last_assistant_message(&status),
        status,
    })
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
    use tokio::sync::RwLock;
    use tools::AskUserClient;

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
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _role_override: RoleOverride,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            Ok(self.model.clone())
        }
    }

    #[tokio::test]
    async fn resume_thread_rehydrates_open_subtree() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        let child_id = child.agent_id.ok_or_else(|| anyhow::anyhow!("child id"))?;
        let _grandchild = control
            .spawn_agent(child_id, "grandchild task".to_string())
            .await?;
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
        .await?;
        let resumed = resumed_manager.resume_thread(root_id).await?;
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

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn resume_thread_skips_closed_edges() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        control
            .close_agent(root_id, child.agent_path.as_str())
            .await?;
        drop(manager);

        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await?;
        let resumed = resumed_manager.resume_thread(root_id).await?;
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

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn resume_thread_warns_and_leaves_edge_open_when_child_rollout_is_missing() -> Result<()>
    {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        let child_id = child.agent_id.ok_or_else(|| anyhow::anyhow!("child id"))?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rollout_path = find_thread_path(workspace.path(), child_id).await?;
        tokio::fs::remove_file(&rollout_path).await?;
        drop(manager);

        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await?;
        let resumed = resumed_manager.resume_thread(root_id).await?;
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
                .await?
                .len(),
            1
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn resume_thread_does_not_replay_terminal_child_completion_notifications() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "done".to_string(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let _child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
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
        .await?;
        let _resumed = resumed_manager.resume_thread(root_id).await?;
        let mut root_events = resumed_manager.subscribe(root_id).await?;

        let replay =
            tokio::time::timeout(std::time::Duration::from_millis(150), root_events.recv()).await;
        assert!(
            replay.is_err(),
            "resuming a terminal child should not synthesize a fresh completion notification"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }
}
