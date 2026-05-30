mod agent_path;

pub use agent_path::AgentPath;

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[schemars(with = "String")]
pub struct ThreadId(Uuid);

impl ThreadId {
    pub fn new() -> Self {
        ThreadId(Uuid::now_v7())
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
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
    Interrupt,
    Shutdown,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Submission {
    /// Unique id for this Submission to correlate with Events
    pub id: String,
    /// Payload
    pub op: Op,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Event {
    /// Submission `id` that this event is correlated with.
    pub id: String,
    /// Payload
    pub msg: EventMsg,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
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
    AgentReasoningDelta(AgentReasoningDeltaEvent),
    AgentReasoningCompleted(AgentReasoningCompletedEvent),
    ToolCallStarted(ToolCallStartedEvent),
    ToolCallCompleted(ToolCallCompletedEvent),
    CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent),
    CollabAgentSpawnEnd(CollabAgentSpawnEndEvent),
    CollabAgentCompleted(CollabAgentCompletedEvent),
    CollabResumeBegin(CollabResumeBeginEvent),
    CollabResumeEnd(CollabResumeEndEvent),
    PlanModeChanged(PlanModeChangedEvent),

    /// User/system input message (what was sent to the model)
    UserMessage(String),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    #[default]
    Cli,
    SubAgent(SubAgentSource),
}

impl SessionSource {
    pub fn get_agent_path(&self) -> Option<AgentPath> {
        match self {
            SessionSource::Cli => None,
            SessionSource::SubAgent(SubAgentSource::Review) => None,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) => {
                agent_path.clone()
            }
        }
    }

    pub fn get_nickname(&self) -> Option<String> {
        match self {
            SessionSource::Cli | SessionSource::SubAgent(SubAgentSource::Review) => None,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_nickname, .. }) => {
                agent_nickname.clone()
            }
        }
    }

    pub fn get_agent_role(&self) -> Option<String> {
        match self {
            SessionSource::Cli | SessionSource::SubAgent(SubAgentSource::Review) => None,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) => {
                agent_role.clone()
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentSource {
    Review,
    ThreadSpawn {
        parent_thread_id: ThreadId,
        depth: i32,
        agent_path: Option<AgentPath>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfiguredEvent {
    pub thread_id: String,
    pub rollout_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedEvent {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatusChangedEvent {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeChangedEvent {
    pub thread_id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentReasoningDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentReasoningCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStartedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub args_preview: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallResultKind {
    #[default]
    Final,
    StatusUpdate,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileChange {
    Add {
        content: String,
    },
    Delete {
        content: String,
    },
    Update {
        unified_diff: String,
        move_path: Option<PathBuf>,
    },
    Omitted {
        reason: String,
        added: usize,
        removed: usize,
        bytes: usize,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeOutput {
    pub path: PathBuf,
    pub change: FileChange,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub success: bool,
    pub output_preview: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub result_kind: ToolCallResultKind,
    #[serde(default)]
    pub related_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_change: Option<FileChangeOutput>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentStatusEntry {
    pub thread_id: ThreadId,
    pub agent_path: AgentPath,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub status: AgentStatus,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentSpawnBeginEvent {
    pub call_id: String,
    pub sender_thread_id: ThreadId,
    pub prompt: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentSpawnEndEvent {
    pub call_id: String,
    pub sender_thread_id: ThreadId,
    pub new_thread_id: Option<ThreadId>,
    pub new_agent_nickname: Option<String>,
    pub new_agent_role: Option<String>,
    pub prompt: String,
    pub model: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentCompletedEvent {
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub agent_path: AgentPath,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub status: AgentStatus,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabResumeBeginEvent {
    pub call_id: String,
    pub sender_thread_id: ThreadId,
    pub receiver_thread_id: ThreadId,
    pub receiver_agent_nickname: Option<String>,
    pub receiver_agent_role: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabResumeEndEvent {
    pub call_id: String,
    pub sender_thread_id: ThreadId,
    pub receiver_thread_id: ThreadId,
    pub receiver_agent_nickname: Option<String>,
    pub receiver_agent_role: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
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

#[cfg(test)]
mod tests {
    use super::{
        AgentPath, AgentStatus, EventMsg, FileChange, FileChangeOutput, Op, SessionSource,
        SubAgentSource, ThreadId, ToolCallCompletedEvent, ToolCallResultKind,
    };

    #[test]
    fn op_serde_round_trip_for_user_input() {
        let op = Op::UserInput("hello".to_string());
        let value = serde_json::to_value(&op).expect("serialize op");
        let decoded: Op = serde_json::from_value(value).expect("deserialize op");
        assert_eq!(decoded, op);
    }

    #[test]
    fn session_source_accessors_return_thread_spawn_metadata() {
        let thread_id = ThreadId::new();
        let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: thread_id,
            depth: 1,
            agent_path: Some(AgentPath::try_from("/root/worker").expect("path")),
            agent_nickname: Some("alpha".to_string()),
            agent_role: Some("explorer".to_string()),
        });

        assert_eq!(
            source.get_agent_path(),
            Some(AgentPath::try_from("/root/worker").expect("path"))
        );
        assert_eq!(source.get_nickname(), Some("alpha".to_string()));
        assert_eq!(source.get_agent_role(), Some("explorer".to_string()));
    }

    #[test]
    fn collab_agent_completed_round_trip() {
        let msg = EventMsg::CollabAgentCompleted(super::CollabAgentCompletedEvent {
            parent_thread_id: ThreadId::new(),
            child_thread_id: ThreadId::new(),
            agent_path: AgentPath::try_from("/root/child").expect("path"),
            agent_nickname: Some("child".to_string()),
            agent_role: Some("worker".to_string()),
            status: AgentStatus::Completed(Some("done".to_string())),
            last_assistant_message: Some("done".to_string()),
        });

        let value = serde_json::to_value(&msg).expect("serialize event");
        let decoded: EventMsg = serde_json::from_value(value).expect("deserialize event");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn tool_call_completed_defaults_to_final_result_kind() {
        let decoded: ToolCallCompletedEvent = serde_json::from_value(serde_json::json!({
            "threadId": "thread",
            "turnId": "turn",
            "callId": "call",
            "success": true,
            "outputPreview": "done",
            "error": null
        }))
        .expect("deserialize tool completion");

        assert_eq!(decoded.result_kind, ToolCallResultKind::Final);
        assert_eq!(decoded.related_thread_id, None);
        assert_eq!(decoded.file_change, None);
    }

    #[test]
    fn tool_call_completed_status_update_round_trip() {
        let related_thread_id = ThreadId::new();
        let event = ToolCallCompletedEvent {
            thread_id: String::from("thread"),
            turn_id: String::from("turn"),
            call_id: String::from("call"),
            success: true,
            output_preview: Some(String::from("running")),
            error: None,
            result_kind: ToolCallResultKind::StatusUpdate,
            related_thread_id: Some(related_thread_id),
            file_change: None,
        };

        let value = serde_json::to_value(&event).expect("serialize tool completion");
        assert_eq!(value["resultKind"], "status_update");
        assert_eq!(value["relatedThreadId"], related_thread_id.to_string());
        let decoded: ToolCallCompletedEvent =
            serde_json::from_value(value).expect("deserialize tool completion");
        assert_eq!(decoded, event);
    }

    #[test]
    fn tool_call_completed_file_change_round_trip() {
        let event = ToolCallCompletedEvent {
            thread_id: String::from("thread"),
            turn_id: String::from("turn"),
            call_id: String::from("call"),
            success: true,
            output_preview: Some(String::from("edited file.txt (1 replacement)")),
            error: None,
            result_kind: ToolCallResultKind::Final,
            related_thread_id: None,
            file_change: Some(FileChangeOutput {
                path: "file.txt".into(),
                change: FileChange::Update {
                    unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                    move_path: None,
                },
            }),
        };

        let value = serde_json::to_value(&event).expect("serialize tool completion");
        assert_eq!(value["fileChange"]["path"], "file.txt");
        let decoded: ToolCallCompletedEvent =
            serde_json::from_value(value).expect("deserialize tool completion");
        assert_eq!(decoded, event);
    }

    #[test]
    fn omitted_file_change_round_trip() {
        let change = FileChange::Omitted {
            reason: "too large".to_string(),
            added: 10,
            removed: 2,
            bytes: 600_000,
        };

        let value = serde_json::to_value(&change).expect("serialize omitted change");
        assert_eq!(value["type"], "omitted");
        let decoded: FileChange =
            serde_json::from_value(value).expect("deserialize omitted change");
        assert_eq!(decoded, change);
    }
}
