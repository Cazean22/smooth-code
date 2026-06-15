#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use anyhow::{Context, Result};

use cazean_tui::run;

mod telemetry;

#[tokio::main]
async fn main() -> Result<()> {
    // Configuration is loaded before telemetry so logging settings can come
    // from it; any ConfigError prints to stderr via Display (no logger yet).
    let workspace_root =
        std::env::current_dir().context("failed to determine current directory")?;
    let config = Arc::new(cazean_config::load(&workspace_root)?);
    let _telemetry = telemetry::init(&config)?;
    Ok(run(config).await?)
}
