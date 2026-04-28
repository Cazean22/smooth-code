use anyhow::Result;

use smooth_tui::run;

mod telemetry;

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = telemetry::init()?;
    run().await
}
