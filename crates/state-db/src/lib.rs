#![deny(clippy::unwrap_used, clippy::expect_used)]

mod error;
mod handle;

pub use error::StateDbError;
pub use handle::{StateDbHandle, ThreadRow, ThreadSpawnEdgeRow};
