use cazean_protocol::{ErrorInfo, ThreadId};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("unknown thread id: {thread_id}")]
    UnknownThread { thread_id: ThreadId },
    #[error("parent thread not registered: {thread_id}")]
    ParentThreadNotRegistered { thread_id: ThreadId },
    #[error("agent depth limit exceeded: {depth} > {max_depth}")]
    AgentDepthLimitExceeded { depth: i32, max_depth: i32 },
    #[error("agent thread limit exceeded: {max_threads}")]
    AgentThreadLimitExceeded { max_threads: usize },
    #[error("missing thread metadata for {thread_id}")]
    MissingThreadMetadata { thread_id: ThreadId },
    #[error("missing agent_path for child thread {thread_id}")]
    MissingAgentPath { thread_id: ThreadId },
    #[error("invalid agent path `{path}` for thread {thread_id}: {source}")]
    InvalidAgentPath {
        thread_id: ThreadId,
        path: String,
        source: cazean_protocol::AgentPathError,
    },
    #[error("agent control runtime is not attached")]
    RuntimeNotAttached,
    #[error("mutex `{name}` was poisoned")]
    MutexPoisoned { name: &'static str },
    #[error("core invariant violated: {message}")]
    InvariantViolation { message: String },
    #[error("failed to spawn task `{task_name}`: {source}")]
    TaskSpawn {
        task_name: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("state database error: {0}")]
    StateDb(#[from] cazean_state_db::StateDbError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("thread id parse error: {0}")]
    ThreadIdParse(#[from] uuid::Error),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("rollout error: {0}")]
    Rollout(String),
    #[error("agent registry error: {0}")]
    Registry(String),
    #[error("agent control error: {0}")]
    AgentControl(String),
    #[error("{0}")]
    Other(String),
}

pub type CoreResult<T> = Result<T, CoreError>;

impl CoreError {
    pub fn invariant(message: impl Into<String>) -> Self {
        Self::InvariantViolation {
            message: message.into(),
        }
    }

    pub fn registry(message: impl Into<String>) -> Self {
        Self::Registry(message.into())
    }

    pub fn control(message: impl Into<String>) -> Self {
        Self::AgentControl(message.into())
    }

    pub fn provider(error: anyhow::Error) -> Self {
        Self::Provider(error.to_string())
    }

    pub fn rollout(error: anyhow::Error) -> Self {
        Self::Rollout(error.to_string())
    }

    pub fn to_error_info(&self) -> ErrorInfo {
        let kind = match self {
            Self::UnknownThread { .. } => "unknown_thread",
            Self::ParentThreadNotRegistered { .. } => "parent_thread_not_registered",
            Self::AgentDepthLimitExceeded { .. } => "agent_depth_limit_exceeded",
            Self::AgentThreadLimitExceeded { .. } => "agent_thread_limit_exceeded",
            Self::MissingThreadMetadata { .. } => "missing_thread_metadata",
            Self::MissingAgentPath { .. } => "missing_agent_path",
            Self::InvalidAgentPath { .. } => "invalid_agent_path",
            Self::RuntimeNotAttached => "runtime_not_attached",
            Self::MutexPoisoned { .. } => "mutex_poisoned",
            Self::InvariantViolation { .. } => "invariant_violation",
            Self::TaskSpawn { .. } => "task_spawn",
            Self::StateDb(_) => "state_db",
            Self::Serialization(_) => "serialization",
            Self::Io(_) => "io",
            Self::ThreadIdParse(_) => "thread_id_parse",
            Self::Provider(_) => "provider",
            Self::Rollout(_) => "rollout",
            Self::Registry(_) => "registry",
            Self::AgentControl(_) => "agent_control",
            Self::Other(_) => "core_error",
        };
        ErrorInfo::new(kind, self.to_string()).with_source("cazean-core")
    }
}

impl From<anyhow::Error> for CoreError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value.to_string())
    }
}
