use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use smooth_protocol::{
    AgentStatus, CollabAgentCompletedEvent, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent,
    ErrorInfo, EventMsg, Op, SessionSource, SubAgentSource, ThreadId,
};
use smooth_state_db::StateDbHandle;
use tokio::sync::{RwLock, oneshot, watch};
use tools::AskUserClient;
use uuid::Uuid;

use crate::{
    agent::{
        agent_resolver,
        prompt::SystemPromptKind,
        registry::{AgentMetadata, AgentRegistry},
        status::{agent_status_from_event, is_final, last_assistant_message},
    },
    core_thread::CoreThread,
    error::{CoreError, CoreResult},
    provider::SessionModelFactory,
};

const AGENT_MAX_DEPTH: i32 = 8;
const AGENT_MAX_THREADS: usize = 16;

/// Shared in-process handle for agent lifecycle and inter-agent coordination.
#[derive(Clone)]
pub struct AgentControl {
    state: Arc<AgentControlState>,
}

struct AgentControlState {
    registry: AgentRegistry,
    statuses: Mutex<HashMap<ThreadId, watch::Sender<AgentStatus>>>,
    inline_waiters: Mutex<HashMap<ThreadId, oneshot::Sender<InlineChildCompletion>>>,
    runtime: Mutex<Option<AgentControlRuntime>>,
}

pub(crate) struct InlineChildCompletion {
    pub(crate) status: AgentStatus,
    pub(crate) last_assistant_message: Option<String>,
}

pub(crate) type InlineChildCompletionReceiver = oneshot::Receiver<InlineChildCompletion>;

#[derive(Clone)]
struct AgentControlRuntime {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
    ask_user_client: Option<AskUserClient>,
    model_factory: Option<Arc<dyn SessionModelFactory>>,
    state_db: StateDbHandle,
}

impl AgentControl {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AgentControlState {
                registry: AgentRegistry::new(),
                statuses: Mutex::new(HashMap::new()),
                inline_waiters: Mutex::new(HashMap::new()),
                runtime: Mutex::new(None),
            }),
        }
    }

    pub(crate) fn attach_runtime(
        &self,
        threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
        ask_user_client: Option<AskUserClient>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        state_db: StateDbHandle,
    ) -> CoreResult<()> {
        *lock_mutex(&self.state.runtime, "agent_control.runtime")? = Some(AgentControlRuntime {
            threads,
            ask_user_client,
            model_factory,
            state_db,
        });
        Ok(())
    }

    pub(crate) fn register_session_root(&self, thread_id: ThreadId) -> CoreResult<AgentMetadata> {
        let metadata = self
            .state
            .registry
            .register_root_thread(thread_id)
            .map_err(CoreError::registry)?;
        let mut statuses = lock_mutex(&self.state.statuses, "agent_control.statuses")?;
        statuses
            .entry(thread_id)
            .or_insert_with(|| watch::channel(AgentStatus::PendingInit).0);
        Ok(metadata)
    }

    pub(crate) fn register_existing_agent(
        &self,
        metadata: AgentMetadata,
        initial_events: &[EventMsg],
    ) -> CoreResult<AgentMetadata> {
        let registered = self
            .state
            .registry
            .register_existing_thread(metadata.clone(), AGENT_MAX_THREADS)
            .map_err(CoreError::registry)?;
        let status = initial_events
            .iter()
            .filter_map(agent_status_from_event)
            .next_back()
            .unwrap_or(AgentStatus::PendingInit);
        self.ensure_status_sender(
            registered
                .agent_id
                .ok_or_else(|| CoreError::invariant("registered agent is missing thread id"))?,
            status,
        )?;
        self.maybe_start_completion_watcher(registered.clone(), false);
        Ok(registered)
    }

    pub(crate) fn get_status(&self, thread_id: ThreadId) -> AgentStatus {
        let Ok(statuses) = self.state.statuses.lock() else {
            return AgentStatus::Errored(
                ErrorInfo::new("mutex_poisoned", "agent control status mutex was poisoned")
                    .with_source("smooth-core"),
            );
        };
        statuses
            .get(&thread_id)
            .map(|status| status.borrow().clone())
            .unwrap_or(AgentStatus::NotFound)
    }

    pub(crate) fn subscribe_status(
        &self,
        thread_id: ThreadId,
    ) -> CoreResult<watch::Receiver<AgentStatus>> {
        let mut statuses = lock_mutex(&self.state.statuses, "agent_control.statuses")?;
        Ok(statuses
            .entry(thread_id)
            .or_insert_with(|| watch::channel(AgentStatus::NotFound).0)
            .subscribe())
    }

    pub(crate) fn set_status(&self, thread_id: ThreadId, status: AgentStatus) -> CoreResult<()> {
        if let Some(sender) = lock_mutex(&self.state.statuses, "agent_control.statuses")?
            .get(&thread_id)
            .cloned()
        {
            sender.send_replace(status);
        }
        Ok(())
    }

    pub(crate) fn registry(&self) -> AgentRegistry {
        self.state.registry.clone()
    }

    pub(crate) fn resolve_agent_reference(
        &self,
        author_thread_id: ThreadId,
        target: &str,
    ) -> CoreResult<ThreadId> {
        let session_source = self.session_source_for_thread(author_thread_id)?;
        agent_resolver::resolve_agent_reference(&self.state.registry, &session_source, target)
            .map_err(CoreError::control)
    }

    pub(crate) fn list_agents(
        &self,
        author_thread_id: ThreadId,
        path_prefix: Option<&str>,
    ) -> CoreResult<Vec<AgentMetadata>> {
        let session_source = self.session_source_for_thread(author_thread_id)?;
        agent_resolver::list_agents(&self.state.registry, &session_source, path_prefix)
            .map_err(CoreError::control)
    }

    pub(crate) async fn spawn_agent(
        &self,
        parent_thread_id: ThreadId,
        prompt: String,
    ) -> CoreResult<AgentMetadata> {
        self.spawn_agent_with_prompt_kind(
            parent_thread_id,
            prompt,
            SystemPromptKind::DefaultSubagent,
        )
        .await
    }

    pub(crate) async fn spawn_agent_with_prompt_kind(
        &self,
        parent_thread_id: ThreadId,
        prompt: String,
        system_prompt_kind: SystemPromptKind,
    ) -> CoreResult<AgentMetadata> {
        self.spawn_agent_internal(parent_thread_id, prompt, system_prompt_kind, false)
            .await
            .map(|(metadata, _, _)| metadata)
    }

    pub(crate) async fn spawn_agent_with_prompt_kind_inline_wait(
        &self,
        parent_thread_id: ThreadId,
        prompt: String,
        system_prompt_kind: SystemPromptKind,
    ) -> CoreResult<(AgentMetadata, InlineChildCompletionReceiver)> {
        let (metadata, _child_thread_id, waiter) = self
            .spawn_agent_internal(parent_thread_id, prompt, system_prompt_kind, true)
            .await?;
        let waiter =
            waiter.ok_or_else(|| CoreError::invariant("inline waiter should be registered"))?;
        Ok((metadata, waiter))
    }

    pub(crate) async fn spawn_agent_for_tool(
        &self,
        parent_thread_id: ThreadId,
        prompt: String,
        system_prompt_kind: SystemPromptKind,
    ) -> CoreResult<(AgentMetadata, AgentStatus, InlineChildCompletionReceiver)> {
        let call_id = Uuid::now_v7().to_string();
        self.emit_collab_event(
            parent_thread_id,
            EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                call_id: call_id.clone(),
                sender_thread_id: parent_thread_id,
                prompt: prompt.clone(),
                model: None,
            }),
        )
        .await;

        match self
            .spawn_agent_with_prompt_kind_inline_wait(
                parent_thread_id,
                prompt.clone(),
                system_prompt_kind,
            )
            .await
        {
            Ok((metadata, waiter)) => {
                let thread_id = metadata
                    .agent_id
                    .ok_or_else(|| CoreError::invariant("spawned agent is missing thread id"))?;
                let status = self.get_status(thread_id);
                self.emit_collab_event(
                    parent_thread_id,
                    EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                        call_id,
                        sender_thread_id: parent_thread_id,
                        new_thread_id: Some(thread_id),
                        new_agent_nickname: metadata.agent_nickname.clone(),
                        prompt,
                        model: None,
                        status: status.clone(),
                    }),
                )
                .await;
                Ok((metadata, status, waiter))
            }
            Err(err) => {
                self.emit_collab_event(
                    parent_thread_id,
                    EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                        call_id,
                        sender_thread_id: parent_thread_id,
                        new_thread_id: None,
                        new_agent_nickname: None,
                        prompt,
                        model: None,
                        status: AgentStatus::Errored(err.to_error_info()),
                    }),
                )
                .await;
                Err(err)
            }
        }
    }

    async fn spawn_agent_internal(
        &self,
        parent_thread_id: ThreadId,
        prompt: String,
        system_prompt_kind: SystemPromptKind,
        inline_wait: bool,
    ) -> CoreResult<(
        AgentMetadata,
        ThreadId,
        Option<InlineChildCompletionReceiver>,
    )> {
        let runtime = self.runtime()?;
        let reservation = self
            .state
            .registry
            .reserve_spawn_slot(parent_thread_id, AGENT_MAX_DEPTH, AGENT_MAX_THREADS)
            .map_err(CoreError::registry)?;

        let child_thread_id = ThreadId::new();
        let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: reservation.depth(),
            agent_path: Some(reservation.agent_path().clone()),
            agent_nickname: Some(reservation.agent_path().name().to_string()),
        });
        let ask_user_client = runtime.ask_user_client.clone();
        let initial_history = Vec::new();
        let child_thread = Arc::new(
            CoreThread::new_with_history(
                child_thread_id,
                ask_user_client,
                runtime.model_factory.clone(),
                child_source,
                system_prompt_kind,
                None,
                self.clone(),
                initial_history,
            )
            .await?,
        );

        {
            let mut threads = runtime.threads.write().await;
            threads.insert(child_thread_id, Arc::clone(&child_thread));
        }
        self.ensure_status_sender(child_thread_id, AgentStatus::PendingInit)?;
        let inline_waiter = if inline_wait {
            Some(self.register_inline_child_completion_waiter(child_thread_id)?)
        } else {
            None
        };

        if let Err(err) = child_thread.submit(Op::UserInput(prompt)).await {
            runtime.threads.write().await.remove(&child_thread_id);
            self.remove_status_sender(child_thread_id)?;
            if inline_wait {
                self.unregister_inline_child_completion_waiter(child_thread_id)?;
            }
            return Err(err);
        }

        let agent_path = reservation.agent_path().clone();
        let agent_nickname = reservation.agent_path().name().to_string();
        let depth = reservation.depth();
        let metadata = reservation
            .commit(AgentMetadata {
                agent_id: Some(child_thread_id),
                agent_path,
                agent_nickname: Some(agent_nickname),
                system_prompt_kind,
                parent_thread_id: Some(parent_thread_id),
                depth,
            })
            .map_err(CoreError::registry)?;
        runtime
            .state_db
            .upsert_thread(
                &child_thread_id.to_string(),
                Some(metadata.agent_path.as_str()),
                metadata.agent_nickname.as_deref(),
                Some(metadata.system_prompt_kind.storage_key()),
            )
            .await?;
        runtime
            .state_db
            .upsert_open_edge(&parent_thread_id.to_string(), &child_thread_id.to_string())
            .await?;
        self.maybe_start_completion_watcher(metadata.clone(), true);
        Ok((metadata, child_thread_id, inline_waiter))
    }

    pub(crate) async fn close_agent(
        &self,
        author_thread_id: ThreadId,
        target: &str,
    ) -> CoreResult<AgentStatus> {
        let target_thread_id = self.resolve_agent_reference(author_thread_id, target)?;
        let runtime = self.runtime()?;
        if let Some(metadata) = self
            .state
            .registry
            .agent_metadata_for_thread(target_thread_id)
            && let Some(parent_thread_id) = metadata.parent_thread_id
        {
            runtime
                .state_db
                .close_edge(&parent_thread_id.to_string(), &target_thread_id.to_string())
                .await?;
        }
        let threads = runtime.threads.read().await;
        let thread = threads
            .get(&target_thread_id)
            .cloned()
            .ok_or(CoreError::UnknownThread {
                thread_id: target_thread_id,
            })?;
        drop(threads);

        let _ = thread.submit(Op::Shutdown).await?;
        thread.core.session.abort_all_tasks("closed").await;
        runtime.threads.write().await.remove(&target_thread_id);
        self.state.registry.unregister_thread(target_thread_id);
        self.remove_status_sender(target_thread_id)?;
        Ok(AgentStatus::Shutdown)
    }

    fn ensure_status_sender(&self, thread_id: ThreadId, status: AgentStatus) -> CoreResult<()> {
        lock_mutex(&self.state.statuses, "agent_control.statuses")?
            .entry(thread_id)
            .or_insert_with(|| watch::channel(status).0);
        Ok(())
    }

    fn remove_status_sender(&self, thread_id: ThreadId) -> CoreResult<()> {
        lock_mutex(&self.state.statuses, "agent_control.statuses")?.remove(&thread_id);
        Ok(())
    }

    pub(crate) fn register_inline_child_completion_waiter(
        &self,
        child_thread_id: ThreadId,
    ) -> CoreResult<InlineChildCompletionReceiver> {
        let (tx, rx) = oneshot::channel();
        lock_mutex(&self.state.inline_waiters, "agent_control.inline_waiters")?
            .insert(child_thread_id, tx);
        Ok(rx)
    }

    pub(crate) fn unregister_inline_child_completion_waiter(
        &self,
        child_thread_id: ThreadId,
    ) -> CoreResult<()> {
        lock_mutex(&self.state.inline_waiters, "agent_control.inline_waiters")?
            .remove(&child_thread_id);
        Ok(())
    }

    fn session_source_for_thread(&self, thread_id: ThreadId) -> CoreResult<SessionSource> {
        let Some(metadata) = self.state.registry.agent_metadata_for_thread(thread_id) else {
            return Err(CoreError::UnknownThread { thread_id });
        };
        if metadata.parent_thread_id.is_none() || metadata.depth == 0 {
            return Ok(SessionSource::Cli);
        }
        Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: metadata.parent_thread_id.ok_or_else(|| {
                CoreError::invariant(format!("missing parent thread id for {thread_id}"))
            })?,
            depth: metadata.depth,
            agent_path: Some(metadata.agent_path),
            agent_nickname: metadata.agent_nickname,
        }))
    }

    fn runtime(&self) -> CoreResult<AgentControlRuntime> {
        lock_mutex(&self.state.runtime, "agent_control.runtime")?
            .clone()
            .ok_or(CoreError::RuntimeNotAttached)
    }

    fn maybe_start_completion_watcher(&self, child: AgentMetadata, notify_if_already_final: bool) {
        let Some(parent_thread_id) = child.parent_thread_id else {
            return;
        };
        let Some(child_thread_id) = child.agent_id else {
            return;
        };
        let Ok(mut status_rx) = self.subscribe_status(child_thread_id) else {
            return;
        };
        let control = self.clone();
        tokio::spawn(async move {
            let mut first_poll = true;
            loop {
                let status = status_rx.borrow().clone();
                if is_final(&status) {
                    if !first_poll || notify_if_already_final {
                        control
                            .handle_child_completion(
                                parent_thread_id,
                                child_thread_id,
                                &child,
                                status,
                            )
                            .await;
                    }
                    break;
                }
                first_poll = false;
                if status_rx.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    pub(crate) async fn emit_collab_event(&self, author_thread_id: ThreadId, msg: EventMsg) {
        let Ok(runtime) = self.runtime() else {
            return;
        };
        let threads = runtime.threads.read().await;
        let Some(thread) = threads.get(&author_thread_id).cloned() else {
            return;
        };
        drop(threads);
        thread.core.emit_session_event(msg).await;
    }

    async fn handle_child_completion(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        child: &AgentMetadata,
        status: AgentStatus,
    ) {
        if !should_notify_parent_on_completion(&status) {
            return;
        }

        self.emit_collab_event(
            parent_thread_id,
            EventMsg::CollabAgentCompleted(CollabAgentCompletedEvent {
                parent_thread_id,
                child_thread_id,
                agent_path: child.agent_path.clone(),
                agent_nickname: child.agent_nickname.clone(),
                last_assistant_message: last_assistant_message(&status),
                status: status.clone(),
            }),
        )
        .await;

        let last_assistant_message = last_assistant_message(&status);
        if let Ok(mut waiters) = self.state.inline_waiters.lock()
            && let Some(waiter) = waiters.remove(&child_thread_id)
        {
            let _ = waiter.send(InlineChildCompletion {
                status,
                last_assistant_message,
            });
        }
    }
}

fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> CoreResult<MutexGuard<'a, T>> {
    mutex.lock().map_err(|_| CoreError::MutexPoisoned { name })
}

fn should_notify_parent_on_completion(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_)
            | AgentStatus::Interrupted
            | AgentStatus::Errored(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, Mutex, MutexGuard},
    };

    use anyhow::{Context, Result, anyhow};
    use futures_util::{StreamExt, stream};
    use rig::message::{Message, Text};
    use smooth_state_db::StateDbHandle;
    use tempfile::TempDir;
    use tokio::sync::{RwLock, Semaphore};

    use super::AgentControl;
    use crate::{
        SessionCompletionEvent, SessionCompletionStream, SessionModel, SessionModelDriver,
        SessionModelFactory, SessionTurnSummary, agent::SystemPromptKind,
        thread_manager::ThreadManagerState,
    };
    use smooth_protocol::{AgentStatus, EventMsg, ThreadId};
    use tools::AskUserClient;

    fn lock_test_mutex<'a, T>(
        mutex: &'a Mutex<T>,
        name: &'static str,
    ) -> Result<MutexGuard<'a, T>> {
        mutex
            .lock()
            .map_err(|_| anyhow!("test mutex `{name}` was poisoned"))
    }

    #[test]
    fn clones_share_registry_and_status_state() -> Result<()> {
        let control = AgentControl::new();
        let clone = control.clone();
        let root_id = ThreadId::new();

        control.register_session_root(root_id)?;
        clone.set_status(root_id, AgentStatus::Running)?;

        assert_eq!(control.get_status(root_id), AgentStatus::Running);
        assert_eq!(clone.registry().live_agents().len(), 1);
        Ok(())
    }

    struct StubDriver {
        text: String,
    }

    impl SessionModelDriver for StubDriver {
        fn stream_completion_turn(
            &self,
            prompt: Message,
            history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            let _ = (prompt, history);
            let text = self.text.clone();
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    crate::SessionAssistantContent::Text(Text {
                        text: self.text.clone(),
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-stub".to_string()),
                    response: text,
                })),
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
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            let _ = thread_id;
            Ok(self.model.clone())
        }
    }

    #[derive(Default)]
    struct RecordingState {
        calls: Mutex<HashMap<ThreadId, Vec<Vec<Message>>>>,
    }

    #[derive(Default)]
    struct PromptKindState {
        calls: Mutex<Vec<(ThreadId, SystemPromptKind)>>,
    }

    struct RecordingDriver {
        thread_id: ThreadId,
        state: Arc<RecordingState>,
        text: String,
    }

    impl SessionModelDriver for RecordingDriver {
        fn stream_completion_turn(
            &self,
            prompt: Message,
            history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            lock_test_mutex(&self.state.calls, "calls")?
                .entry(self.thread_id)
                .or_default()
                .push(history.clone());
            let _ = prompt;
            let text = self.text.clone();
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    crate::SessionAssistantContent::Text(Text {
                        text: self.text.clone(),
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-recording".to_string()),
                    response: text,
                })),
            ])))
        }
    }

    struct RecordingFactory {
        state: Arc<RecordingState>,
    }

    impl SessionModelFactory for RecordingFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            thread_id: ThreadId,
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            Ok(SessionModel::Stub(Arc::new(RecordingDriver {
                thread_id,
                state: Arc::clone(&self.state),
                text: "recorded".to_string(),
            })))
        }
    }

    struct PromptKindFactory {
        state: Arc<PromptKindState>,
    }

    impl SessionModelFactory for PromptKindFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            thread_id: ThreadId,
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            lock_test_mutex(&self.state.calls, "prompt_kind_calls")?
                .push((thread_id, system_prompt_kind));
            Ok(SessionModel::Stub(Arc::new(StubDriver {
                text: "prompt-kind".to_string(),
            })))
        }
    }

    struct BlockingRootDriver {
        release: Arc<Semaphore>,
        text: String,
    }

    impl SessionModelDriver for BlockingRootDriver {
        fn stream_completion_turn(
            &self,
            prompt: Message,
            history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            let _ = (prompt, history);
            let release = Arc::clone(&self.release);
            let text = self.text.clone();
            let completed_text = text.clone();
            Ok(Box::pin(
                stream::once(async move {
                    let _permit = release
                        .acquire_owned()
                        .await
                        .map_err(|err| anyhow!("release permit: {err}"))?;
                    Ok(SessionCompletionEvent::AssistantItem(
                        crate::SessionAssistantContent::Text(Text { text }),
                    ))
                })
                .chain(stream::iter(vec![Ok(SessionCompletionEvent::Completed(
                    SessionTurnSummary {
                        assistant_message_id: Some("assistant-blocking".to_string()),
                        response: completed_text,
                    },
                ))])),
            ))
        }
    }

    #[tokio::test]
    async fn spawn_agent_creates_child_and_tracks_it_live() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "child".into(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;

        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await?;

        assert!(child.agent_path.as_str().starts_with("/root/"));
        assert_eq!(control.registry().live_agents().len(), 2);
        let state_db = StateDbHandle::open(workspace.path().join(".smooth-code/state.db")).await?;
        let root_row = state_db
            .get_thread(&root_id.to_string())
            .await?
            .context("root row")?;
        assert_eq!(root_row.agent_path, None);
        let child_id = child.agent_id.context("child id")?;
        let child_row = state_db
            .get_thread(&child_id.to_string())
            .await?
            .context("child row")?;
        assert_eq!(
            child_row.agent_path.as_deref(),
            Some(child.agent_path.as_str())
        );
        assert_eq!(
            state_db
                .list_open_children(&root_id.to_string())
                .await?
                .len(),
            1
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn spawn_agent_uses_default_subagent_prompt_kind() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let prompt_state = Arc::new(PromptKindState::default());
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(PromptKindFactory {
                state: Arc::clone(&prompt_state),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;

        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await?;
        let child_id = child.agent_id.context("child id")?;

        let calls = lock_test_mutex(&prompt_state.calls, "prompt_kind_calls")?;
        assert!(calls.contains(&(root_id, SystemPromptKind::Root)));
        assert!(calls.contains(&(child_id, SystemPromptKind::DefaultSubagent)));

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn explore_spawn_uses_explorer_prompt_kind() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let prompt_state = Arc::new(PromptKindState::default());
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(PromptKindFactory {
                state: Arc::clone(&prompt_state),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;

        let control = manager.agent_control();
        let child = control
            .spawn_agent_with_prompt_kind(
                root_id,
                "inspect only".to_string(),
                SystemPromptKind::Explore,
            )
            .await?;
        let child_id = child.agent_id.context("child id")?;

        let calls = lock_test_mutex(&prompt_state.calls, "prompt_kind_calls")?;
        assert!(calls.contains(&(child_id, SystemPromptKind::Explore)));

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn list_agents_resolves_relative_agent_target() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await?;
        let child_id = child.agent_id.context("child id")?;
        let child_name = child.agent_path.name().to_string();

        let Err(listed) = control.list_agents(child_id, Some("..")) else {
            panic!("relative parent traversal should be rejected");
        };
        assert!(listed.to_string().contains("`..` is reserved"));
        let listed = control.list_agents(root_id, Some(&child_name))?;
        assert_eq!(listed.len(), 1);

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn close_agent_removes_live_child() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await?;

        let status = control
            .close_agent(root_id, child.agent_path.as_str())
            .await?;
        assert_eq!(status, AgentStatus::Shutdown);
        assert_eq!(control.registry().live_agents().len(), 1);
        let state_db = StateDbHandle::open(workspace.path().join(".smooth-code/state.db")).await?;
        assert!(
            state_db
                .list_open_children(&root_id.to_string())
                .await?
                .is_empty()
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn completion_watcher_emits_parent_event_for_shutdown_child() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let release = Arc::new(Semaphore::new(0));
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(BlockingRootDriver {
                    release: Arc::clone(&release),
                    text: "response".into(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await?;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await?;
        let child_id = child.agent_id.context("child id")?;

        let status = control
            .close_agent(root_id, child.agent_path.as_str())
            .await?;
        assert_eq!(status, AgentStatus::Shutdown);

        let mut saw_shutdown_completion = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = match tokio::time::timeout(remaining, root_events.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(err)) => panic!("root event channel closed: {err}"),
                Err(_) => break,
            };

            if let EventMsg::CollabAgentCompleted(completion) = event.msg {
                assert_eq!(completion.parent_thread_id, root_id);
                assert_eq!(completion.child_thread_id, child_id);
                assert_eq!(completion.status, AgentStatus::Shutdown);
                assert_eq!(completion.last_assistant_message, None);
                saw_shutdown_completion = true;
                break;
            }
        }

        assert!(
            saw_shutdown_completion,
            "expected shutdown child completion event on parent thread"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn completion_watcher_emits_parent_completion_event() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await?;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await?;
        let child_id = child.agent_id.context("child id")?;
        let mut saw_completion = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = match tokio::time::timeout(remaining, root_events.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(err)) => panic!("root event channel closed: {err}"),
                Err(_) => break,
            };

            match event.msg {
                EventMsg::CollabAgentCompleted(completion) => {
                    assert_eq!(completion.parent_thread_id, root_id);
                    assert_eq!(completion.child_thread_id, child_id);
                    assert_eq!(completion.agent_path, child.agent_path);
                    assert_eq!(completion.agent_nickname, child.agent_nickname);
                    assert_eq!(
                        completion.status,
                        AgentStatus::Completed(Some("response".to_string()))
                    );
                    assert_eq!(
                        completion.last_assistant_message.as_deref(),
                        Some("response")
                    );
                    saw_completion = true;
                }
                _ => {}
            }

            if saw_completion {
                break;
            }
        }

        assert!(
            saw_completion,
            "expected completion watcher to emit collab event"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn completion_watcher_resolves_inline_waiter() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await?;
        let control = manager.agent_control();
        let (child, waiter) = control
            .spawn_agent_with_prompt_kind_inline_wait(
                root_id,
                "hello child".to_string(),
                SystemPromptKind::DefaultSubagent,
            )
            .await?;
        let child_id = child.agent_id.context("child id")?;
        let completion = waiter.await?;

        assert_eq!(
            completion.status,
            AgentStatus::Completed(Some("response".to_string()))
        );
        assert_eq!(
            completion.last_assistant_message.as_deref(),
            Some("response")
        );

        let mut saw_collab_completion = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = match tokio::time::timeout(remaining, root_events.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(err)) => panic!("root event channel closed: {err}"),
                Err(_) => break,
            };

            match event.msg {
                EventMsg::CollabAgentCompleted(completion) => {
                    assert_eq!(completion.child_thread_id, child_id);
                    saw_collab_completion = true;
                }
                _ => {}
            }

            if saw_collab_completion {
                break;
            }
        }

        assert!(
            saw_collab_completion,
            "expected child completion event on parent thread"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn spawn_agent_starts_child_with_empty_history() -> Result<()> {
        let _cwd_guard = crate::test_support::cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let recording_state = Arc::new(RecordingState::default());
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(RecordingFactory {
                state: Arc::clone(&recording_state),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        manager
            .start_user_input(root_id, "parent asks".to_string())
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let child_id = child.agent_id.context("child id")?;
        let calls = lock_test_mutex(&recording_state.calls, "calls")?;
        let child_history = calls
            .get(&child_id)
            .and_then(|calls| calls.first())
            .context("child first call history")?;
        assert!(
            child_history.is_empty(),
            "spawned child should not inherit parent history"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }
}
