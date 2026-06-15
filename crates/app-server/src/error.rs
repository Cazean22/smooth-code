use app_server_protocol::JsonRpcError;
use cazean_protocol::ErrorInfo;

use crate::error_code::{INTERNAL_ERROR_CODE, INVALID_PARAMS_ERROR_CODE, SERVER_ERROR_CODE};

#[derive(Debug, thiserror::Error)]
pub enum AppServerError {
    #[error("invalid thread id: {message}")]
    InvalidThreadId { message: String },
    #[error("request channel closed: {message}")]
    RequestChannel { message: String },
    #[error("failed to spawn task `{task_name}`: {source}")]
    TaskSpawn {
        task_name: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("core error: {0}")]
    Core(#[from] cazean_core::CoreError),
    #[error("{0}")]
    Internal(String),
}

pub type AppServerResult<T> = Result<T, AppServerError>;

impl AppServerError {
    pub fn invalid_thread_id(error: impl ToString) -> Self {
        Self::InvalidThreadId {
            message: error.to_string(),
        }
    }

    pub fn to_json_rpc_error(&self) -> JsonRpcError {
        let code = match self {
            Self::InvalidThreadId { .. } => INVALID_PARAMS_ERROR_CODE,
            Self::Serialization(_) => INTERNAL_ERROR_CODE,
            Self::RequestChannel { .. }
            | Self::TaskSpawn { .. }
            | Self::Core(_)
            | Self::Internal(_) => SERVER_ERROR_CODE,
        };
        JsonRpcError::new(code, self.to_error_info())
    }

    pub fn to_error_info(&self) -> ErrorInfo {
        match self {
            Self::InvalidThreadId { .. } => {
                ErrorInfo::new("invalid_thread_id", self.to_string()).with_source("app-server")
            }
            Self::RequestChannel { .. } => {
                ErrorInfo::new("request_channel", self.to_string()).with_source("app-server")
            }
            Self::TaskSpawn { .. } => {
                ErrorInfo::new("task_spawn", self.to_string()).with_source("app-server")
            }
            Self::Serialization(_) => {
                ErrorInfo::new("serialization", self.to_string()).with_source("app-server")
            }
            Self::Core(err) => err.to_error_info(),
            Self::Internal(_) => {
                ErrorInfo::new("internal", self.to_string()).with_source("app-server")
            }
        }
    }
}

impl From<AppServerError> for JsonRpcError {
    fn from(value: AppServerError) -> Self {
        value.to_json_rpc_error()
    }
}

#[cfg(test)]
mod tests {
    use cazean_core::CoreError;
    use cazean_protocol::ThreadId;

    use super::AppServerError;
    use crate::error_code::SERVER_ERROR_CODE;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn core_errors_map_to_json_rpc_with_structured_error_info() -> TestResult {
        let thread_id = ThreadId::new();
        let error = AppServerError::Core(CoreError::UnknownThread { thread_id });
        let json_rpc = error.to_json_rpc_error();

        assert_eq!(json_rpc.code, SERVER_ERROR_CODE);
        let info = json_rpc.data.as_ref().ok_or("missing error data")?;
        assert_eq!(info.kind, "unknown_thread");
        assert_eq!(info.source.as_deref(), Some("cazean-core"));
        assert_eq!(json_rpc.message, info.message);
        assert!(info.message.contains(&thread_id.to_string()));
        Ok(())
    }
}
