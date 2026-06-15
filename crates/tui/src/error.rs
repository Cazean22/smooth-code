#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("cazean-tui requires a TTY")]
    TtyRequired,
    #[error("app-server error: {0}")]
    AppServer(#[from] app_server::AppServerError),
    #[error("app-server request failed: {0}")]
    JsonRpc(#[source] Box<app_server_protocol::JsonRpcError>),
    #[error("failed to decode app-server response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("terminal I/O error: {0}")]
    Terminal(#[from] std::io::Error),
    #[error("telemetry error: {0}")]
    Telemetry(String),
    #[error("invalid TUI config: {0}")]
    Config(String),
}

pub type TuiResult<T> = Result<T, TuiError>;

impl From<app_server_protocol::JsonRpcError> for TuiError {
    fn from(value: app_server_protocol::JsonRpcError) -> Self {
        Self::JsonRpc(Box::new(value))
    }
}
