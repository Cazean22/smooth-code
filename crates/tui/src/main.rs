#![deny(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;

use smooth_tui::run;

mod telemetry;

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = telemetry::init()?;
    Ok(run().await?)
}
