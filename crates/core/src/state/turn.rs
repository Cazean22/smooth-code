use std::sync::Arc;

use indexmap::IndexMap;
use tokio::sync::{Mutex, Notify};
use tokio_util::task::AbortOnDropHandle;

use crate::{
    core::{TurnCancel, TurnContext},
    tasks::AnySessionTask,
};

/// Metadata about the currently running turn.
pub(crate) struct ActiveTurn {
    pub(crate) tasks: IndexMap<String, RunningTask>,
    #[allow(dead_code)]
    pub(crate) turn_state: Arc<Mutex<TurnState>>,
}

impl Default for ActiveTurn {
    fn default() -> Self {
        Self {
            tasks: IndexMap::new(),
            turn_state: Arc::new(Mutex::new(TurnState::default())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum TaskKind {
    Regular,
    Review,
    Compact,
}

pub(crate) struct RunningTask {
    /// Notified by the runner once the task's body (including terminal-event
    /// emission) has finished; `drain_aborted_tasks` waits on this before
    /// deciding to hard-abort.
    pub(crate) done: Arc<Notify>,
    #[allow(dead_code)]
    pub(crate) kind: TaskKind,
    pub(crate) task: Arc<dyn AnySessionTask>,
    pub(crate) cancellation: TurnCancel,
    pub(crate) handle: AbortOnDropHandle<()>,
    pub(crate) turn_context: Arc<TurnContext>,
}

impl ActiveTurn {
    pub(crate) fn add_task(&mut self, task: RunningTask) {
        let sub_id = task.turn_context.sub_id.clone();
        self.tasks.insert(sub_id, task);
    }

    pub(crate) fn take_task(&mut self, sub_id: &str) -> Option<RunningTask> {
        self.tasks.swap_remove(sub_id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(crate) fn drain_tasks(&mut self) -> Vec<RunningTask> {
        self.tasks.drain(..).map(|(_, task)| task).collect()
    }
}

/// Mutable state for a single turn.
#[derive(Default)]
pub(crate) struct TurnState {
    #[allow(dead_code)]
    pub(crate) tool_calls: u64,
}
