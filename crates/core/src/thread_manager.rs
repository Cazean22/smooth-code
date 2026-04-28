use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use smooth_protocol::Event;
use smooth_protocol::ThreadId;
use tokio::sync::{RwLock, broadcast};

use crate::{
    ThreadSummary,
    core_thread::CoreThread,
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
}

impl ThreadManagerState {
    pub fn new() -> Self {
        Self {
            threads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[tracing::instrument(name = "core.thread_manager.start_thread", skip(self))]
    pub async fn start_thread(&self) -> Result<StartedThread> {
        let thread_id = ThreadId::new();
        let thread = Arc::new(CoreThread::new(thread_id).await?);
        let rollout_path = thread.rollout_path().clone();

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
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
        let thread = Arc::new(CoreThread::resume(rollout_path.clone(), resume_state).await?);

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
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

    #[tracing::instrument(name = "core.thread_manager.start_user_input", skip(self, input), fields(thread_id = %thread_id, input_len = input.len()))]
    pub async fn start_user_input(&self, thread_id: ThreadId, input: String) -> Result<String> {
        let thread = self.get(thread_id).await?;
        thread.start_user_input(input).await
    }

    pub async fn subscribe(&self, thread_id: ThreadId) -> Result<broadcast::Receiver<Event>> {
        let thread = self.get(thread_id).await?;
        Ok(thread.subscribe())
    }

    async fn get(&self, thread_id: ThreadId) -> Result<Arc<CoreThread>> {
        self.threads
            .read()
            .await
            .get(&thread_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown thread id: {thread_id}"))
    }
}
