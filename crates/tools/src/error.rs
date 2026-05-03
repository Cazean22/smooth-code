#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolFailure(String);

impl ToolFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
