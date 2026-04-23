use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "openclaw-listen")]
#[command(about = "Listen for microphone input, transcribe it, and send it to OpenClaw")]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the long-lived daemon that listens to the microphone and sends transcripts.
    Daemon,
    /// List local input devices available through CPAL.
    Devices,
    /// Show current OpenClaw session connectivity settings.
    Sessions,
    /// Show recent messages from the target OpenClaw session.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Send a typed text message to the target OpenClaw session.
    Send {
        #[arg(long)]
        text: String,
    },
    /// Capture one utterance, transcribe it, and optionally send it.
    Test {
        #[arg(long, default_value_t = false)]
        send: bool,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
