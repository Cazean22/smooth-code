use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ThreadId(Uuid);

impl ThreadId {
    pub fn new() -> Self {
        ThreadId(Uuid::now_v7())
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ThreadId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema)]
pub enum Op {
    UserInput(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Submission {
    /// Unique id for this Submission to correlate with Events
    pub id: String,
    /// Payload
    pub op: Op,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    /// Submission `id` that this event is correlated with.
    pub id: String,
    /// Payload
    pub msg: EventMsg,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    /// Error while executing a submission
    Error(ErrorEvent),
    SessionConfigured(SessionConfiguredEvent),
    TurnStarted(TurnStartedEvent),
    TurnCompleted(TurnCompletedEvent),
    TurnInterrupted(TurnInterruptedEvent),
    AgentStatusChanged(AgentStatusChangedEvent),
    /// Agent text output message
    AgentMessage(String),
    AgentMessageDelta(AgentMessageDeltaEvent),
    AgentMessageCompleted(AgentMessageCompletedEvent),
    ToolCallStarted(ToolCallStartedEvent),
    ToolCallCompleted(ToolCallCompletedEvent),

    /// User/system input message (what was sent to the model)
    UserMessage(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfiguredEvent {
    pub thread_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedEvent {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatusChangedEvent {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStartedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub args_preview: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub success: bool,
    pub output_preview: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ErrorEvent {
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<CoreErrorInfo>,
}

/// runtime errors that we expose to clients.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoreErrorInfo {
    ContextWindowExceeded,
    UsageLimitExceeded,
    ServerOverloaded,
    HttpConnectionFailed {
        http_status_code: Option<u16>,
    },
    /// Failed to connect to the response SSE stream.
    ResponseStreamConnectionFailed {
        http_status_code: Option<u16>,
    },
    InternalServerError,
    Unauthorized,
    BadRequest,
    SandboxError,
    /// The response SSE stream disconnected in the middle of a turnbefore completion.
    ResponseStreamDisconnected {
        http_status_code: Option<u16>,
    },
    /// Reached the retry limit for responses.
    ResponseTooManyFailedAttempts {
        http_status_code: Option<u16>,
    },
    ThreadRollbackFailed,
    Other,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Agent is waiting for initialization.
    #[default]
    PendingInit,
    /// Agent is currently running.
    Running,
    /// Agent's current turn was interrupted and it may receive more input.
    Interrupted,
    /// Agent is done. Contains the final assistant message.
    Completed(Option<String>),
    /// Agent encountered an error.
    Errored(String),
    /// Agent has been shutdown.
    Shutdown,
    /// Agent is not found.
    NotFound,
}
