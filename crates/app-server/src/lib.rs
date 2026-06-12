#![deny(clippy::unwrap_used, clippy::expect_used)]

mod core_message_processor;
mod error;
mod error_code;
pub mod in_process;
mod message_processor;
mod outgoing_message;

/// Tests that touch the process working directory (the state DB and rollouts
/// live under the cwd) must serialize on one crate-wide lock — per-module
/// locks do not exclude each other.
#[cfg(test)]
pub(crate) fn cwd_test_lock() -> &'static tokio::sync::Mutex<()> {
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    &CWD_LOCK
}

pub use error::{AppServerError, AppServerResult};

use app_server_protocol::JsonRpcError;
pub type ClientRequestResult = std::result::Result<serde_json::Value, JsonRpcError>;
