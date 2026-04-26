use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RequestId;

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
    pub message: String,
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

#[doc = r" Request from the client to the server."]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ClientRequest {
    TurnStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: TurnStartParams,
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
}
