use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use smooth_protocol::ThreadId;
use smooth_protocol::Event;
use tokio::sync::{RwLock, broadcast};

use crate::core_thread::CoreThread;

pub struct ThreadManagerState {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CoreThread>>>>,
}

impl ThreadManagerState {
    pub fn new() -> Self {
        Self {
            threads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn start_user_input(&self, thread_id: ThreadId, input: String) -> Result<String> {
        let thread = self.get_or_create(thread_id).await?;
        thread.start_user_input(input).await
    }

    pub async fn subscribe(&self, thread_id: ThreadId) -> Result<broadcast::Receiver<Event>> {
        let thread = self.get_or_create(thread_id).await?;
        Ok(thread.subscribe())
    }

    async fn get_or_create(&self, thread_id: ThreadId) -> Result<Arc<CoreThread>> {
        if let Some(thread) = self.threads.read().await.get(&thread_id).cloned() {
            return Ok(thread);
        }

        let mut threads = self.threads.write().await;
        if let Some(thread) = threads.get(&thread_id).cloned() {
            return Ok(thread);
        }

        let thread = Arc::new(CoreThread::new(thread_id)?);
        threads.insert(thread_id, Arc::clone(&thread));
        Ok(thread)
    }
}
