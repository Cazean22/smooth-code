use crate::context_manager::ContextManager;

/// Persistent, session-scoped state previously stored directly on `Session`.
pub(crate) struct SessionState {
    pub(crate) history: ContextManager,
    next_turn_is_first: bool,
}
