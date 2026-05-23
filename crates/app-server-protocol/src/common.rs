use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RequestId;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    pub thread_id: String,
    pub rollout_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeResponse {
    pub thread_id: String,
    pub rollout_path: String,
    pub initial_messages: Vec<smooth_protocol::EventMsg>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListParams {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListItem {
    pub thread_id: String,
    pub rollout_path: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    pub data: Vec<ThreadListItem>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetPlanModeParams {
    pub thread_id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetPlanModeResponse {
    pub thread_id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub questions: Vec<AskUserQuestion>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<AskUserQuestionOption>,
    pub multi_select: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionResponse {
    pub answers: Vec<AskUserQuestionAnswer>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionAnswer {
    pub question: String,
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[allow(clippy::large_enum_variant)]
pub enum ServerRequestPayload {
    DynamicToolCall(DynamicToolCallParams),
    AskUserQuestion(AskUserQuestionParams),
}

impl ServerRequestPayload {
    pub fn request_with_id(self, request_id: RequestId) -> ServerRequest {
        match self {
            Self::DynamicToolCall(params) => ServerRequest::DynamicToolCall { request_id, params },
            Self::AskUserQuestion(params) => ServerRequest::AskUserQuestion { request_id, params },
        }
    }
}

#[doc = r" Request from the client to the server."]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ClientRequest {
    ThreadStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ThreadStartParams,
    },
    TurnStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: TurnStartParams,
    },
    ThreadResume {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ThreadResumeParams,
    },
    ThreadList {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ThreadListParams,
    },
    SetPlanMode {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: SetPlanModeParams,
    },
}

#[doc = r" Request initiated from the server and sent to the client."]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ServerRequest {
    #[doc = r" Execute a dynamic tool call on the client."]
    #[serde(rename = "item/tool/call")]
    DynamicToolCall {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: DynamicToolCallParams,
    },
    #[doc = r" Ask the user one or more multiple-choice questions interactively."]
    #[serde(rename = "item/ask_user_question")]
    AskUserQuestion {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: AskUserQuestionParams,
    },
}

impl ServerRequest {
    pub fn id(&self) -> &RequestId {
        match self {
            Self::DynamicToolCall { request_id, .. } => request_id,
            Self::AskUserQuestion { request_id, .. } => request_id,
        }
    }
}
