#![deny(clippy::unwrap_used, clippy::expect_used)]

mod core_message_processor;
mod error;
mod error_code;
pub mod in_process;
mod message_processor;
mod outgoing_message;

pub use error::{AppServerError, AppServerResult};

use app_server_protocol::JsonRpcError;
pub type ClientRequestResult = std::result::Result<serde_json::Value, JsonRpcError>;
