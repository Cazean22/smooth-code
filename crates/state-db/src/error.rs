#[derive(Debug, thiserror::Error)]
pub enum StateDbError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}
