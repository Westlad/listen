mod app;
mod audio;
mod cli;
mod config;
mod conversation_log;
mod gateway;
mod transcription;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    app::run().await
}
