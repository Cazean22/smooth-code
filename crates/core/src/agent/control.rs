use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow};
use smooth_protocol::{
    AgentStatus, CollabAgentCompletedEvent, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent,
    EventMsg, InterAgentCommunication, Op, SessionSource, SubAgentSource, ThreadId,
};
use smooth_state_db::StateDbHandle;
use tokio::sync::{RwLock, oneshot, watch};
use tools::DynamicToolClientFactory;
use uuid::Uuid;

use crate::{
    agent::{
        agent_resolver,
        fork::{SpawnAgentForkMode, persisted_items_to_messages},
        registry::{AgentMetadata, AgentRegistry},
        role::resolve_role,
        status::{agent_status_from_event, is_final, last_assistant_message},
    },
    core_thread::CoreThread,
    provider::SessionModelFactory,
    rollout::read_persisted_items,
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
    pub(crate) communication: InterAgentCommunication,
    pub(crate) status: AgentStatus,
    pub(crate) last_assistant_message: Option<String>,
}

pub(crate) type InlineChildCompletionReceiver = oneshot::Receiver<InlineChildCompletion>;

#[derive(Clone)]
struct AgentControlRuntime {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
    dynamic_tool_client_factory: Option<Arc<dyn DynamicToolClientFactory>>,
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
        dynamic_tool_client_factory: Option<Arc<dyn DynamicToolClientFactory>>,
        model_factory: Option<Arc<dyn SessionModelFactory>>,
        state_db: StateDbHandle,
    ) {
        *self
            .state
            .runtime
            .lock()
            .expect("agent control runtime mutex should lock") = Some(AgentControlRuntime {
            threads,
            dynamic_tool_client_factory,
            model_factory,
            state_db,
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

    pub(crate) fn register_existing_agent(
        &self,
        metadata: AgentMetadata,
        initial_events: &[EventMsg],
    ) -> Result<AgentMetadata> {
        let registered = self
            .state
            .registry
            .register_existing_thread(metadata.clone(), AGENT_MAX_THREADS)
            .map_err(anyhow::Error::msg)?;
        let status = initial_events
            .iter()
            .filter_map(agent_status_from_event)
            .next_back()
            .unwrap_or(AgentStatus::PendingInit);
        self.ensure_status_sender(
            registered
                .agent_id
                .ok_or_else(|| anyhow!("registered agent is missing thread id"))?,
            status,
        );
        self.maybe_start_completion_watcher(registered.clone(), false);
        Ok(registered)
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

    pub(crate) fn resolve_agent_reference(
        &self,
        author_thread_id: ThreadId,
        target: &str,
    ) -> Result<ThreadId> {
        let session_source = self.session_source_for_thread(author_thread_id)?;
        agent_resolver::resolve_agent_reference(&self.state.registry, &session_source, target)
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn list_agents(
        &self,
        author_thread_id: ThreadId,
        path_prefix: Option<&str>,
    ) -> Result<Vec<AgentMetadata>> {
        let session_source = self.session_source_for_thread(author_thread_id)?;
        agent_resolver::list_agents(&self.state.registry, &session_source, path_prefix)
            .map_err(anyhow::Error::msg)
    }

    pub(crate) async fn spawn_agent(
        &self,
        parent_thread_id: ThreadId,
        message: String,
    ) -> Result<AgentMetadata> {
        self.spawn_agent_with_role(parent_thread_id, message, None, None, false)
            .await
    }

    pub(crate) async fn spawn_agent_with_role(
        &self,
        parent_thread_id: ThreadId,
        message: String,
        agent_role: Option<String>,
        model: Option<String>,
        fork_context: bool,
    ) -> Result<AgentMetadata> {
        self.spawn_agent_with_role_internal(
            parent_thread_id,
            message,
            agent_role,
            model,
            fork_context,
            false,
        )
        .await
        .map(|(metadata, _, _)| metadata)
    }

    pub(crate) async fn spawn_agent_with_role_inline_wait(
        &self,
        parent_thread_id: ThreadId,
        message: String,
        agent_role: Option<String>,
        model: Option<String>,
        fork_context: bool,
    ) -> Result<(AgentMetadata, InlineChildCompletionReceiver)> {
        let (metadata, _child_thread_id, waiter) = self
            .spawn_agent_with_role_internal(
                parent_thread_id,
                message,
                agent_role,
                model,
                fork_context,
                true,
            )
            .await?;
        let waiter = waiter.expect("inline waiter should be registered");
        Ok((metadata, waiter))
    }

    pub(crate) async fn spawn_agent_with_role_for_tool(
        &self,
        parent_thread_id: ThreadId,
        message: String,
        agent_role: Option<String>,
        model: Option<String>,
        fork_context: bool,
    ) -> Result<(AgentMetadata, AgentStatus, InlineChildCompletionReceiver)> {
        let call_id = Uuid::now_v7().to_string();
        self.emit_collab_event(
            parent_thread_id,
            EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                call_id: call_id.clone(),
                sender_thread_id: parent_thread_id,
                prompt: message.clone(),
                model: model.clone(),
            }),
        )
        .await;

        match self
            .spawn_agent_with_role_inline_wait(
                parent_thread_id,
                message.clone(),
                agent_role.clone(),
                model.clone(),
                fork_context,
            )
            .await
        {
            Ok((metadata, waiter)) => {
                let thread_id = metadata
                    .agent_id
                    .ok_or_else(|| anyhow!("spawned agent is missing thread id"))?;
                let status = self.get_status(thread_id);
                self.emit_collab_event(
                    parent_thread_id,
                    EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                        call_id,
                        sender_thread_id: parent_thread_id,
                        new_thread_id: Some(thread_id),
                        new_agent_nickname: metadata.agent_nickname.clone(),
                        new_agent_role: metadata.agent_role.clone(),
                        prompt: message,
                        model,
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
                        new_agent_role: agent_role,
                        prompt: message,
                        model,
                        status: AgentStatus::Errored(err.to_string()),
                    }),
                )
                .await;
                Err(err)
            }
        }
    }

    async fn spawn_agent_with_role_internal(
        &self,
        parent_thread_id: ThreadId,
        message: String,
        agent_role: Option<String>,
        model: Option<String>,
        fork_context: bool,
        inline_wait: bool,
    ) -> Result<(
        AgentMetadata,
        ThreadId,
        Option<InlineChildCompletionReceiver>,
    )> {
        if let Some(role) = agent_role.as_deref()
            && resolve_role(role).is_none()
        {
            return Err(anyhow!("unknown agent role `{role}`"));
        }
        if model.is_some() {
            return Err(anyhow!("spawn_agent model override is not implemented yet"));
        }

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
            agent_role: agent_role.clone(),
        });
        let dynamic_tool_client = runtime
            .dynamic_tool_client_factory
            .as_ref()
            .map(|factory| factory.build(child_thread_id));
        let initial_history = if fork_context {
            self.load_fork_history(parent_thread_id, SpawnAgentForkMode::ParentHistory)
                .await?
        } else {
            Vec::new()
        };
        let child_thread = Arc::new(
            CoreThread::new_with_history(
                child_thread_id,
                dynamic_tool_client,
                runtime.model_factory.clone(),
                child_source,
                self.clone(),
                initial_history,
            )
            .await?,
        );

        {
            let mut threads = runtime.threads.write().await;
            threads.insert(child_thread_id, Arc::clone(&child_thread));
        }
        self.ensure_status_sender(child_thread_id, AgentStatus::PendingInit);
        let inline_waiter =
            inline_wait.then(|| self.register_inline_child_completion_waiter(child_thread_id));

        if let Err(err) = child_thread.submit(Op::UserInput(message)).await {
            runtime.threads.write().await.remove(&child_thread_id);
            self.remove_status_sender(child_thread_id);
            if inline_wait {
                self.unregister_inline_child_completion_waiter(child_thread_id);
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
                agent_role,
                parent_thread_id: Some(parent_thread_id),
                depth,
            })
            .map_err(anyhow::Error::msg)?;
        runtime
            .state_db
            .upsert_thread(
                &child_thread_id.to_string(),
                Some(metadata.agent_path.as_str()),
                metadata.agent_nickname.as_deref(),
                metadata.agent_role.as_deref(),
            )
            .await?;
        runtime
            .state_db
            .upsert_open_edge(&parent_thread_id.to_string(), &child_thread_id.to_string())
            .await?;
        self.maybe_start_completion_watcher(metadata.clone(), true);
        Ok((metadata, child_thread_id, inline_waiter))
    }

    pub(crate) async fn send_input(
        &self,
        author_thread_id: ThreadId,
        target: &str,
        content: String,
        trigger_turn: bool,
    ) -> Result<String> {
        let recipient_thread_id = self.resolve_agent_reference(author_thread_id, target)?;
        let author = self
            .state
            .registry
            .agent_metadata_for_thread(author_thread_id)
            .ok_or_else(|| anyhow!("unknown author thread id: {author_thread_id}"))?;
        let recipient = self
            .state
            .registry
            .agent_metadata_for_thread(recipient_thread_id)
            .ok_or_else(|| anyhow!("unknown recipient thread id: {recipient_thread_id}"))?;
        self.send_input_to_recipient(&author, &recipient, content, trigger_turn)
            .await
    }

    pub(crate) async fn close_agent(
        &self,
        author_thread_id: ThreadId,
        target: &str,
    ) -> Result<AgentStatus> {
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
            .ok_or_else(|| anyhow!("unknown live agent thread id: {target_thread_id}"))?;
        drop(threads);

        let _ = thread.submit(Op::Shutdown).await?;
        thread.core.session.abort_all_tasks("closed").await;
        runtime.threads.write().await.remove(&target_thread_id);
        self.state.registry.unregister_thread(target_thread_id);
        self.remove_status_sender(target_thread_id);
        Ok(AgentStatus::Shutdown)
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

    pub(crate) fn register_inline_child_completion_waiter(
        &self,
        child_thread_id: ThreadId,
    ) -> InlineChildCompletionReceiver {
        let (tx, rx) = oneshot::channel();
        self.state
            .inline_waiters
            .lock()
            .expect("agent control inline waiter mutex should lock")
            .insert(child_thread_id, tx);
        rx
    }

    pub(crate) fn unregister_inline_child_completion_waiter(&self, child_thread_id: ThreadId) {
        self.state
            .inline_waiters
            .lock()
            .expect("agent control inline waiter mutex should lock")
            .remove(&child_thread_id);
    }

    fn session_source_for_thread(&self, thread_id: ThreadId) -> Result<SessionSource> {
        let Some(metadata) = self.state.registry.agent_metadata_for_thread(thread_id) else {
            return Err(anyhow!("unknown thread id: {thread_id}"));
        };
        if metadata.parent_thread_id.is_none() || metadata.depth == 0 {
            return Ok(SessionSource::Cli);
        }
        Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: metadata
                .parent_thread_id
                .ok_or_else(|| anyhow!("missing parent thread id for {thread_id}"))?,
            depth: metadata.depth,
            agent_path: Some(metadata.agent_path),
            agent_nickname: metadata.agent_nickname,
            agent_role: metadata.agent_role,
        }))
    }

    fn runtime(&self) -> Result<AgentControlRuntime> {
        self.state
            .runtime
            .lock()
            .expect("agent control runtime mutex should lock")
            .clone()
            .ok_or_else(|| anyhow!("agent control runtime is not attached"))
    }

    fn maybe_start_completion_watcher(&self, child: AgentMetadata, notify_if_already_final: bool) {
        let Some(parent_thread_id) = child.parent_thread_id else {
            return;
        };
        let Some(child_thread_id) = child.agent_id else {
            return;
        };
        let mut status_rx = self.subscribe_status(child_thread_id);
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

    async fn load_fork_history(
        &self,
        parent_thread_id: ThreadId,
        _fork_mode: SpawnAgentForkMode,
    ) -> Result<Vec<rig::message::Message>> {
        let runtime = self.runtime()?;
        let threads = runtime.threads.read().await;
        let parent_thread = threads
            .get(&parent_thread_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown parent thread id: {parent_thread_id}"))?;
        drop(threads);

        parent_thread.flush_rollout().await?;
        let items = read_persisted_items(parent_thread.rollout_path()).await?;
        Ok(persisted_items_to_messages(items))
    }

    async fn send_input_to_recipient(
        &self,
        author: &AgentMetadata,
        recipient: &AgentMetadata,
        content: String,
        trigger_turn: bool,
    ) -> Result<String> {
        let recipient_thread_id = recipient
            .agent_id
            .ok_or_else(|| anyhow!("recipient agent metadata is missing thread id"))?;
        let runtime = self.runtime()?;
        let threads = runtime.threads.read().await;
        let thread = threads
            .get(&recipient_thread_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown live recipient thread id: {recipient_thread_id}"))?;
        drop(threads);

        thread
            .submit(Op::InterAgentCommunication {
                communication: InterAgentCommunication::new(
                    author.agent_path.clone(),
                    recipient.agent_path.clone(),
                    vec![],
                    content,
                    trigger_turn,
                ),
            })
            .await
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
                agent_role: child.agent_role.clone(),
                last_assistant_message: last_assistant_message(&status),
                status: status.clone(),
            }),
        )
        .await;

        let Some(parent) = self
            .state
            .registry
            .agent_metadata_for_thread(parent_thread_id)
        else {
            return;
        };
        let notice = render_completion_notice(child, &status);
        let communication = InterAgentCommunication::new(
            child.agent_path.clone(),
            parent.agent_path.clone(),
            vec![],
            notice,
            true,
        );

        let last_assistant_message = last_assistant_message(&status);
        if let Some(waiter) = self
            .state
            .inline_waiters
            .lock()
            .expect("agent control inline waiter mutex should lock")
            .remove(&child_thread_id)
        {
            if waiter
                .send(InlineChildCompletion {
                    communication: communication.clone(),
                    status,
                    last_assistant_message,
                })
                .is_ok()
            {
                return;
            }
        }

        if let Err(err) = self
            .send_input_to_recipient(child, &parent, communication.content.clone(), true)
            .await
        {
            tracing::warn!(
                parent_thread_id = %parent_thread_id,
                child_thread_id = %child_thread_id,
                error = %err,
                "failed to deliver child completion notice to parent mailbox"
            );
        }
    }
}

fn should_notify_parent_on_completion(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_) | AgentStatus::Interrupted | AgentStatus::Errored(_)
    )
}

fn render_completion_notice(child: &AgentMetadata, status: &AgentStatus) -> String {
    let mut lines = vec![
        "[agent_completed]".to_string(),
        format!("agent_path={}", child.agent_path),
        format!("status={}", agent_status_label(status)),
    ];
    if let Some(agent_nickname) = child.agent_nickname.as_deref() {
        lines.push(format!(
            "agent_nickname={}",
            escape_notice_value(agent_nickname)
        ));
    }
    if let Some(agent_role) = child.agent_role.as_deref() {
        lines.push(format!("agent_role={}", escape_notice_value(agent_role)));
    }
    if let Some(last_message) = last_assistant_message(status) {
        lines.push(format!(
            "last_assistant_message={}",
            escape_notice_value(&last_message)
        ));
    }
    lines.push("[/agent_completed]".to_string());
    lines.join("\n")
}

fn agent_status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::PendingInit => "pending_init",
        AgentStatus::Running => "running",
        AgentStatus::Completed(_) => "completed",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Errored(_) => "errored",
        AgentStatus::Shutdown => "shutdown",
        AgentStatus::NotFound => "not_found",
    }
}

fn escape_notice_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use futures_util::stream;
    use rig::{
        agent::FinalResponse,
        message::{Message, Text, UserContent},
    };
    use smooth_state_db::StateDbHandle;
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    use super::AgentControl;
    use crate::{
        SessionModel, SessionModelDriver, SessionModelFactory, SessionStream,
        agent::role::RoleOverride, provider::SessionStreamEvent,
        thread_manager::ThreadManagerState,
    };
    use smooth_protocol::{AgentStatus, EventMsg, ThreadId};
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
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _role_override: RoleOverride,
            _agent_control: AgentControl,
        ) -> Result<SessionModel> {
            let _ = thread_id;
            Ok(self.model.clone())
        }
    }

    #[derive(Default)]
    struct RecordingState {
        calls: Mutex<HashMap<ThreadId, Vec<Vec<Message>>>>,
    }

    struct RecordingDriver {
        thread_id: ThreadId,
        state: Arc<RecordingState>,
        text: String,
    }

    impl SessionModelDriver for RecordingDriver {
        fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream> {
            self.state
                .calls
                .lock()
                .expect("calls mutex should lock")
                .entry(self.thread_id)
                .or_default()
                .push(history.clone());
            let _ = prompt;
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

    struct RecordingFactory {
        state: Arc<RecordingState>,
    }

    impl SessionModelFactory for RecordingFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            thread_id: ThreadId,
            _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _role_override: RoleOverride,
            _agent_control: AgentControl,
        ) -> Result<SessionModel> {
            Ok(SessionModel::Stub(Arc::new(RecordingDriver {
                thread_id,
                state: Arc::clone(&self.state),
                text: "recorded".to_string(),
            })))
        }
    }

    #[tokio::test]
    async fn spawn_agent_creates_child_and_tracks_it_live() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
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
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;

        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await
            .expect("spawn child");

        assert!(child.agent_path.as_str().starts_with("/root/"));
        assert_eq!(control.registry().live_agents().len(), 2);
        let state_db = StateDbHandle::open(workspace.path().join(".smooth-code/state.db"))
            .await
            .expect("open state db");
        let root_row = state_db
            .get_thread(&root_id.to_string())
            .await
            .expect("get root row")
            .expect("root row");
        assert_eq!(root_row.agent_path, None);
        let child_id = child.agent_id.expect("child id");
        let child_row = state_db
            .get_thread(&child_id.to_string())
            .await
            .expect("get child row")
            .expect("child row");
        assert_eq!(
            child_row.agent_path.as_deref(),
            Some(child.agent_path.as_str())
        );
        assert_eq!(
            state_db
                .list_open_children(&root_id.to_string())
                .await
                .expect("list open children")
                .len(),
            1
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn send_input_resolves_relative_agent_target() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await
            .expect("spawn child");
        let child_id = child.agent_id.expect("child id");
        let child_name = child.agent_path.name().to_string();

        let _turn_id = control
            .send_input(child_id, "/root", "wake root".to_string(), true)
            .await
            .expect("send input");

        let mut saw_mail = false;
        for _ in 0..10 {
            let event = root_events.recv().await.expect("root event");
            if let EventMsg::InterAgentMessage(mail) = event.msg {
                if mail.communication.content == "wake root" {
                    saw_mail = true;
                    break;
                }
            }
        }
        assert!(saw_mail);

        let listed = control
            .list_agents(child_id, Some(".."))
            .expect_err("invalid relative");
        assert!(listed.to_string().contains("`..` is reserved"));
        let listed = control
            .list_agents(root_id, Some(&child_name))
            .expect("list child");
        assert_eq!(listed.len(), 1);

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn close_agent_removes_live_child() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await
            .expect("spawn child");

        let status = control
            .close_agent(root_id, child.agent_path.as_str())
            .await
            .expect("close child");
        assert_eq!(status, AgentStatus::Shutdown);
        assert_eq!(control.registry().live_agents().len(), 1);
        let state_db = StateDbHandle::open(workspace.path().join(".smooth-code/state.db"))
            .await
            .expect("open state db");
        assert!(
            state_db
                .list_open_children(&root_id.to_string())
                .await
                .expect("list open children")
                .is_empty()
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn completion_watcher_emits_parent_completion_event_and_mailbox_notice() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
        let control = manager.agent_control();
        let child = control
            .spawn_agent(root_id, "hello child".to_string())
            .await
            .expect("spawn child");
        let child_id = child.agent_id.expect("child id");
        let mut saw_completion = false;
        let mut saw_notice = false;
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
                    assert_eq!(completion.agent_role, child.agent_role);
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
                EventMsg::InterAgentMessage(mail) => {
                    if mail.communication.author == child.agent_path {
                        assert!(mail.communication.trigger_turn);
                        assert!(mail.communication.content.contains("[agent_completed]"));
                        assert!(
                            mail.communication
                                .content
                                .contains(&format!("agent_path={}", child.agent_path))
                        );
                        assert!(mail.communication.content.contains("status=completed"));
                        assert!(
                            mail.communication
                                .content
                                .contains("last_assistant_message=response")
                        );
                        saw_notice = true;
                    }
                }
                _ => {}
            }

            if saw_completion && saw_notice {
                break;
            }
        }

        assert!(
            saw_completion,
            "expected completion watcher to emit collab event"
        );
        assert!(
            saw_notice,
            "expected completion watcher to queue parent mailbox notice"
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn completion_watcher_resolves_inline_waiter_without_mailbox_notice() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
        let control = manager.agent_control();
        let (child, waiter) = control
            .spawn_agent_with_role_inline_wait(
                root_id,
                "hello child".to_string(),
                Some("worker".to_string()),
                None,
                false,
            )
            .await
            .expect("spawn child");
        let child_id = child.agent_id.expect("child id");
        let completion = waiter.await.expect("inline waiter result");

        assert_eq!(
            completion.status,
            AgentStatus::Completed(Some("response".to_string()))
        );
        assert_eq!(
            completion.last_assistant_message.as_deref(),
            Some("response")
        );
        assert_eq!(completion.communication.author, child.agent_path);
        assert!(
            completion
                .communication
                .content
                .contains("[agent_completed]")
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
                EventMsg::InterAgentMessage(mail) => {
                    if mail.communication.author == child.agent_path {
                        panic!("inline waiter should suppress mailbox notice");
                    }
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

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn completion_watcher_falls_back_to_mailbox_after_inline_waiter_unregisters() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "response".into(),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
        let control = manager.agent_control();
        let (child, waiter) = control
            .spawn_agent_with_role_inline_wait(
                root_id,
                "hello child".to_string(),
                Some("worker".to_string()),
                None,
                false,
            )
            .await
            .expect("spawn child");
        let child_id = child.agent_id.expect("child id");
        control.unregister_inline_child_completion_waiter(child_id);
        drop(waiter);

        let mut saw_completion = false;
        let mut saw_notice = false;
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
                    saw_completion = true;
                }
                EventMsg::InterAgentMessage(mail) => {
                    if mail.communication.author == child.agent_path {
                        saw_notice = true;
                    }
                }
                _ => {}
            }

            if saw_completion && saw_notice {
                break;
            }
        }

        assert!(saw_completion, "expected child completion event");
        assert!(
            saw_notice,
            "expected mailbox fallback notice after unregister"
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn spawn_agent_with_fork_context_seeds_child_history() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let recording_state = Arc::new(RecordingState::default());
        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(RecordingFactory {
                state: Arc::clone(&recording_state),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let root_id = started.thread_id;
        manager
            .start_user_input(root_id, "parent asks".to_string())
            .await
            .expect("parent turn");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let control = manager.agent_control();
        let child = control
            .spawn_agent_with_role(
                root_id,
                "child task".to_string(),
                Some("explorer".to_string()),
                None,
                true,
            )
            .await
            .expect("spawn child");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let child_id = child.agent_id.expect("child id");
        let calls = recording_state
            .calls
            .lock()
            .expect("calls mutex should lock");
        let child_history = calls
            .get(&child_id)
            .and_then(|calls| calls.first())
            .expect("child first call history");
        assert_eq!(child_history.len(), 1);
        assert!(matches!(
            &child_history[0],
            Message::User { content }
                if matches!(
                    content.iter().next(),
                    Some(UserContent::Text(Text { text })) if text == "parent asks"
                )
        ));

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }
}
