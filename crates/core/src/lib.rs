#![deny(clippy::unwrap_used, clippy::expect_used)]

mod agent;
mod context_manager;
mod core;
mod core_thread;
mod environment;
mod error;
mod provider;
mod rollout;
mod state;
mod tasks;
pub mod test_support;
mod thread_manager;

pub use agent::AgentControl;
pub use agent::role::RoleOverride;
pub use error::{CoreError, CoreResult};
pub use provider::{
    EnvSessionModelFactory, SessionAssistantContent, SessionCompletionEvent,
    SessionCompletionStream, SessionModel, SessionModelDriver, SessionModelFactory, SessionStream,
    SessionStreamEvent, SessionTurnSummary,
};
pub use rollout::ThreadSummary;
pub use thread_manager::{ResumedThread, StartedThread, ThreadManagerState};
pub use tools::AskUserClient;
