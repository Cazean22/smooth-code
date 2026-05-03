use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use smooth_protocol::{Event, Op, SessionSource, ThreadId};
use smooth_state_db::StateDbHandle;
use tokio::sync::{RwLock, broadcast};
use tools::{DynamicToolClient, DynamicToolClientFactory};

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
        let initial_messages = resume_state.initial_messages.clone();
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
        self.state_db
            .upsert_thread(&thread_id.to_string(), None, None, None)
            .await?;
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
}
