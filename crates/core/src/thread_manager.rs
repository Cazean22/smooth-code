use std::{collections::HashMap, sync::Arc};

use smooth_protocol::ThreadId;
use tokio::sync::RwLock;

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
}
