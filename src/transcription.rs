use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

use crate::config::OpenAiConfig;

#[derive(Debug, Clone)]
pub struct OpenAiTranscriptionClient {
    config: OpenAiConfig,
    http: Client,
}

impl OpenAiTranscriptionClient {
    pub fn new(config: OpenAiConfig) -> Self {
        Self {
            config,
            http: Client::new(),
        }
    }

    pub fn describe_configuration(&self) -> Result<()> {
        if self.config.api_key.trim().is_empty() {
            tracing::warn!("OpenAI api_key is not configured yet");
        }

        tracing::info!(
            "OpenAI transcription client configured with model {} at {}",
            self.config.transcription_model,
            self.config.base_url
        );
        Ok(())
    }

    pub async fn transcribe_wav(&self, wav_bytes: Vec<u8>) -> Result<Transcript> {
        if wav_bytes.is_empty() {
            bail!("cannot transcribe an empty audio buffer");
        }

        if self.config.api_key.trim().is_empty() {
            bail!("OPENAI_API_KEY is not configured");
        }

        let url = format!(
            "{}/audio/transcriptions",
            self.config.base_url.trim_end_matches('/')
        );

        let audio_part = Part::bytes(wav_bytes)
            .file_name("microphone.wav")
            .mime_str("audio/wav")
            .context("failed to build multipart audio payload")?;

        let mut form = Form::new()
            .part("file", audio_part)
            .text("model", self.config.transcription_model.clone());

        if let Some(language) = self
            .config
            .language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            form = form.text("language", language.to_string());
        }

        if let Some(prompt) = self
            .config
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            form = form.text("prompt", prompt.to_string());
        }

        let response = self
            .http
            .post(url)
            .bearer_auth(&self.config.api_key)
            .multipart(form)
            .send()
            .await
            .context("failed to call OpenAI transcription endpoint")?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unavailable>".to_string());
            bail!("OpenAI transcription request failed with {status}: {error_body}");
        }

        let body = response
            .json::<TranscriptionResponse>()
            .await
            .context("failed to decode OpenAI transcription response")?;

        Ok(Transcript {
            text: body.text.trim().to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}
