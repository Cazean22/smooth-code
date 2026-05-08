mod core_message_processor;
mod error_code;
pub mod in_process;
mod message_processor;
mod outgoing_message;

use app_server_protocol::JSONRPCErrorError;
pub type ClientRequestResult = std::result::Result<serde_json::Value, JSONRPCErrorError>;
