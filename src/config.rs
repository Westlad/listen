use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dotenvy::Error as DotenvError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub openclaw: OpenClawConfig,
    pub openai: OpenAiConfig,
    pub audio: AudioConfig,
    pub speech: SpeechConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenClawConfig {
    pub gateway_url: String,
    pub gateway_token: String,
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub session_filter: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key: String,
    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
    #[serde(default = "default_transcription_model")]
    pub transcription_model: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default)]
    pub input_device: String,
    #[serde(default = "default_sample_rate_hz")]
    pub sample_rate_hz: u32,
    #[serde(default = "default_channel_count")]
    pub channel_count: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechConfig {
    #[serde(default = "default_level_threshold")]
    pub level_threshold: f32,
    #[serde(default = "default_start_window_ms")]
    pub start_window_ms: u64,
    #[serde(default = "default_pre_roll_ms")]
    pub pre_roll_ms: u64,
    #[serde(default = "default_trailing_silence_ms")]
    pub trailing_silence_ms: u64,
    #[serde(default = "default_min_utterance_ms")]
    pub min_utterance_ms: u64,
    #[serde(default = "default_max_utterance_ms")]
    pub max_utterance_ms: u64,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_transcript_log_path")]
    pub transcript_log_path: String,
}

impl AppConfig {
    pub fn load(explicit_path: Option<&Path>) -> Result<Self> {
        load_dotenv()?;

        let path = resolve_config_path(explicit_path);
        let mut config = if let Some(path) = path {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            toml::from_str::<AppConfig>(&raw)
                .with_context(|| format!("failed to parse config file {}", path.display()))?
        } else {
            Self::default()
        };

        config.apply_env_overrides();
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(value) = env::var("OPENCLAW_GATEWAY_URL") {
            self.openclaw.gateway_url = value;
        }
        if let Ok(value) = env::var("OPENCLAW_GATEWAY_TOKEN") {
            self.openclaw.gateway_token = value;
        }
        if let Ok(value) = env::var("OPENCLAW_SESSION_KEY") {
            self.openclaw.session_key = value;
        }
        if let Ok(value) = env::var("OPENCLAW_SESSION_FILTER") {
            self.openclaw.session_filter = value;
        }
        if let Ok(value) = env::var("OPENAI_API_KEY") {
            self.openai.api_key = value;
        }
        if let Ok(value) = env::var("OPENAI_BASE_URL") {
            self.openai.base_url = value;
        }
        if let Ok(value) = env::var("OPENAI_TRANSCRIPTION_MODEL") {
            self.openai.transcription_model = value;
        }
        if let Ok(value) = env::var("OPENAI_TRANSCRIPTION_LANGUAGE") {
            self.openai.language = Some(value);
        }
        if let Ok(value) = env::var("OPENAI_TRANSCRIPTION_PROMPT") {
            self.openai.prompt = Some(value);
        }
        if let Ok(value) = env::var("AUDIO_INPUT_DEVICE") {
            self.audio.input_device = value;
        }
        if let Ok(value) = env::var("OPENCLAW_LISTEN_LOG_PATH") {
            self.logging.transcript_log_path = value;
        }
    }
}

fn load_dotenv() -> Result<()> {
    match dotenvy::from_filename(".env") {
        Ok(path) => {
            tracing::info!("loaded environment overrides from {}", path.display());
            Ok(())
        }
        Err(DotenvError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to load .env file"),
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            openclaw: OpenClawConfig {
                gateway_url: "ws://127.0.0.1:18789".to_string(),
                gateway_token: String::new(),
                session_key: String::new(),
                session_filter: String::new(),
            },
            openai: OpenAiConfig {
                api_key: String::new(),
                base_url: default_openai_base_url(),
                transcription_model: default_transcription_model(),
                language: None,
                prompt: None,
            },
            audio: AudioConfig {
                input_device: String::new(),
                sample_rate_hz: default_sample_rate_hz(),
                channel_count: default_channel_count(),
            },
            speech: SpeechConfig {
                level_threshold: default_level_threshold(),
                start_window_ms: default_start_window_ms(),
                pre_roll_ms: default_pre_roll_ms(),
                trailing_silence_ms: default_trailing_silence_ms(),
                min_utterance_ms: default_min_utterance_ms(),
                max_utterance_ms: default_max_utterance_ms(),
                cooldown_ms: default_cooldown_ms(),
            },
            logging: LoggingConfig::default(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            transcript_log_path: default_transcript_log_path(),
        }
    }
}

fn resolve_config_path(explicit_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit_path {
        return Some(path.to_path_buf());
    }

    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("openclaw-listen").join("config.toml");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_transcription_model() -> String {
    "whisper-1".to_string()
}

fn default_transcript_log_path() -> String {
    "/var/log/openclaw-listen.log".to_string()
}

const fn default_sample_rate_hz() -> u32 {
    16_000
}

const fn default_channel_count() -> u16 {
    1
}

const fn default_level_threshold() -> f32 {
    0.015
}

const fn default_start_window_ms() -> u64 {
    120
}

const fn default_pre_roll_ms() -> u64 {
    300
}

const fn default_trailing_silence_ms() -> u64 {
    900
}

const fn default_min_utterance_ms() -> u64 {
    500
}

const fn default_max_utterance_ms() -> u64 {
    20_000
}

const fn default_cooldown_ms() -> u64 {
    400
}
