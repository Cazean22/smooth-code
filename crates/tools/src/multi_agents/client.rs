use std::sync::Arc;

use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use smooth_protocol::AgentStatus;

use crate::ToolFailure;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnAgentParams {
    pub message: String,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub fork_context: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WaitAgentParams {
    pub target: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub thread_id: String,
    pub agent_path: String,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub status_detail: Option<AgentStatus>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentWaitOutcome {
    pub target: String,
    pub status: String,
    pub thread_id: String,
    pub agent_path: String,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub status_detail: AgentStatus,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

pub trait MultiAgentClient: Send + Sync {
    fn spawn(&self, params: SpawnAgentParams)
    -> BoxFuture<'static, Result<AgentInfo, ToolFailure>>;
    fn send_message(
        &self,
        target: String,
        content: String,
        trigger_turn: bool,
    ) -> BoxFuture<'static, Result<String, ToolFailure>>;
    fn wait_agent(
        &self,
        params: WaitAgentParams,
    ) -> BoxFuture<'static, Result<AgentWaitOutcome, ToolFailure>>;
    fn list_agents(
        &self,
        path_prefix: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<AgentInfo>, ToolFailure>>;
    fn close_agent(&self, target: String) -> BoxFuture<'static, Result<String, ToolFailure>>;
}

pub type DynMultiAgentClient = Arc<dyn MultiAgentClient>;
