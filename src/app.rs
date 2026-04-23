use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::signal;
use tracing_subscriber::EnvFilter;

use crate::audio::{AudioInput, CapturedAudio};
use crate::cli::{Cli, Commands};
use crate::config::{AppConfig, OpenClawConfig};
use crate::conversation_log::ConversationLog;
use crate::gateway::{GatewayConnection, OpenClawGatewayClient, SessionMessage, SessionSummary};
use crate::transcription::OpenAiTranscriptionClient;

const RESPONSE_HISTORY_LIMIT: usize = 50;
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const RESPONSE_WAIT_TIMEOUT: Duration = Duration::from_secs(45);

pub async fn run() -> Result<()> {
    init_tracing();

    let cli = Cli::parse_args();
    let config = AppConfig::load(cli.config.as_deref())?;

    match cli.command {
        Commands::Daemon => run_daemon(config).await,
        Commands::Devices => list_devices(config).await,
        Commands::Sessions => list_sessions(config).await,
        Commands::History { limit } => show_history(config, limit).await,
        Commands::Send { text } => send_text(config, &text).await,
        Commands::Test { send } => test_capture(config, send).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run_daemon(config: AppConfig) -> Result<()> {
    let gateway = OpenClawGatewayClient::new(config.openclaw.clone());
    let transcriber = OpenAiTranscriptionClient::new(config.openai.clone());
    let audio = AudioInput::new(config.audio.clone(), config.speech.clone());
    let conversation_log = ConversationLog::open(&config.logging.transcript_log_path)?;

    tracing::info!("starting openclaw-listen daemon");
    tracing::info!("gateway_url = {}", gateway.gateway_url());
    tracing::info!("session_key = {:?}", gateway.session_key());
    tracing::info!("session_filter = {:?}", gateway.session_filter());
    tracing::info!(
        "transcript log path = {}",
        conversation_log.path().display()
    );

    gateway.describe_connectivity()?;
    transcriber.describe_configuration()?;
    audio.describe_configuration()?;

    let connection = gateway.connect().await?;
    let target = resolve_target_session(&connection, &config.openclaw).await?;
    tracing::info!(
        "microphone transcripts will be sent to session {}",
        describe_session(&target)
    );

    loop {
        let audio_for_capture = audio.clone();
        let mut capture_task =
            tokio::task::spawn_blocking(move || audio_for_capture.capture_utterance());

        let captured = tokio::select! {
            _ = signal::ctrl_c() => {
                capture_task.abort();
                tracing::info!("received Ctrl+C, stopping daemon");
                break;
            }
            result = &mut capture_task => result??,
        };

        if let Some(transcript) = transcribe_capture(&transcriber, captured).await? {
            let target = resolve_target_session(&connection, &config.openclaw).await?;
            send_transcript(&connection, &target, &transcript, Some(&conversation_log)).await?;
        }
    }

    Ok(())
}

async fn list_devices(config: AppConfig) -> Result<()> {
    let audio = AudioInput::new(config.audio, config.speech);
    let devices = audio.list_input_devices()?;

    if devices.is_empty() {
        println!("No input devices found.");
    } else {
        for device in devices {
            println!("{device}");
        }
    }

    Ok(())
}

async fn list_sessions(config: AppConfig) -> Result<()> {
    let gateway = OpenClawGatewayClient::new(config.openclaw);
    gateway.describe_connectivity()?;
    let connection = gateway.connect().await?;
    let sessions = connection.list_sessions().await?;

    println!("Gateway URL: {}", gateway.gateway_url());
    println!("Session key: {:?}", gateway.session_key());
    println!("Session filter: {:?}", gateway.session_filter());

    if sessions.is_empty() {
        println!("No sessions found.");
    } else {
        for session in sessions {
            println!("{}", describe_session(&session));
        }
    }

    Ok(())
}

async fn show_history(config: AppConfig, limit: usize) -> Result<()> {
    let gateway = OpenClawGatewayClient::new(config.openclaw.clone());
    gateway.describe_connectivity()?;
    let connection = gateway.connect().await?;
    let target = resolve_target_session(&connection, &config.openclaw).await?;
    let messages = connection
        .fetch_session_messages(&target.key, limit)
        .await?;

    println!("Session: {}", describe_session(&target));
    println!("Messages shown: {}", messages.len());

    for message in messages {
        let role = message.role.unwrap_or_else(|| "unknown".to_string());
        match message.text {
            Some(text) if !text.is_empty() => println!("[{role}] {text}"),
            _ => println!("[{role}] <non-text message>"),
        }
    }

    Ok(())
}

async fn send_text(config: AppConfig, text: &str) -> Result<()> {
    let gateway = OpenClawGatewayClient::new(config.openclaw.clone());
    gateway.describe_connectivity()?;
    let connection = gateway.connect().await?;
    let target = resolve_target_session(&connection, &config.openclaw).await?;
    send_transcript(&connection, &target, text, None).await?;
    println!("Sent to {}", describe_session(&target));
    Ok(())
}

async fn test_capture(config: AppConfig, send: bool) -> Result<()> {
    let gateway = OpenClawGatewayClient::new(config.openclaw.clone());
    let transcriber = OpenAiTranscriptionClient::new(config.openai.clone());
    let audio = AudioInput::new(config.audio.clone(), config.speech.clone());

    transcriber.describe_configuration()?;
    audio.describe_configuration()?;

    let captured = tokio::task::spawn_blocking(move || audio.capture_utterance()).await??;
    let Some(transcript) = transcribe_capture(&transcriber, captured).await? else {
        println!("Transcript was empty.");
        return Ok(());
    };

    println!("Transcript: {}", transcript);

    if send {
        gateway.describe_connectivity()?;
        let connection = gateway.connect().await?;
        let target = resolve_target_session(&connection, &config.openclaw).await?;
        let conversation_log = ConversationLog::open(&config.logging.transcript_log_path)?;
        send_transcript(&connection, &target, &transcript, Some(&conversation_log)).await?;
    }

    Ok(())
}

async fn transcribe_capture(
    transcriber: &OpenAiTranscriptionClient,
    captured: CapturedAudio,
) -> Result<Option<String>> {
    tracing::info!(
        "captured {:.2}s of microphone audio at {} Hz; requesting transcription",
        captured.duration.as_secs_f64(),
        captured.sample_rate_hz
    );

    let transcript = transcriber.transcribe_wav(captured.wav_bytes).await?;
    let text = transcript.text.trim().to_string();
    if text.is_empty() {
        tracing::info!("transcription returned empty text; skipping send");
        return Ok(None);
    }

    tracing::info!("transcript: {}", text);
    Ok(Some(text))
}

async fn send_transcript(
    connection: &GatewayConnection,
    target: &SessionSummary,
    transcript: &str,
    conversation_log: Option<&ConversationLog>,
) -> Result<()> {
    let known_messages = if conversation_log.is_some() {
        known_message_fingerprints(connection, &target.key).await?
    } else {
        HashSet::new()
    };

    if let Some(conversation_log) = conversation_log {
        conversation_log.append(&target.key, "user", transcript)?;
    }

    let ack = connection
        .send_message(&target.key, transcript, true)
        .await?;
    tracing::info!(
        "sent transcript to {} with status={:?}, run_id={:?}, message_seq={:?}",
        describe_session(target),
        ack.status,
        ack.run_id,
        ack.message_seq
    );

    if let Some(conversation_log) = conversation_log {
        log_agent_responses(connection, target, conversation_log, known_messages).await?;
    }

    Ok(())
}

async fn known_message_fingerprints(
    connection: &GatewayConnection,
    session_key: &str,
) -> Result<HashSet<String>> {
    let messages = connection
        .fetch_session_messages(session_key, RESPONSE_HISTORY_LIMIT)
        .await?;
    Ok(messages.iter().map(SessionMessage::fingerprint).collect())
}

async fn log_agent_responses(
    connection: &GatewayConnection,
    target: &SessionSummary,
    conversation_log: &ConversationLog,
    mut known_messages: HashSet<String>,
) -> Result<()> {
    let deadline = Instant::now() + RESPONSE_WAIT_TIMEOUT;

    loop {
        tokio::time::sleep(RESPONSE_POLL_INTERVAL).await;

        let messages = connection
            .fetch_session_messages(&target.key, RESPONSE_HISTORY_LIMIT)
            .await?;
        let mut logged_response = false;

        for message in messages {
            let fingerprint = message.fingerprint();
            if !known_messages.insert(fingerprint) {
                continue;
            }

            if !is_assistant_message(&message) {
                continue;
            }

            let Some(text) = message.text.as_deref() else {
                continue;
            };
            if text.is_empty() {
                continue;
            }

            conversation_log.append(&target.key, "assistant", text)?;
            tracing::info!("logged agent response for {}", describe_session(target));
            logged_response = true;
        }

        if logged_response {
            return Ok(());
        }

        if Instant::now() >= deadline {
            tracing::warn!(
                "timed out waiting to log an agent response for {}",
                describe_session(target)
            );
            return Ok(());
        }
    }
}

fn is_assistant_message(message: &SessionMessage) -> bool {
    message.role.as_deref() == Some("assistant")
}

async fn resolve_target_session(
    connection: &GatewayConnection,
    config: &OpenClawConfig,
) -> Result<SessionSummary> {
    if let Some(explicit) = non_empty(&config.session_key) {
        return Ok(SessionSummary {
            key: explicit.to_string(),
            title: None,
        });
    }

    let sessions = connection.list_sessions().await?;
    if sessions.is_empty() {
        bail!("no OpenClaw sessions were found");
    }

    if let Some(filter) = non_empty(&config.session_filter) {
        let matches: Vec<_> = sessions
            .into_iter()
            .filter(|session| session_matches_filter(filter, session))
            .collect();

        return match matches.len() {
            0 => bail!("no sessions matched OPENCLAW_SESSION_FILTER='{filter}'"),
            1 => Ok(matches.into_iter().next().expect("single match")),
            count => bail!(
                "OPENCLAW_SESSION_FILTER='{filter}' matched {count} sessions; set OPENCLAW_SESSION_KEY for an exact target"
            ),
        };
    }

    if let Some(main) = sessions
        .iter()
        .find(|session| is_main_session(&session.key))
    {
        return Ok(main.clone());
    }

    if sessions.len() == 1 {
        return Ok(sessions.into_iter().next().expect("single session"));
    }

    bail!(
        "multiple sessions are available; set OPENCLAW_SESSION_KEY or OPENCLAW_SESSION_FILTER to choose a target"
    )
}

fn session_matches_filter(filter: &str, session: &SessionSummary) -> bool {
    let filter = filter.to_ascii_lowercase();
    if session.key.to_ascii_lowercase().contains(&filter) {
        return true;
    }

    session
        .title
        .as_deref()
        .map(|title| title.to_ascii_lowercase().contains(&filter))
        .unwrap_or(false)
}

fn is_main_session(key: &str) -> bool {
    key.eq_ignore_ascii_case("main") || key.to_ascii_lowercase().ends_with(":main")
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn describe_session(session: &SessionSummary) -> String {
    match session.title.as_deref() {
        Some(title) if !title.is_empty() => format!("{} ({title})", session.key),
        _ => session.key.clone(),
    }
}

#[allow(dead_code)]
fn _cooldown_duration(config: &AppConfig) -> Duration {
    Duration::from_millis(config.speech.cooldown_ms)
}
