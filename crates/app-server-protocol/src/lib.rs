mod common;

pub use common::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[derive(
    Debug, Clone, PartialEq, PartialOrd, Ord, Deserialize, Serialize, Hash, Eq, JsonSchema,
)]
pub struct RequestId(pub usize);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct JSONRPCErrorError {
    pub code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    pub message: String,
}

pub enum ClientCommand {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JSONRPCErrorError>>,
    },
}
