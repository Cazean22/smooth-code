#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("{message}")]
    InvalidArguments { message: String },
    #[error("{message}")]
    PathResolution { message: String },
    #[error("{message}")]
    Io { message: String },
    #[error("{message}")]
    Client { message: String },
    #[error("{message}")]
    Unsupported { message: String },
    #[error("{message}")]
    Other { message: String },
}

pub type ToolResult<T> = Result<T, ToolError>;

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::Other {
            message: message.into(),
        }
    }

    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::InvalidArguments {
            message: message.into(),
        }
    }

    pub fn path_resolution(message: impl Into<String>) -> Self {
        Self::PathResolution {
            message: message.into(),
        }
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::Io {
            message: message.into(),
        }
    }

    pub fn client(message: impl Into<String>) -> Self {
        Self::Client {
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported {
            message: message.into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidArguments { .. } => "invalid_arguments",
            Self::PathResolution { .. } => "path_resolution",
            Self::Io { .. } => "io",
            Self::Client { .. } => "client",
            Self::Unsupported { .. } => "unsupported",
            Self::Other { .. } => "tool_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ToolError;

    #[test]
    fn tool_errors_expose_structured_kind_and_readable_message() {
        let error = ToolError::invalid_arguments("file_path must not be empty");

        assert_eq!(error.kind(), "invalid_arguments");
        assert_eq!(error.to_string(), "file_path must not be empty");
    }
}
