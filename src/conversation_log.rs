use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::json;

#[derive(Clone)]
pub struct ConversationLog {
    path: PathBuf,
    writer: Arc<Mutex<BufWriter<File>>>,
}

impl ConversationLog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open transcript log {}", path.display()))?;

        Ok(Self {
            path,
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, session_key: &str, role: &str, text: &str) -> Result<()> {
        let entry = json!({
            "timestamp_unix_ms": unix_time_ms()?,
            "session_key": session_key,
            "role": role,
            "text": text,
        });

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("transcript log writer lock poisoned"))?;
        serde_json::to_writer(&mut *writer, &entry)
            .context("failed to encode transcript log entry")?;
        writeln!(&mut *writer).context("failed to write transcript log newline")?;
        writer.flush().context("failed to flush transcript log")?;
        Ok(())
    }
}

fn unix_time_ms() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis())
}
