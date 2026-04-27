use crate::context_manager::ContextManager;

/// Persistent, session-scoped state previously stored directly on `Session`.
pub(crate) struct SessionState {
    pub(crate) history: ContextManager,
    pub(crate) next_turn_is_first: bool,
}

impl SessionState {
    pub(crate) fn new(history: ContextManager) -> Self {
        Self {
            history,
            next_turn_is_first: true,
        }
    }
}
