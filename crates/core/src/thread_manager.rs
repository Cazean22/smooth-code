use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use cazean_config::Config;
use cazean_protocol::{
    AgentPath, AgentStatus, CollabAgentStatusEntry, CollabResumeBeginEvent, CollabResumeEndEvent,
    ErrorInfo, Event, EventMsg, Op, ProjectInstructions, SessionSource, SubAgentSource, ThreadId,
};
use cazean_state_db::StateDbHandle;
use tokio::sync::{Mutex, RwLock, broadcast};
use tools::AskUserClient;
use uuid::Uuid;

use crate::{
    ThreadSummary,
    agent::{
        AgentControl, SystemPromptKind,
        registry::AgentMetadata,
        status::{agent_status_from_event, last_assistant_message},
    },
    core_thread::CoreThread,
    error::{CoreError, CoreResult},
    provider::{ConfigSessionModelFactory, SessionModelFactory},
    rollout::{
        RecoveryMode, ResumeState, collect_rollout_path_map, find_thread_path, list_threads,
        load_resume_state, load_state, workspace_root,
    },
};

pub struct StartedThread {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
}

pub struct ResumedThread {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
    pub initial_messages: Vec<cazean_protocol::EventMsg>,
}

/// Read-only snapshot of a thread's transcript, taken without registering,
/// resuming, or otherwise mutating the thread (unlike `resume_thread`).
pub struct PreviewedThread {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
    pub initial_messages: Vec<cazean_protocol::EventMsg>,
    pub agent_path: Option<String>,
    pub agent_nickname: Option<String>,
    pub status: AgentStatus,
    pub is_live: bool,
}

/// A memoized `load_state` result for a non-live thread's rollout, validated by
/// the file's `len`/`mtime` so an appended-to (e.g. later resumed) rollout is
/// re-read. Previews are re-requested on every picker selection move, so this
/// keeps arrowing through a large old session from re-parsing it each time.
struct CachedPreview {
    len: u64,
    mtime: Option<SystemTime>,
    state: Arc<ResumeState>,
}

pub struct ThreadManagerState {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
    ask_user_client: Option<AskUserClient>,
    model_factory: Option<Arc<dyn SessionModelFactory>>,
    agent_control: AgentControl,
    state_db: StateDbHandle,
    preview_cache: Arc<Mutex<HashMap<PathBuf, CachedPreview>>>,
}

impl ThreadManagerState {
    /// Convenience constructor using built-in default configuration. Used by
    /// tests and any caller that does not thread a resolved [`Config`].
    pub async fn new(
        ask_user_client: Option<AskUserClient>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
    ) -> CoreResult<Self> {
        Self::new_with_config(ask_user_client, model_factory, Arc::new(Config::default())).await
    }

    pub async fn new_with_config(
        ask_user_client: Option<AskUserClient>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        config: Arc<Config>,
    ) -> CoreResult<Self> {
        let threads = Arc::new(RwLock::new(HashMap::new()));
        let workspace_root = workspace_root().map_err(CoreError::rollout)?;
        let state_db = StateDbHandle::open(workspace_root.join(".cazean").join("state.db")).await?;
        // Resolve the factory exactly once so every thread — including spawned
        // subagents, which inherit this through `AgentControl` — uses the same
        // configured factory rather than falling back to a default.
        let model_factory: Arc<dyn SessionModelFactory> = model_factory
            .unwrap_or_else(|| Arc::new(ConfigSessionModelFactory::new(Arc::clone(&config))));
        let agent_control = AgentControl::with_limits(
            config.agent.max_depth,
            config.agent.max_threads,
            config.tools.max_skill_bytes,
        );
        agent_control.attach_runtime(
            Arc::clone(&threads),
            ask_user_client.clone(),
            Some(Arc::clone(&model_factory)),
            state_db.clone(),
        )?;
        Ok(Self {
            threads,
            ask_user_client,
            model_factory: Some(model_factory),
            agent_control,
            state_db,
            preview_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[tracing::instrument(name = "core.thread_manager.start_thread", skip(self))]
    pub async fn start_thread(&self) -> CoreResult<StartedThread> {
        self.start_thread_with_project_instructions(None).await
    }

    #[tracing::instrument(
        name = "core.thread_manager.start_thread_with_project_instructions",
        skip(self, project_instructions)
    )]
    pub async fn start_thread_with_project_instructions(
        &self,
        project_instructions: Option<ProjectInstructions>,
    ) -> CoreResult<StartedThread> {
        let thread_id = ThreadId::new();
        let thread = Arc::new(
            CoreThread::new(
                thread_id,
                self.ask_user_client.clone(),
                self.model_factory.clone(),
                SessionSource::Cli,
                SystemPromptKind::Root,
                project_instructions,
                self.agent_control.clone(),
            )
            .await?,
        );
        let rollout_path = thread.rollout_path().clone();

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        self.agent_control.register_session_root(thread_id)?;
        self.state_db
            .upsert_thread(
                &thread_id.to_string(),
                None,
                None,
                Some(SystemPromptKind::Root.storage_key()),
            )
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
        // One directory walk resolves both this thread and its whole child
        // subtree below, instead of re-scanning the sessions tree per thread.
        let rollout_paths = collect_rollout_path_map(&workspace_root)
            .await
            .map_err(CoreError::rollout)?;
        let rollout_path = rollout_paths
            .get(&thread_id)
            .cloned()
            .ok_or(CoreError::UnknownThread { thread_id })?;
        let resume_state = load_resume_state(&rollout_path)
            .await
            .map_err(CoreError::rollout)?;
        let mut initial_messages = resume_state.initial_messages.clone();
        let thread = Arc::new(
            CoreThread::resume(
                rollout_path.clone(),
                resume_state,
                self.ask_user_client.clone(),
                self.model_factory.clone(),
                SessionSource::Cli,
                SystemPromptKind::Root,
                self.agent_control.clone(),
            )
            .await?,
        );

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        self.agent_control.register_session_root(thread_id)?;
        drop(threads);
        self.state_db
            .upsert_thread(
                &thread_id.to_string(),
                None,
                None,
                Some(SystemPromptKind::Root.storage_key()),
            )
            .await?;
        initial_messages.extend(self.resume_child_subtree(thread_id, &rollout_paths).await?);
        Ok(ResumedThread {
            thread_id,
            rollout_path,
            initial_messages,
        })
    }

    /// Read-only preview of a thread: snapshot its transcript from the rollout
    /// and report its identity/status, without constructing a `CoreThread`,
    /// registering it as a root, or touching its children (see `resume_thread`
    /// for the mutating path).
    #[tracing::instrument(name = "core.thread_manager.preview_thread", skip(self), fields(thread_id = %thread_id))]
    pub async fn preview_thread(&self, thread_id: ThreadId) -> CoreResult<PreviewedThread> {
        let live_thread = self.threads.read().await.get(&thread_id).cloned();
        let is_live = live_thread.is_some();
        let rollout_path = match &live_thread {
            Some(thread) => thread.rollout_path().clone(),
            None => {
                let workspace_root = workspace_root().map_err(CoreError::rollout)?;
                find_thread_path(&workspace_root, thread_id)
                    .await
                    .map_err(CoreError::rollout)?
            }
        };
        // A live thread's rollout is still being appended to, so it is read
        // fresh (and as `PreviewLive`); a finished thread's rollout is immutable
        // for this session, so its parsed state is memoized — the picker
        // re-requests a preview on every selection move.
        let state: Arc<ResumeState> = if is_live {
            Arc::new(
                load_state(&rollout_path, RecoveryMode::PreviewLive)
                    .await
                    .map_err(CoreError::rollout)?,
            )
        } else {
            self.load_preview_state_cached(&rollout_path).await?
        };

        let derived_status = || {
            state
                .initial_messages
                .iter()
                .filter_map(agent_status_from_event)
                .next_back()
                .unwrap_or(AgentStatus::PendingInit)
        };
        let status = if is_live {
            match self.agent_control.get_status(thread_id) {
                // Roots have no agent status entry; fold the replayed events
                // chronologically (a `TurnStarted` means running, any later
                // status-bearing event — completion, interruption, error —
                // supersedes it).
                AgentStatus::NotFound => folded_live_status(&state.initial_messages),
                status => status,
            }
        } else {
            derived_status()
        };

        // Identity metadata is best-effort: older rollouts and plain roots may
        // have no state-db row, and the rollout alone is enough to preview.
        let (agent_path, agent_nickname) =
            match self.state_db.get_thread(&thread_id.to_string()).await {
                Ok(Some(row)) => (row.agent_path, row.agent_nickname),
                Ok(None) | Err(_) => (None, None),
            };

        Ok(PreviewedThread {
            thread_id,
            rollout_path,
            initial_messages: state.initial_messages.clone(),
            agent_path,
            agent_nickname,
            status,
            is_live,
        })
    }

    /// Load a non-live thread's `ResumeState` for preview, served from
    /// [`Self::preview_cache`] when the rollout's `len`/`mtime` are unchanged.
    /// A finished rollout is immutable for the session, so this turns repeated
    /// previews (picker navigation) into one parse instead of one per view.
    async fn load_preview_state_cached(&self, rollout_path: &Path) -> CoreResult<Arc<ResumeState>> {
        let (len, mtime) = match tokio::fs::metadata(rollout_path).await {
            Ok(meta) => (meta.len(), meta.modified().ok()),
            Err(_) => (0, None),
        };
        {
            let cache = self.preview_cache.lock().await;
            if let Some(entry) = cache.get(rollout_path)
                && entry.len == len
                && entry.mtime == mtime
            {
                return Ok(Arc::clone(&entry.state));
            }
        }
        let state = Arc::new(
            load_state(rollout_path, RecoveryMode::Resume)
                .await
                .map_err(CoreError::rollout)?,
        );
        let mut cache = self.preview_cache.lock().await;
        cache.insert(
            rollout_path.to_path_buf(),
            CachedPreview {
                len,
                mtime,
                state: Arc::clone(&state),
            },
        );
        Ok(state)
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

    pub async fn cancel_turn_subtree(&self, thread_id: ThreadId) -> CoreResult<Vec<ThreadId>> {
        let thread = self.get(thread_id).await?;
        // `interrupt_turn_cascade` aborts the root's turn and every live
        // descendant's — the same path `Op::Interrupt` takes — and reports
        // which threads actually had a turn interrupted.
        Ok(thread.core.interrupt_turn_cascade().await)
    }

    /// Shut every thread down gracefully: each root thread gets
    /// `Op::Shutdown`, which cancels its running turn (killing tool
    /// subprocesses via the cancellation chain) and cascades to its live
    /// descendants, draining inline under the shutdown grace. The thread map
    /// is cleared afterwards so nothing restarts.
    #[tracing::instrument(name = "core.thread_manager.shutdown_all", skip(self))]
    pub async fn shutdown_all(&self) -> CoreResult<()> {
        let thread_ids = self
            .threads
            .read()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let registry = self.agent_control.registry();
        for thread_id in thread_ids {
            let is_root = registry
                .agent_metadata_for_thread(thread_id)
                .is_none_or(|metadata| metadata.parent_thread_id.is_none());
            if !is_root {
                // Descendants are shut down by their root's cascade.
                continue;
            }
            if let Ok(thread) = self.get(thread_id).await {
                let _ = thread.submit(Op::Shutdown).await;
            }
        }
        self.threads.write().await.clear();
        // Fire any kill sweeps still inside their SIGTERM grace: shutdown
        // finishes faster than that grace by design, and the process is about
        // to exit — a sweep that has not run yet would otherwise be lost with
        // it, orphaning SIGTERM-ignoring subprocesses.
        tools::sweep_pending_process_kills();
        Ok(())
    }

    /// Toggle plan mode for the given thread. Returns the new effective state.
    pub async fn set_plan_mode(&self, thread_id: ThreadId, enabled: bool) -> CoreResult<bool> {
        let thread = self.get(thread_id).await?;
        thread.set_plan_mode(enabled).await
    }

    /// Current plan-mode state of the given thread.
    pub async fn plan_mode(&self, thread_id: ThreadId) -> CoreResult<bool> {
        let thread = self.get(thread_id).await?;
        Ok(thread.plan_mode())
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

    pub async fn spawn_agent(
        &self,
        parent_thread_id: ThreadId,
        prompt: String,
    ) -> CoreResult<CollabAgentStatusEntry> {
        let metadata = self
            .agent_control
            .spawn_agent_with_prompt_kind(
                parent_thread_id,
                prompt,
                SystemPromptKind::DefaultSubagent,
            )
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
        reason: crate::core::CancelReason,
    ) -> CoreResult<()> {
        if let Ok(thread) = self.get(thread_id).await {
            let _ = thread.submit(Op::Shutdown).await;
            thread.core.session.abort_and_drain_all_tasks(reason).await;
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

    async fn resume_child_subtree(
        &self,
        root_thread_id: ThreadId,
        rollout_paths: &HashMap<ThreadId, PathBuf>,
    ) -> CoreResult<Vec<EventMsg>> {
        // BFS over the persisted open-edge subtree. Each entry carries whether
        // the subtree is being reaped. A child that *completed* is reaped — its
        // edge is closed and it is not rehydrated, since its result already
        // lives in the parent's history (or was consumed) and a one-shot
        // sub-agent cannot be re-driven. Every descendant of a reaped child is
        // reaped too. Children that ended abnormally (interrupted/errored) are
        // still rehydrated for visibility. This keeps the many completed/
        // consumed children an interrupted long turn can leave behind from
        // piling up on resume, while still restoring the incomplete frontier.
        //
        // (Every persisted status is terminal — `AgentStatusChanged(Running)`
        // is not persisted and an unfinished turn is reconstructed as
        // `Interrupted` — so the reap/rehydrate split keys on `Completed`
        // specifically, not on `is_final`.)
        let mut queue: VecDeque<(ThreadId, bool)> = VecDeque::from([(root_thread_id, false)]);
        let mut events = Vec::new();

        while let Some((parent_thread_id, reap_subtree)) = queue.pop_front() {
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
                            status: AgentStatus::Errored(
                                ErrorInfo::new(
                                    "missing_thread_metadata",
                                    "missing thread metadata",
                                )
                                .with_source("cazean-core"),
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

                // Decide reap vs rehydrate. A reaped ancestor forces reaping the
                // whole subtree; otherwise only a *completed* child is reaped (a
                // finished/consumed child whose result is already in the parent's
                // history). Abnormally-ended children are rehydrated. If the
                // status can't be determined (e.g. a missing rollout), fall
                // through to the rehydrate attempt, which surfaces the error and
                // leaves the edge open, exactly as before.
                let reap_status = if reap_subtree {
                    Some(
                        self.peek_child_status(rollout_paths, child_thread_id)
                            .await
                            .unwrap_or(AgentStatus::Shutdown),
                    )
                } else {
                    match self.peek_child_status(rollout_paths, child_thread_id).await {
                        Ok(status @ AgentStatus::Completed(_)) => Some(status),
                        _ => None,
                    }
                };

                events.push(EventMsg::CollabResumeBegin(CollabResumeBeginEvent {
                    call_id: call_id.clone(),
                    sender_thread_id: parent_thread_id,
                    receiver_thread_id: child_thread_id,
                    receiver_agent_nickname: thread_row.agent_nickname.clone(),
                }));

                if let Some(status) = reap_status {
                    if let Err(err) = self
                        .state_db
                        .close_edge(&parent_thread_id.to_string(), &child_thread_id.to_string())
                        .await
                    {
                        tracing::warn!(
                            parent_thread_id = %parent_thread_id,
                            child_thread_id = %child_thread_id,
                            error = %err,
                            "failed to close edge while reaping finished child on resume"
                        );
                    }
                    // Recurse to reap the finished child's own open descendants.
                    queue.push_back((child_thread_id, true));
                    events.push(EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                        call_id,
                        sender_thread_id: parent_thread_id,
                        receiver_thread_id: child_thread_id,
                        receiver_agent_nickname: thread_row.agent_nickname,
                        status,
                    }));
                    continue;
                }

                let result = self
                    .resume_child_thread(
                        rollout_paths,
                        parent_thread_id,
                        child_thread_id,
                        thread_row.agent_path.as_deref(),
                        thread_row.agent_nickname.clone(),
                        thread_row.prompt_kind.as_deref(),
                    )
                    .await;
                match result {
                    Ok(()) => {
                        queue.push_back((child_thread_id, false));
                        events.push(EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                            call_id,
                            sender_thread_id: parent_thread_id,
                            receiver_thread_id: child_thread_id,
                            receiver_agent_nickname: thread_row.agent_nickname,
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
                            status: AgentStatus::Errored(err.to_error_info()),
                        }));
                    }
                }
            }
        }

        Ok(events)
    }

    /// Read a persisted child's last known status without rehydrating it, used
    /// to decide whether to reap or rehydrate it on resume. Returns the live
    /// status if the thread is already loaded, otherwise the last status event
    /// from its rollout (or `PendingInit` if none was recorded).
    async fn peek_child_status(
        &self,
        rollout_paths: &HashMap<ThreadId, PathBuf>,
        child_thread_id: ThreadId,
    ) -> CoreResult<AgentStatus> {
        if self.threads.read().await.contains_key(&child_thread_id) {
            return Ok(self.agent_control.get_status(child_thread_id));
        }
        let rollout_path = rollout_paths
            .get(&child_thread_id)
            .ok_or(CoreError::UnknownThread {
                thread_id: child_thread_id,
            })?;
        let resume_state = load_resume_state(rollout_path)
            .await
            .map_err(CoreError::rollout)?;
        Ok(resume_state
            .initial_messages
            .iter()
            .filter_map(agent_status_from_event)
            .next_back()
            .unwrap_or(AgentStatus::PendingInit))
    }

    async fn resume_child_thread(
        &self,
        rollout_paths: &HashMap<ThreadId, PathBuf>,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        agent_path: Option<&str>,
        agent_nickname: Option<String>,
        prompt_kind: Option<&str>,
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
        let max_depth = self.agent_control.max_depth();
        if depth > max_depth {
            return Err(CoreError::AgentDepthLimitExceeded { depth, max_depth });
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
        let rollout_path =
            rollout_paths
                .get(&child_thread_id)
                .cloned()
                .ok_or(CoreError::UnknownThread {
                    thread_id: child_thread_id,
                })?;
        let resume_state = load_resume_state(&rollout_path)
            .await
            .map_err(CoreError::rollout)?;
        let initial_messages = resume_state.initial_messages.clone();
        let system_prompt_kind = SystemPromptKind::from_child_storage_key(prompt_kind);
        let thread = Arc::new(
            CoreThread::resume(
                rollout_path,
                resume_state,
                self.ask_user_client.clone(),
                self.model_factory.clone(),
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth,
                    agent_path: Some(agent_path.clone()),
                    agent_nickname: agent_nickname.clone(),
                }),
                system_prompt_kind,
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
                system_prompt_kind,
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
        last_assistant_message: last_assistant_message(&status),
        status,
    })
}

/// Status of a live thread without an agent-control entry (a root), folded
/// chronologically from its replayed events: `TurnStarted` means running,
/// and any later status-bearing event (completion, interruption, error,
/// explicit status change) supersedes it. The fold is order-sensitive on
/// purpose — an earlier turn's completion must not mask a later open turn,
/// and a persisted error inside an open turn must not be reported as running.
fn folded_live_status(events: &[EventMsg]) -> AgentStatus {
    events
        .iter()
        .filter_map(|event| match event {
            EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
            other => agent_status_from_event(other),
        })
        .next_back()
        .unwrap_or(AgentStatus::PendingInit)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        path::{Path, PathBuf},
        sync::{Arc, Mutex, MutexGuard},
    };

    use anyhow::Result;
    use futures_util::stream;
    use rig::message::{AssistantContent, Message, Text, UserContent};
    use tempfile::TempDir;
    use tokio::sync::RwLock;
    use tools::AskUserClient;

    use super::ThreadManagerState;
    use crate::{
        SessionCompletionEvent, SessionCompletionStream, SessionModel, SessionModelDriver,
        SessionModelFactory, SessionTurnSummary,
        agent::{AgentControl, SystemPromptKind, status::is_final},
        rollout::{find_thread_path, load_resume_state},
        test_support::cwd_test_lock,
    };
    use cazean_protocol::{
        AgentStatus, EventMsg, ProjectInstructionEntry, ProjectInstructions, ThreadId,
    };

    fn lock_test_mutex<'a, T>(
        mutex: &'a Mutex<T>,
        name: &'static str,
    ) -> Result<MutexGuard<'a, T>> {
        mutex
            .lock()
            .map_err(|_| anyhow::anyhow!("test mutex `{name}` was poisoned"))
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
                        additional_params: None,
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
            _thread_id: ThreadId,
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            Ok(self.model.clone())
        }
    }

    #[derive(Clone)]
    struct CapturedTurn {
        prompt: Message,
        history: Vec<Message>,
    }

    struct CapturingDriver {
        calls: Arc<Mutex<Vec<CapturedTurn>>>,
    }

    impl SessionModelDriver for CapturingDriver {
        fn stream_completion_turn(
            &self,
            prompt: Message,
            history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            lock_test_mutex(&self.calls, "captured_turns")?.push(CapturedTurn { prompt, history });
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    crate::SessionAssistantContent::Text(Text {
                        text: "captured response".to_string(),
                        additional_params: None,
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-captured".to_string()),
                    response: "captured response".to_string(),
                })),
            ])))
        }
    }

    struct CapturingFactory {
        calls: Arc<Mutex<Vec<CapturedTurn>>>,
    }

    impl SessionModelFactory for CapturingFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            _thread_id: ThreadId,
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            Ok(SessionModel::Stub(Arc::new(CapturingDriver {
                calls: Arc::clone(&self.calls),
            })))
        }
    }

    fn project_instructions_for(root: &Path, text: &str) -> ProjectInstructions {
        ProjectInstructions {
            entries: vec![ProjectInstructionEntry {
                source_path: root.join("AGENTS.md").display().to_string(),
                directory: root.display().to_string(),
                text: text.to_string(),
            }],
        }
    }

    fn user_message_text(message: &Message) -> Option<String> {
        match message {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|content| match content {
                        UserContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            Message::Assistant { .. } | Message::System { .. } => None,
        }
    }

    fn assistant_message_text(message: &Message) -> Option<String> {
        match message {
            Message::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|content| match content {
                        AssistantContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            Message::User { .. } | Message::System { .. } => None,
        }
    }

    #[derive(Default)]
    struct PromptKindState {
        calls: Mutex<Vec<(ThreadId, SystemPromptKind)>>,
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
                text: "done".to_string(),
            })))
        }
    }

    struct PendingDriver;

    impl SessionModelDriver for PendingDriver {
        fn stream_completion_turn(
            &self,
            _prompt: Message,
            _history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            Ok(Box::pin(stream::pending::<Result<SessionCompletionEvent>>()))
        }
    }

    struct PendingFactory;

    impl SessionModelFactory for PendingFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            _thread_id: ThreadId,
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            _plan_mode: bool,
        ) -> Result<SessionModel> {
            Ok(SessionModel::Stub(Arc::new(PendingDriver)))
        }
    }

    async fn wait_for_final_status(
        control: &AgentControl,
        thread_id: ThreadId,
    ) -> Result<AgentStatus> {
        let mut status_rx = control.subscribe_status(thread_id)?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = status_rx.borrow().clone();
                if is_final(&status) {
                    return Ok::<AgentStatus, tokio::sync::watch::error::RecvError>(status);
                }
                status_rx.changed().await?;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for final status for {thread_id}"))?
        .map_err(|err| anyhow::anyhow!("status channel closed for {thread_id}: {err}"))
    }

    #[tokio::test]
    async fn project_instructions_are_contextual_request_history_only() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(CapturingFactory {
                calls: Arc::clone(&calls),
            })),
        )
        .await?;
        let instructions =
            project_instructions_for(workspace.path(), "Original AGENTS instructions");
        let started = manager
            .start_thread_with_project_instructions(Some(instructions))
            .await?;
        let control = manager.agent_control();

        manager
            .start_user_input(started.thread_id, "first prompt".to_string())
            .await?;
        let _ = wait_for_final_status(&control, started.thread_id).await?;
        manager
            .start_user_input(started.thread_id, "second prompt".to_string())
            .await?;
        let _ = wait_for_final_status(&control, started.thread_id).await?;

        {
            let calls = lock_test_mutex(&calls, "captured_turns")?;
            let first = calls
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing first captured turn"))?;
            assert_eq!(first.history.len(), 1);
            assert!(
                user_message_text(&first.history[0])
                    .as_deref()
                    .is_some_and(|text| text.contains("Original AGENTS instructions"))
            );
            assert_eq!(
                user_message_text(&first.prompt).as_deref(),
                Some("first prompt")
            );

            let second = calls
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("missing second captured turn"))?;
            assert_eq!(second.history.len(), 3);
            assert!(
                user_message_text(&second.history[0])
                    .as_deref()
                    .is_some_and(|text| {
                        text.contains("# AGENTS.md instructions for")
                            && text.contains("<INSTRUCTIONS>")
                            && text.contains("Original AGENTS instructions")
                    })
            );
            assert_eq!(
                user_message_text(&second.history[1]).as_deref(),
                Some("first prompt")
            );
            assert_eq!(
                assistant_message_text(&second.history[2]).as_deref(),
                Some("captured response")
            );
            assert_eq!(
                user_message_text(&second.prompt).as_deref(),
                Some("second prompt")
            );
        }

        let state = load_resume_state(&started.rollout_path).await?;
        assert!(
            state
                .history
                .iter()
                .filter_map(user_message_text)
                .all(|text| !text.contains("Original AGENTS instructions"))
        );
        assert!(state.initial_messages.iter().all(|event| {
            !matches!(
                event,
                EventMsg::UserMessage { text }
                    if text.contains("Original AGENTS instructions")
            )
        }));

        let threads = manager.list_threads().await?;
        let summary = threads
            .iter()
            .find(|summary| summary.thread_id == started.thread_id)
            .ok_or_else(|| anyhow::anyhow!("started thread should be listed"))?;
        assert_eq!(summary.last_user_message.as_deref(), Some("second prompt"));

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn skills_are_advertised_request_only_and_slash_input_expands() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let skill_dir = tools::project_skills_dir(workspace.path()).join("deploy");
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Deploy the app\n---\nRun make deploy.",
        )?;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(CapturingFactory {
                calls: Arc::clone(&calls),
            })),
        )
        .await?;
        let started = manager.start_thread_with_project_instructions(None).await?;
        let control = manager.agent_control();

        manager
            .start_user_input(started.thread_id, "/deploy to staging".to_string())
            .await?;
        let _ = wait_for_final_status(&control, started.thread_id).await?;

        {
            let calls = lock_test_mutex(&calls, "captured_turns")?;
            let turn = calls
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing captured turn"))?;
            // The skills listing is a request-only synthetic message ahead of
            // history.
            assert_eq!(turn.history.len(), 1);
            assert!(
                user_message_text(&turn.history[0])
                    .as_deref()
                    .is_some_and(|text| {
                        text.contains("# Available skills")
                            && text.contains("- deploy: Deploy the app")
                    })
            );
            // The slash invocation reaches the model expanded.
            let prompt_text = user_message_text(&turn.prompt)
                .ok_or_else(|| anyhow::anyhow!("prompt should be a user message"))?;
            assert!(prompt_text.contains("<skill-invocation skill=\"deploy\">"));
            assert!(prompt_text.contains("Run make deploy."));
            assert!(prompt_text.ends_with("to staging"));
        }

        let state = load_resume_state(&started.rollout_path).await?;
        // Persisted history holds the expanded text but never the listing.
        assert!(
            state
                .history
                .iter()
                .filter_map(user_message_text)
                .any(|text| text.contains("<skill-invocation skill=\"deploy\">"))
        );
        assert!(
            state
                .history
                .iter()
                .filter_map(user_message_text)
                .all(|text| !text.contains("# Available skills"))
        );
        // The transcript event keeps the raw text the user typed.
        assert!(state.initial_messages.iter().any(|event| {
            matches!(
                event,
                EventMsg::UserMessage { text } if text == "/deploy to staging"
            )
        }));

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn resume_uses_project_instructions_from_rollout_metadata() -> Result<()> {
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
        let started = manager
            .start_thread_with_project_instructions(Some(project_instructions_for(
                workspace.path(),
                "Original metadata instructions",
            )))
            .await?;
        drop(manager);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(CapturingFactory {
                calls: Arc::clone(&calls),
            })),
        )
        .await?;
        let _resumed = resumed_manager.resume_thread(started.thread_id).await?;
        resumed_manager
            .start_user_input(started.thread_id, "after resume".to_string())
            .await?;
        let _ = wait_for_final_status(&resumed_manager.agent_control(), started.thread_id).await?;

        let calls = lock_test_mutex(&calls, "captured_turns")?;
        let turn = calls
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing resumed captured turn"))?;
        assert_eq!(
            user_message_text(&turn.prompt).as_deref(),
            Some("after resume")
        );
        assert!(
            turn.history
                .iter()
                .filter_map(user_message_text)
                .any(|text| text.contains("Original metadata instructions"))
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn cancel_turn_subtree_interrupts_target_and_live_descendants_only() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(None, Some(Arc::new(PendingFactory))).await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        let child_id = child.agent_id.ok_or_else(|| anyhow::anyhow!("child id"))?;
        let sibling = control
            .spawn_agent(root_id, "sibling task".to_string())
            .await?;
        let sibling_id = sibling
            .agent_id
            .ok_or_else(|| anyhow::anyhow!("sibling id"))?;
        let grandchild = control
            .spawn_agent(child_id, "grandchild task".to_string())
            .await?;
        let grandchild_id = grandchild
            .agent_id
            .ok_or_else(|| anyhow::anyhow!("grandchild id"))?;

        let cancelled = manager.cancel_turn_subtree(child_id).await?;
        let cancelled = cancelled.into_iter().collect::<HashSet<_>>();

        assert_eq!(
            cancelled,
            HashSet::from([child_id, grandchild_id]),
            "only the selected child subtree should be cancelled"
        );
        assert_eq!(control.get_status(child_id), AgentStatus::Interrupted);
        assert_eq!(control.get_status(grandchild_id), AgentStatus::Interrupted);
        assert_ne!(control.get_status(root_id), AgentStatus::Interrupted);
        assert_ne!(control.get_status(sibling_id), AgentStatus::Interrupted);

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn cancel_turn_subtree_preserves_idle_statuses_and_returns_only_active_threads()
    -> Result<()> {
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
        let child_status = wait_for_final_status(&control, child_id).await?;

        let cancelled = manager.cancel_turn_subtree(root_id).await?;

        assert!(cancelled.is_empty());
        assert_eq!(control.get_status(root_id), AgentStatus::PendingInit);
        assert_eq!(control.get_status(child_id), child_status);
        assert_ne!(control.get_status(child_id), AgentStatus::Interrupted);

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn replacing_running_turn_interrupts_live_descendants() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(None, Some(Arc::new(PendingFactory))).await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();

        // Root has a running (never-ending) turn with a live child agent.
        manager
            .start_user_input(root_id, "first prompt".to_string())
            .await?;
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        let child_id = child.agent_id.ok_or_else(|| anyhow::anyhow!("child id"))?;

        // A new turn replaces the running one: the superseded turn's live
        // descendants must not keep running against the old conversation.
        manager
            .start_user_input(root_id, "second prompt".to_string())
            .await?;

        assert_eq!(control.get_status(child_id), AgentStatus::Interrupted);

        manager.cancel_turn_subtree(root_id).await?;
        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_cascades_to_live_descendants() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(None, Some(Arc::new(PendingFactory))).await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "child task".to_string())
            .await?;
        let child_id = child.agent_id.ok_or_else(|| anyhow::anyhow!("child id"))?;
        let grandchild = control
            .spawn_agent(child_id, "grandchild task".to_string())
            .await?;
        let grandchild_id = grandchild
            .agent_id
            .ok_or_else(|| anyhow::anyhow!("grandchild id"))?;

        let result = manager
            .submit(root_id, cazean_protocol::Op::Shutdown)
            .await?;
        assert_eq!(result, "shutdown");
        assert_eq!(control.get_status(child_id), AgentStatus::Shutdown);
        assert_eq!(control.get_status(grandchild_id), AgentStatus::Shutdown);

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn resume_reaps_completed_open_subtree() -> Result<()> {
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
        // Both children completed, so resume reaps the whole subtree: a
        // Begin/End pair is still emitted for each (the End carries the terminal
        // status), but neither child is rehydrated — only root stays live and
        // the finished children's edges are closed.
        assert_eq!(resume_events, 4);
        assert_eq!(
            resumed_manager
                .agent_control()
                .registry()
                .live_agents()
                .len(),
            1
        );
        assert!(
            resumed_manager
                .state_db
                .list_open_children(&root_id.to_string())
                .await?
                .is_empty(),
            "reaping should close the finished children's edges"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn resume_thread_restores_child_prompt_kind() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        // A non-completing child stays non-terminal, so resume rehydrates it
        // (rather than reaping a finished child) and restores its prompt kind.
        let manager = ThreadManagerState::new(None, Some(Arc::new(PendingFactory))).await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        let child = manager
            .agent_control()
            .spawn_agent_with_prompt_kind(
                root_id,
                "inspect read-only".to_string(),
                SystemPromptKind::Explore,
            )
            .await?;
        let child_id = child.agent_id.ok_or_else(|| anyhow::anyhow!("child id"))?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(manager);

        let prompt_state = Arc::new(PromptKindState::default());
        let resumed_manager = ThreadManagerState::new(
            None,
            Some(Arc::new(PromptKindFactory {
                state: Arc::clone(&prompt_state),
            })),
        )
        .await?;
        let _resumed = resumed_manager.resume_thread(root_id).await?;

        let calls = lock_test_mutex(&prompt_state.calls, "prompt_kind_calls")?;
        assert!(calls.contains(&(root_id, SystemPromptKind::Root)));
        assert!(calls.contains(&(child_id, SystemPromptKind::Explore)));

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

    struct PlanModeRecordingFactory {
        calls: Arc<Mutex<Vec<bool>>>,
    }

    impl SessionModelFactory for PlanModeRecordingFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            _thread_id: ThreadId,
            _ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _system_prompt_kind: SystemPromptKind,
            _agent_control: AgentControl,
            plan_mode: bool,
        ) -> Result<SessionModel> {
            lock_test_mutex(&self.calls, "plan_mode_factory_calls")?.push(plan_mode);
            Ok(SessionModel::Stub(Arc::new(StubDriver {
                text: "done".to_string(),
            })))
        }
    }

    #[tokio::test]
    async fn plan_mode_toggle_does_not_rebuild_session_models() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(PlanModeRecordingFactory {
                calls: Arc::clone(&calls),
            })),
        )
        .await?;
        let started = manager.start_thread().await?;
        assert_eq!(
            *lock_test_mutex(&calls, "plan_mode_factory_calls")?,
            vec![false, true],
            "both plan-mode variants should be built once at thread creation"
        );

        assert!(manager.set_plan_mode(started.thread_id, true).await?);
        assert!(!manager.set_plan_mode(started.thread_id, false).await?);
        assert_eq!(
            *lock_test_mutex(&calls, "plan_mode_factory_calls")?,
            vec![false, true],
            "toggling plan mode must not rebuild the session model"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn preview_thread_snapshots_completed_child_without_mutation() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "child done".to_string(),
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
        wait_for_final_status(&control, child_id).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(manager);

        // Fresh manager: the child is not live, so the preview must come from
        // its rollout alone.
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "unused".to_string(),
                })),
            })),
        )
        .await?;
        let live_before = manager.agent_control().registry().live_agents().len();

        let preview = manager.preview_thread(child_id).await?;
        assert_eq!(preview.thread_id, child_id);
        assert!(!preview.is_live);
        assert!(matches!(preview.status, AgentStatus::Completed(_)));
        assert!(
            preview.initial_messages.iter().any(
                |event| matches!(event, EventMsg::UserMessage { text } if text == "child task")
            )
        );
        assert!(
            preview
                .initial_messages
                .iter()
                .any(|event| matches!(event, EventMsg::TurnCompleted(_)))
        );

        // Previewing must not register, resume, or otherwise mutate the thread.
        assert_eq!(
            manager.agent_control().registry().live_agents().len(),
            live_before
        );
        assert!(!manager.threads.read().await.contains_key(&child_id));

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn preview_thread_unknown_thread_errors() -> Result<()> {
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
        assert!(manager.preview_thread(ThreadId::new()).await.is_err());

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[tokio::test]
    async fn preview_thread_live_open_turn_has_no_synthetic_interruption() -> Result<()> {
        let _cwd_guard = cwd_test_lock().lock().await;
        let workspace = TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        // The pending driver never completes, so the turn stays open while the
        // thread is live.
        let manager = ThreadManagerState::new(None, Some(Arc::new(PendingFactory))).await?;
        let started = manager.start_thread().await?;
        let root_id = started.thread_id;
        manager
            .start_user_input(root_id, "long running".to_string())
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let preview = manager.preview_thread(root_id).await?;
        assert!(preview.is_live);
        assert_eq!(preview.status, AgentStatus::Running);
        assert!(
            preview
                .initial_messages
                .iter()
                .any(|event| matches!(event, EventMsg::TurnStarted(_)))
        );
        assert!(
            !preview
                .initial_messages
                .iter()
                .any(|event| matches!(event, EventMsg::TurnInterrupted(_))),
            "a live open turn must not be replayed as interrupted"
        );

        std::env::set_current_dir(original_cwd)?;
        Ok(())
    }

    #[test]
    fn folded_live_status_is_order_sensitive() {
        use cazean_protocol::{
            ErrorEvent, ErrorInfo, TurnCompletedEvent, TurnInterruptedEvent, TurnStartedEvent,
        };

        let turn_started = |turn: &str| {
            EventMsg::TurnStarted(TurnStartedEvent {
                thread_id: String::from("t"),
                turn_id: turn.to_string(),
            })
        };
        let turn_completed = |turn: &str| {
            EventMsg::TurnCompleted(TurnCompletedEvent {
                thread_id: String::from("t"),
                turn_id: turn.to_string(),
                last_assistant_message: Some(String::from("done")),
            })
        };
        let error = EventMsg::Error(ErrorEvent {
            error: ErrorInfo::new("provider", "boom"),
        });

        assert_eq!(
            super::folded_live_status(&[]),
            AgentStatus::PendingInit,
            "no events: nothing has happened yet"
        );
        assert_eq!(
            super::folded_live_status(&[turn_started("0")]),
            AgentStatus::Running,
            "an open turn on a live thread is running"
        );
        assert_eq!(
            super::folded_live_status(&[turn_started("0"), error.clone()]),
            AgentStatus::Errored(ErrorInfo::new("provider", "boom")),
            "a persisted error inside an open turn must not read as running"
        );
        assert_eq!(
            super::folded_live_status(
                &[turn_started("0"), turn_completed("0"), turn_started("1"),]
            ),
            AgentStatus::Running,
            "an earlier turn's completion must not mask a later open turn"
        );
        assert_eq!(
            super::folded_live_status(&[
                turn_started("0"),
                EventMsg::TurnInterrupted(TurnInterruptedEvent {
                    thread_id: String::from("t"),
                    turn_id: String::from("0"),
                    reason: String::from("user"),
                }),
            ]),
            AgentStatus::Interrupted,
        );
    }
}
