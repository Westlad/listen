use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use rand_core::OsRng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::config::OpenClawConfig;

const PROTOCOL_VERSION: u32 = 3;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_ID: &str = "cli";
const CLIENT_MODE: &str = "cli";
const DEVICE_FAMILY: &str = "server";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct OpenClawGatewayClient {
    config: OpenClawConfig,
}

impl OpenClawGatewayClient {
    pub fn new(config: OpenClawConfig) -> Self {
        Self { config }
    }

    pub fn gateway_url(&self) -> &str {
        &self.config.gateway_url
    }

    pub fn session_key(&self) -> Option<&str> {
        if self.config.session_key.is_empty() {
            None
        } else {
            Some(&self.config.session_key)
        }
    }

    pub fn session_filter(&self) -> Option<&str> {
        if self.config.session_filter.is_empty() {
            None
        } else {
            Some(&self.config.session_filter)
        }
    }

    pub fn describe_connectivity(&self) -> Result<()> {
        if self.config.gateway_url.is_empty() {
            bail!("openclaw gateway url is empty");
        }

        tracing::info!("OpenClaw gateway client configured");
        Ok(())
    }

    pub async fn connect(&self) -> Result<GatewayConnection> {
        GatewayConnection::connect(self.config.clone()).await
    }
}

pub struct GatewayConnection {
    outbound_tx: mpsc::Sender<Message>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>>,
    request_counter: AtomicU64,
}

impl GatewayConnection {
    async fn connect(config: OpenClawConfig) -> Result<Self> {
        Self::connect_internal(config, true).await
    }

    async fn connect_internal(config: OpenClawConfig, allow_auto_approve: bool) -> Result<Self> {
        let device_identity = DeviceIdentity::load_or_create()?;
        let (stream, _) = connect_async(&config.gateway_url)
            .await
            .with_context(|| format!("failed to connect to {}", config.gateway_url))?;

        let (mut write, mut read) = stream.split();

        let challenge = read
            .next()
            .await
            .ok_or_else(|| anyhow!("gateway closed before handshake"))?
            .context("failed to read connect challenge")?;
        let challenge_payload = parse_json_message(challenge)?;
        let challenge_nonce = challenge_payload
            .get("payload")
            .and_then(|value| value.get("nonce"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let signed_at_ms = now_unix_ms()?;
        let signature_payload = device_identity.build_signature_payload(SignatureParams {
            client_id: CLIENT_ID,
            client_mode: CLIENT_MODE,
            role: "operator",
            scopes: &["operator.read", "operator.write"],
            signed_at_ms,
            token: if config.gateway_token.trim().is_empty() {
                None
            } else {
                Some(config.gateway_token.as_str())
            },
            nonce: &challenge_nonce,
            platform: std::env::consts::OS,
            device_family: DEVICE_FAMILY,
        });

        let mut auth = serde_json::Map::new();
        if !config.gateway_token.trim().is_empty() {
            auth.insert(
                "token".to_string(),
                Value::String(config.gateway_token.clone()),
            );
        }
        if let Some(device_token) = device_identity.device_token.clone() {
            auth.insert("deviceToken".to_string(), Value::String(device_token));
        }

        let connect_request = json!({
            "type": "req",
            "id": "connect-1",
            "method": "connect",
            "params": {
                "minProtocol": PROTOCOL_VERSION,
                "maxProtocol": PROTOCOL_VERSION,
                "client": {
                    "id": CLIENT_ID,
                    "version": env!("CARGO_PKG_VERSION"),
                    "platform": std::env::consts::OS,
                    "mode": CLIENT_MODE,
                    "deviceFamily": DEVICE_FAMILY,
                    "displayName": "openclaw-listen"
                },
                "role": "operator",
                "scopes": ["operator.read", "operator.write"],
                "caps": [],
                "commands": [],
                "permissions": {},
                "auth": auth,
                "locale": "en-GB",
                "userAgent": format!("openclaw-listen/{}", env!("CARGO_PKG_VERSION")),
                "device": {
                    "id": device_identity.device_id,
                    "publicKey": device_identity.public_key_base64url,
                    "signature": device_identity.sign(&signature_payload)?,
                    "signedAt": signed_at_ms,
                    "nonce": challenge_nonce
                }
            }
        });

        send_json(&mut write, &connect_request).await?;

        let hello_response = read
            .next()
            .await
            .ok_or_else(|| anyhow!("gateway closed during handshake"))?
            .context("failed to read connect response")?;
        let hello_payload = parse_json_message(hello_response)?;

        let ok = hello_payload
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !ok {
            if allow_auto_approve {
                if let Some(request_id) = extract_pairing_request_id(&hello_payload) {
                    tracing::warn!(
                        "device pairing required for request {request_id}; attempting automatic approval"
                    );
                    if let Err(error) = auto_approve_pairing_request(&config, &request_id).await {
                        bail!(
                            "device pairing required (requestId={request_id}); automatic approval failed: {error}"
                        );
                    }
                    return Box::pin(Self::connect_internal(config, false)).await;
                }
            }
            bail!(
                "gateway handshake failed: {}",
                hello_payload
                    .get("error")
                    .cloned()
                    .unwrap_or_else(|| Value::String("unknown error".to_string()))
            );
        }

        if let Some(device_token) = hello_payload
            .get("payload")
            .and_then(|payload| payload.get("auth"))
            .and_then(|auth| auth.get("deviceToken"))
            .and_then(Value::as_str)
        {
            device_identity.persist_device_token(device_token)?;
        }

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(64);
        let pending = Arc::new(Mutex::new(
            HashMap::<String, oneshot::Sender<RpcResponse>>::new(),
        ));

        let writer_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                if let Err(error) = write.send(message).await {
                    tracing::warn!("gateway writer stopped: {error}");
                    break;
                }
            }

            let mut pending = writer_pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(RpcResponse {
                    ok: false,
                    payload: None,
                    error: Some(Value::String("gateway writer stopped".to_string())),
                });
            }
        });

        let read_pending = pending.clone();
        tokio::spawn(async move {
            pump_gateway_reader(read, read_pending).await;
        });

        tracing::info!("connected to OpenClaw Gateway");

        Ok(Self {
            outbound_tx,
            pending,
            request_counter: AtomicU64::new(1),
        })
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let payload = self.call("sessions.list", json!({})).await?;
        Ok(extract_session_summaries(&payload))
    }

    pub async fn send_message(
        &self,
        session_key: &str,
        message: &str,
        deliver: bool,
    ) -> Result<SendMessageAck> {
        let trimmed = message.trim();
        if trimmed.is_empty() {
            bail!("cannot send an empty message");
        }

        let payload = self
            .call(
                "chat.send",
                json!({
                    "sessionKey": session_key,
                    "message": trimmed,
                    "idempotencyKey": self.next_idempotency_key()?,
                    "deliver": deliver,
                }),
            )
            .await?;

        Ok(SendMessageAck {
            status: payload
                .get("status")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            run_id: payload
                .get("runId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            message_seq: payload.get("messageSeq").and_then(Value::as_u64),
        })
    }

    pub async fn fetch_session_messages(
        &self,
        session_key: &str,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        let payload = self
            .call(
                "sessions.get",
                json!({
                    "key": session_key,
                    "limit": limit,
                }),
            )
            .await?;

        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(messages.iter().map(session_message_from_value).collect())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = format!(
            "req-{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let request = json!({
            "type": "req",
            "id": id,
            "method": method,
            "params": params
        });

        self.outbound_tx
            .send(Message::Text(request.to_string().into()))
            .await
            .with_context(|| format!("failed to send gateway request {method}"))?;

        let response = timeout(CALL_TIMEOUT, rx)
            .await
            .with_context(|| format!("gateway request timed out for {method}"))?
            .with_context(|| format!("gateway request channel closed for {method}"))?;

        if response.ok {
            Ok(response.payload.unwrap_or(Value::Null))
        } else {
            bail!(
                "gateway request failed for {method}: {}",
                response
                    .error
                    .unwrap_or_else(|| Value::String("unknown error".to_string()))
            );
        }
    }

    fn next_idempotency_key(&self) -> Result<String> {
        Ok(format!(
            "openclaw-listen-{}-{}",
            now_unix_ms()?,
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

#[derive(Debug, Clone)]
pub struct SendMessageAck {
    pub status: Option<String>,
    pub run_id: Option<String>,
    pub message_seq: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SessionMessage {
    pub role: Option<String>,
    pub text: Option<String>,
    pub id: Option<String>,
    pub seq: Option<u64>,
    pub created_at: Option<String>,
}

impl SessionMessage {
    pub fn fingerprint(&self) -> String {
        if let Some(id) = &self.id {
            return format!("id:{id}");
        }

        if let Some(seq) = self.seq {
            return format!("seq:{seq}");
        }

        format!(
            "role:{}|at:{}|text:{}",
            self.role.as_deref().unwrap_or_default(),
            self.created_at.as_deref().unwrap_or_default(),
            self.text.as_deref().unwrap_or_default()
        )
    }
}

async fn auto_approve_pairing_request(config: &OpenClawConfig, request_id: &str) -> Result<()> {
    if config.gateway_token.trim().is_empty() {
        bail!("cannot auto-approve pairing without a gateway token");
    }

    let (stream, _) = connect_async(&config.gateway_url).await.with_context(|| {
        format!(
            "failed to connect to {} for pairing approval",
            config.gateway_url
        )
    })?;
    let (mut write, mut read) = stream.split();

    let challenge = read
        .next()
        .await
        .ok_or_else(|| anyhow!("gateway closed before pairing approval handshake"))?
        .context("failed to read pairing approval challenge")?;
    let _challenge_payload = parse_json_message(challenge)?;

    let connect_request = json!({
        "type": "req",
        "id": "connect-approve-1",
        "method": "connect",
        "params": {
            "minProtocol": PROTOCOL_VERSION,
            "maxProtocol": PROTOCOL_VERSION,
            "client": {
                "id": "gateway-client",
                "version": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS,
                "mode": "backend",
                "deviceFamily": DEVICE_FAMILY,
                "displayName": "openclaw-listen-pairing"
            },
            "role": "operator",
            "scopes": [
                "operator.admin",
                "operator.pairing",
                "operator.read",
                "operator.write",
                "operator.talk.secrets"
            ],
            "caps": [],
            "commands": [],
            "permissions": {},
            "auth": {
                "token": config.gateway_token
            },
            "locale": "en-GB",
            "userAgent": format!("openclaw-listen/{}", env!("CARGO_PKG_VERSION"))
        }
    });

    send_json(&mut write, &connect_request).await?;
    let hello_response = read
        .next()
        .await
        .ok_or_else(|| anyhow!("gateway closed during pairing approval handshake"))?
        .context("failed to read pairing approval connect response")?;
    let hello_payload = parse_json_message(hello_response)?;
    if !hello_payload
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "pairing approval handshake failed: {}",
            hello_payload
                .get("error")
                .cloned()
                .unwrap_or_else(|| Value::String("unknown error".to_string()))
        );
    }

    let approve_request = json!({
        "type": "req",
        "id": "approve-1",
        "method": "device.pair.approve",
        "params": {
            "requestId": request_id
        }
    });
    send_json(&mut write, &approve_request).await?;

    let approve_response = read
        .next()
        .await
        .ok_or_else(|| anyhow!("gateway closed during device pairing approval"))?
        .context("failed to read device pairing approval response")?;
    let approve_payload = parse_json_message(approve_response)?;
    if !approve_payload
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "device pairing approval failed: {}",
            approve_payload
                .get("error")
                .cloned()
                .unwrap_or_else(|| Value::String("unknown error".to_string()))
        );
    }

    tracing::info!("device pairing approved for request {request_id}");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub key: String,
    pub title: Option<String>,
}

#[derive(Debug)]
struct RpcResponse {
    ok: bool,
    payload: Option<Value>,
    error: Option<Value>,
}

async fn pump_gateway_reader(
    mut read: futures_util::stream::SplitStream<WsStream>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>>,
) {
    while let Some(frame) = read.next().await {
        match frame {
            Ok(message) => match parse_json_message(message) {
                Ok(value) => {
                    let message_type = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match message_type {
                        "res" => {
                            let id = value
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let response = RpcResponse {
                                ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
                                payload: value.get("payload").cloned(),
                                error: value.get("error").cloned(),
                            };

                            if let Some(tx) = pending.lock().await.remove(&id) {
                                let _ = tx.send(response);
                            }
                        }
                        "event" => {
                            let name = value
                                .get("event")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            tracing::debug!("ignoring gateway event {name}");
                        }
                        other => tracing::debug!("ignoring gateway frame type {other}"),
                    }
                }
                Err(error) => tracing::warn!("failed to parse gateway message: {error}"),
            },
            Err(error) => {
                tracing::warn!("gateway reader stopped: {error}");
                break;
            }
        }
    }

    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(RpcResponse {
            ok: false,
            payload: None,
            error: Some(Value::String("gateway reader stopped".to_string())),
        });
    }
}

async fn send_json(
    write: &mut futures_util::stream::SplitSink<WsStream, Message>,
    payload: &Value,
) -> Result<()> {
    write
        .send(Message::Text(payload.to_string().into()))
        .await
        .context("failed to send websocket message")
}

fn parse_json_message(message: Message) -> Result<Value> {
    match message {
        Message::Text(text) => {
            serde_json::from_str(&text).context("failed to decode text websocket payload")
        }
        Message::Binary(bytes) => {
            serde_json::from_slice(&bytes).context("failed to decode binary websocket payload")
        }
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {
            bail!("received non-JSON websocket control frame")
        }
    }
}

fn extract_session_summaries(value: &Value) -> Vec<SessionSummary> {
    let mut summaries = Vec::new();
    collect_session_summaries(value, &mut summaries);

    let mut seen = HashSet::new();
    summaries
        .into_iter()
        .filter(|summary| seen.insert(summary.key.clone()))
        .collect()
}

fn collect_session_summaries(value: &Value, output: &mut Vec<SessionSummary>) {
    match value {
        Value::Object(map) => {
            if let Some(key) = map
                .get("sessionKey")
                .or_else(|| map.get("key"))
                .and_then(Value::as_str)
            {
                let title = map
                    .get("title")
                    .or_else(|| map.get("name"))
                    .or_else(|| map.get("label"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                output.push(SessionSummary {
                    key: key.to_string(),
                    title,
                });
            }

            for value in map.values() {
                collect_session_summaries(value, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_session_summaries(item, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn session_message_from_value(message: &Value) -> SessionMessage {
    SessionMessage {
        role: message
            .get("role")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        text: extract_text_from_value(message),
        id: message
            .get("id")
            .or_else(|| message.get("messageId"))
            .or_else(|| message.get("message_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        seq: message
            .get("seq")
            .or_else(|| message.get("sequence"))
            .or_else(|| message.get("messageSeq"))
            .or_else(|| message.get("message_seq"))
            .and_then(Value::as_u64),
        created_at: message
            .get("createdAt")
            .or_else(|| message.get("created_at"))
            .or_else(|| message.get("timestamp"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

fn extract_text_from_value(value: &Value) -> Option<String> {
    let Value::Object(map) = value else {
        return None;
    };

    for key in ["text", "content", "body", "message", "value"] {
        if let Some(text) = map.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    if let Some(Value::Array(parts)) = map.get("content") {
        let mut collected = Vec::new();
        for part in parts {
            if let Value::Object(part_map) = part {
                for key in ["text", "content", "value"] {
                    if let Some(text) = part_map.get(key).and_then(Value::as_str) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            collected.push(trimmed.to_string());
                        }
                    }
                }
            }
        }

        if !collected.is_empty() {
            return Some(collected.join("\n"));
        }
    }

    None
}

fn extract_pairing_request_id(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("details"))
        .and_then(|details| details.get("requestId"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("payload")
                .and_then(|payload| payload.get("requestId"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

struct DeviceIdentity {
    signing_key: SigningKey,
    device_id: String,
    public_key_base64url: String,
    device_token: Option<String>,
    storage_path: PathBuf,
}

impl DeviceIdentity {
    fn load_or_create() -> Result<Self> {
        let primary_path = device_identity_path()?;
        if primary_path.exists() {
            let stored = read_stored_device_identity(&primary_path)?;
            if stored.device_token.is_some() {
                return Self::from_stored(primary_path, stored);
            }
        }

        let speak_path = sibling_speak_device_identity_path()?;
        if speak_path.exists() {
            let stored = read_stored_device_identity(&speak_path)?;
            if stored.device_token.is_some() {
                tracing::info!(
                    "reusing paired device identity from {}",
                    speak_path.display()
                );
                return Self::from_stored(speak_path, stored);
            }
        }

        if primary_path.exists() {
            let stored = read_stored_device_identity(&primary_path)?;
            return Self::from_stored(primary_path, stored);
        }

        let signing_key = SigningKey::generate(&mut OsRng);
        let stored = StoredDeviceIdentity {
            secret_key_base64url: URL_SAFE_NO_PAD.encode(signing_key.to_bytes()),
            private_key_hex: String::new(),
            device_token: None,
        };
        persist_device_identity(&primary_path, &stored)?;
        Self::from_stored(primary_path, stored)
    }

    fn persist_device_token(&self, device_token: &str) -> Result<()> {
        let mut stored = read_stored_device_identity(&self.storage_path)?;
        stored.device_token = Some(device_token.to_string());
        persist_device_identity(&self.storage_path, &stored)
    }

    fn from_stored(storage_path: PathBuf, stored: StoredDeviceIdentity) -> Result<Self> {
        let secret_key = if !stored.secret_key_base64url.trim().is_empty() {
            URL_SAFE_NO_PAD
                .decode(stored.secret_key_base64url.as_bytes())
                .context("failed to decode device secret key")?
        } else {
            hex::decode(stored.private_key_hex.trim())
                .context("failed to decode legacy device secret key")?
        };
        let secret_key: [u8; 32] = secret_key
            .try_into()
            .map_err(|_| anyhow!("device secret key must be 32 bytes"))?;
        let signing_key = SigningKey::from_bytes(&secret_key);
        let verifying_key = signing_key.verifying_key();
        let public_key_raw = verifying_key.to_bytes();
        let public_key_base64url = URL_SAFE_NO_PAD.encode(public_key_raw);
        let device_id = hex::encode(Sha256::digest(public_key_raw));

        Ok(Self {
            signing_key,
            device_id,
            public_key_base64url,
            device_token: stored.device_token,
            storage_path,
        })
    }

    fn build_signature_payload(&self, params: SignatureParams<'_>) -> String {
        let scopes = params.scopes.join(",");
        let token = params.token.unwrap_or_default();
        [
            "v3".to_string(),
            self.device_id.clone(),
            params.client_id.to_string(),
            params.client_mode.to_string(),
            params.role.to_string(),
            scopes,
            params.signed_at_ms.to_string(),
            token.to_string(),
            params.nonce.to_string(),
            params.platform.to_string(),
            params.device_family.to_string(),
        ]
        .join("|")
    }

    fn sign(&self, payload: &str) -> Result<String> {
        let signature: Signature = self.signing_key.sign(payload.as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredDeviceIdentity {
    #[serde(default)]
    secret_key_base64url: String,
    #[serde(default)]
    private_key_hex: String,
    #[serde(default)]
    device_token: Option<String>,
}

struct SignatureParams<'a> {
    client_id: &'a str,
    client_mode: &'a str,
    role: &'a str,
    scopes: &'a [&'a str],
    signed_at_ms: u64,
    token: Option<&'a str>,
    nonce: &'a str,
    platform: &'a str,
    device_family: &'a str,
}

fn persist_device_identity(path: &PathBuf, stored: &StoredDeviceIdentity) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let raw =
        serde_json::to_string_pretty(stored).context("failed to serialize device identity")?;
    fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
}

fn device_identity_path() -> Result<PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("config directory is unavailable"))?;
    Ok(config_dir
        .join("openclaw-listen")
        .join("device-identity.json"))
}

fn sibling_speak_device_identity_path() -> Result<PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("config directory is unavailable"))?;
    Ok(config_dir
        .join("openclaw-speak")
        .join("device-identity.json"))
}

fn read_stored_device_identity(path: &PathBuf) -> Result<StoredDeviceIdentity> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn now_unix_ms() -> Result<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before unix epoch")?;
    Ok(now.as_millis() as u64)
}
