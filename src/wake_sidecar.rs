#[cfg(feature = "audio-cpal")]
use std::io::{BufRead, BufReader, ErrorKind, Write};
#[cfg(feature = "audio-cpal")]
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(feature = "audio-cpal")]
use std::sync::mpsc::{self, Receiver};
#[cfg(feature = "audio-cpal")]
use std::thread;

#[cfg(feature = "audio-cpal")]
use anyhow::{Context, Result, bail};

#[cfg(feature = "audio-cpal")]
use crate::config::WakeConfig;

#[derive(Debug, Clone)]
pub struct WakeDetection {
    pub model: Option<String>,
    pub score: Option<f32>,
}

#[cfg(feature = "audio-cpal")]
pub struct WakeSidecar {
    child: Child,
    stdin: ChildStdin,
    detection_rx: Receiver<WakeDetection>,
}

#[cfg(feature = "audio-cpal")]
impl WakeSidecar {
    pub fn start(config: &WakeConfig) -> Result<Self> {
        validate_config(config)?;

        tracing::info!(
            "starting wake sidecar using {} model {}",
            config.engine,
            config.model_path
        );

        let mut child = Command::new(&config.sidecar_command)
            .arg(&config.sidecar_script)
            .arg("--model-path")
            .arg(&config.model_path)
            .arg("--threshold")
            .arg(config.threshold.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start wake sidecar {}", config.sidecar_command))?;

        let stdin = child
            .stdin
            .take()
            .context("wake sidecar stdin was not captured")?;
        let stdout = child
            .stdout
            .take()
            .context("wake sidecar stdout was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("wake sidecar stderr was not captured")?;

        let (detection_tx, detection_rx) = mpsc::channel();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if let Some(detection) = parse_detection_line(&line) {
                            let _ = detection_tx.send(detection);
                            break;
                        }
                        tracing::debug!("wake sidecar stdout: {line}");
                    }
                    Err(error) => {
                        tracing::warn!("failed to read wake sidecar stdout: {error}");
                        break;
                    }
                }
            }
        });

        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => tracing::info!("wake sidecar: {line}"),
                    Err(error) => {
                        tracing::warn!("failed to read wake sidecar stderr: {error}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            detection_rx,
        })
    }

    pub fn write_pcm_i16(&mut self, samples: &[i16]) -> Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }

        match self.stdin.write_all(&bytes) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::BrokenPipe => {
                bail!("wake sidecar stopped while receiving audio")
            }
            Err(error) => Err(error).context("failed to write audio to wake sidecar"),
        }
    }

    pub fn try_detection(&self) -> Option<WakeDetection> {
        self.detection_rx.try_recv().ok()
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(feature = "audio-cpal")]
impl Drop for WakeSidecar {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(feature = "audio-cpal")]
fn validate_config(config: &WakeConfig) -> Result<()> {
    if config.engine != "openwakeword" {
        bail!("unsupported wake engine '{}'", config.engine);
    }

    if config.model_path.trim().is_empty() {
        bail!("wake word is enabled but wake.model_path is empty");
    }

    if !(0.0..=1.0).contains(&config.threshold) {
        bail!("wake.threshold must be between 0.0 and 1.0");
    }

    Ok(())
}

#[cfg(feature = "audio-cpal")]
fn parse_detection_line(line: &str) -> Option<WakeDetection> {
    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("wake") || trimmed.eq_ignore_ascii_case("wake_detected") {
        return Some(WakeDetection {
            model: None,
            score: None,
        });
    }

    let value = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
    if value
        .get("event")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|event| event == "wake" || event == "wake_detected")
    {
        return Some(WakeDetection {
            model: value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
            score: value
                .get("score")
                .and_then(serde_json::Value::as_f64)
                .map(|score| score as f32),
        });
    }

    None
}
