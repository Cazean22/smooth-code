use anyhow::Result;

use smooth_tui::run;

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}
