use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use smooth_protocol::Event;
use smooth_protocol::ThreadId;
use tokio::sync::{RwLock, broadcast};

use crate::core_thread::CoreThread;

pub struct StartedThread {
    pub thread_id: ThreadId,
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

    pub async fn start_thread(&self) -> Result<StartedThread> {
        let thread_id = ThreadId::new();
        let thread = Arc::new(CoreThread::new(thread_id)?);

        let mut threads = self.threads.write().await;
        threads.insert(thread_id, thread);
        Ok(StartedThread { thread_id })
    }

    pub async fn emit_session_configured(&self, thread_id: ThreadId) -> Result<()> {
        self.get(thread_id).await?.emit_session_configured().await;
        Ok(())
    }

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
