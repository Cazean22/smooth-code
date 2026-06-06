#![deny(clippy::unwrap_used, clippy::expect_used)]

mod agent_path;

pub use agent_path::{AgentPath, AgentPathError};

use std::{fmt, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorInfo {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ErrorInfo {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            source: None,
            details: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInstructions {
    pub entries: Vec<ProjectInstructionEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInstructionEntry {
    pub source_path: String,
    pub directory: String,
    pub text: String,
}

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
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EventMsg {
    /// Error while executing a submission
    Error(ErrorEvent),
    SessionConfigured(SessionConfiguredEvent),
    TurnStarted(TurnStartedEvent),
    TurnCompleted(TurnCompletedEvent),
    TurnInterrupted(TurnInterruptedEvent),
    AgentStatusChanged(AgentStatusChangedEvent),
    /// Agent text output message
    AgentMessage {
        text: String,
    },
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
    UserMessage {
        text: String,
    },
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

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeOperation {
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
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
        operation: FileChangeOperation,
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
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabResumeEndEvent {
    pub call_id: String,
    pub sender_thread_id: ThreadId,
    pub receiver_thread_id: ThreadId,
    pub receiver_agent_nickname: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct ErrorEvent {
    pub error: ErrorInfo,
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
    Errored(ErrorInfo),
    /// Agent has been shutdown.
    Shutdown,
    /// Agent is not found.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::{
        AgentPath, AgentStatus, ErrorEvent, ErrorInfo, EventMsg, FileChange, FileChangeOperation,
        FileChangeOutput, Op, SessionSource, SubAgentSource, ThreadId, ToolCallCompletedEvent,
        ToolCallResultKind,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn op_serde_round_trip_for_user_input() -> TestResult {
        let op = Op::UserInput("hello".to_string());
        let value = serde_json::to_value(&op)?;
        let decoded: Op = serde_json::from_value(value)?;
        assert_eq!(decoded, op);
        Ok(())
    }

    #[test]
    fn session_source_accessors_return_thread_spawn_metadata() -> TestResult {
        let thread_id = ThreadId::new();
        let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: thread_id,
            depth: 1,
            agent_path: Some(AgentPath::try_from("/root/worker")?),
            agent_nickname: Some("alpha".to_string()),
        });

        assert_eq!(
            source.get_agent_path(),
            Some(AgentPath::try_from("/root/worker")?)
        );
        assert_eq!(source.get_nickname(), Some("alpha".to_string()));
        Ok(())
    }

    #[test]
    fn collab_agent_completed_round_trip() -> TestResult {
        let msg = EventMsg::CollabAgentCompleted(super::CollabAgentCompletedEvent {
            parent_thread_id: ThreadId::new(),
            child_thread_id: ThreadId::new(),
            agent_path: AgentPath::try_from("/root/child")?,
            agent_nickname: Some("child".to_string()),
            status: AgentStatus::Completed(Some("done".to_string())),
            last_assistant_message: Some("done".to_string()),
        });

        let value = serde_json::to_value(&msg)?;
        let decoded: EventMsg = serde_json::from_value(value)?;
        assert_eq!(decoded, msg);
        Ok(())
    }

    #[test]
    fn text_event_variants_round_trip_as_tagged_objects() -> TestResult {
        let user_msg = EventMsg::UserMessage {
            text: "hello".to_string(),
        };
        let user_value = serde_json::to_value(&user_msg)?;
        assert_eq!(
            user_value,
            serde_json::json!({
                "type": "user_message",
                "text": "hello",
            })
        );
        let decoded_user: EventMsg = serde_json::from_value(user_value)?;
        assert_eq!(decoded_user, user_msg);

        let agent_msg = EventMsg::AgentMessage {
            text: "done".to_string(),
        };
        let agent_value = serde_json::to_value(&agent_msg)?;
        assert_eq!(
            agent_value,
            serde_json::json!({
                "type": "agent_message",
                "text": "done",
            })
        );
        let decoded_agent: EventMsg = serde_json::from_value(agent_value)?;
        assert_eq!(decoded_agent, agent_msg);
        Ok(())
    }

    #[test]
    fn tool_call_completed_defaults_to_final_result_kind() -> TestResult {
        let decoded: ToolCallCompletedEvent = serde_json::from_value(serde_json::json!({
            "threadId": "thread",
            "turnId": "turn",
            "callId": "call",
            "success": true,
            "outputPreview": "done",
            "error": null
        }))?;

        assert_eq!(decoded.result_kind, ToolCallResultKind::Final);
        assert_eq!(decoded.related_thread_id, None);
        assert_eq!(decoded.file_change, None);
        Ok(())
    }

    #[test]
    fn tool_call_completed_status_update_round_trip() -> TestResult {
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

        let value = serde_json::to_value(&event)?;
        assert_eq!(value["resultKind"], "status_update");
        assert_eq!(value["relatedThreadId"], related_thread_id.to_string());
        let decoded: ToolCallCompletedEvent = serde_json::from_value(value)?;
        assert_eq!(decoded, event);
        Ok(())
    }

    #[test]
    fn tool_call_completed_file_change_round_trip() -> TestResult {
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

        let value = serde_json::to_value(&event)?;
        assert_eq!(value["fileChange"]["path"], "file.txt");
        assert_eq!(
            value["fileChange"]["change"]["unifiedDiff"],
            "@@ -1 +1 @@\n-old\n+new\n"
        );
        assert!(value["fileChange"]["change"].get("unified_diff").is_none());
        assert!(value["fileChange"]["change"].get("move_path").is_none());
        let decoded: ToolCallCompletedEvent = serde_json::from_value(value)?;
        assert_eq!(decoded, event);
        Ok(())
    }

    #[test]
    fn omitted_file_change_round_trip() -> TestResult {
        let change = FileChange::Omitted {
            operation: FileChangeOperation::Add,
            reason: "too large".to_string(),
            added: 10,
            removed: 2,
            bytes: 600_000,
        };

        let value = serde_json::to_value(&change)?;
        assert_eq!(value["type"], "omitted");
        assert_eq!(value["operation"], "add");
        let decoded: FileChange = serde_json::from_value(value)?;
        assert_eq!(decoded, change);
        Ok(())
    }

    #[test]
    fn structured_error_event_round_trip() -> TestResult {
        let event = ErrorEvent {
            error: ErrorInfo::new("provider", "model stream failed").with_source("smooth-core"),
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(value["error"]["kind"], "provider");
        assert_eq!(value["error"]["message"], "model stream failed");
        let decoded: ErrorEvent = serde_json::from_value(value)?;
        assert_eq!(decoded, event);
        Ok(())
    }

    #[test]
    fn errored_agent_status_round_trip() -> TestResult {
        let status = AgentStatus::Errored(ErrorInfo::new("agent_failed", "boom"));
        let value = serde_json::to_value(&status)?;
        let decoded: AgentStatus = serde_json::from_value(value)?;
        assert_eq!(decoded, status);
        Ok(())
    }
}
