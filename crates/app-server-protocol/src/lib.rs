#![deny(clippy::unwrap_used, clippy::expect_used)]

mod common;

use cazean_protocol::ErrorInfo;
pub use common::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::sync::oneshot;

#[derive(
    Debug, Clone, PartialEq, PartialOrd, Ord, Deserialize, Serialize, Hash, Eq, JsonSchema,
)]
pub struct RequestId(pub usize);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct JsonRpcError {
    pub code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorInfo>,
    pub message: String,
}

impl JsonRpcError {
    pub fn new(code: i64, error: ErrorInfo) -> Self {
        Self {
            code,
            message: error.message.clone(),
            data: Some(error),
        }
    }

    pub fn message_only(code: i64, kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, ErrorInfo::new(kind, message))
    }
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JsonRpcError {}

pub enum ClientCommand {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JsonRpcError>>,
    },
    ServerRequestResponse {
        request_id: RequestId,
        result: serde_json::Value,
    },
    ServerRequestError {
        request_id: RequestId,
        error: JsonRpcError,
    },
}
