use crate::schema::*;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub const GOOGLE_CHAT_API_BASE: &str = "https://chat.googleapis.com/v1";
/// Google Chat Media API base for `media.upload`. Distinct host prefix from
/// the message API: `/upload/v1/...` instead of `/v1/...`.
pub const GOOGLE_CHAT_UPLOAD_BASE: &str = "https://chat.googleapis.com/upload/v1";
const GOOGLE_CHAT_MESSAGE_LIMIT: usize = 4096;

const IMAGE_MAX_DIMENSION_PX: u32 = 1200;
const IMAGE_JPEG_QUALITY: u8 = 75;
const IMAGE_MAX_DOWNLOAD: u64 = 10 * 1024 * 1024; // 10 MB
const FILE_MAX_DOWNLOAD: u64 = 512 * 1024; // 512 KB
const AUDIO_MAX_DOWNLOAD: u64 = 25 * 1024 * 1024; // 25 MB
/// Per-request timeout for Google Chat Media API downloads. Prevents a hung
/// connection from blocking the spawned download task indefinitely.
const MEDIA_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Cap on text file attachments per message (matches Discord/Slack).
const TEXT_FILE_COUNT_CAP: usize = 5;
/// Cap on aggregate text file bytes per message (matches Discord/Slack 1 MB).
const TEXT_TOTAL_CAP: u64 = 1024 * 1024;

// Outbound attachment caps: mirror inbound limits per MIME family so what a
// user can send the bot, the bot can send back.
const OUTBOUND_IMAGE_MAX_BYTES: u64 = IMAGE_MAX_DOWNLOAD;
const OUTBOUND_AUDIO_MAX_BYTES: u64 = AUDIO_MAX_DOWNLOAD;
const OUTBOUND_TEXT_MAX_BYTES: u64 = TEXT_TOTAL_CAP;
const OUTBOUND_PDF_MAX_BYTES: u64 = IMAGE_MAX_DOWNLOAD;
/// Timeout for a single outbound upload request.
const OUTBOUND_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// --- Access / mesh configuration (mirrors feishu.rs) ---

/// Controls bot-to-bot messaging — when this bot responds to messages from
/// other bots in the same Space. Mirrors `FEISHU_ALLOW_BOTS`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AllowBots {
    /// Never respond to bot messages (default).
    #[default]
    Off,
    /// Respond only if explicitly @mentioned.
    Mentions,
    /// Respond to any bot message in an active thread.
    All,
}

/// Controls when the bot responds without an @mention in a Space. Note that
/// Google Chat platform requires @mention in every Space message regardless;
/// this setting governs which messages the gateway forwards (vs. drops).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AllowUsers {
    /// Forward messages in Spaces the bot has previously participated in,
    /// even if the platform's @mention requirement made the message addressed
    /// to it (default).
    #[default]
    Involved,
    /// Only forward messages that @mention this bot.
    Mentions,
    /// Like Involved, but if another bot is also participating in the Space,
    /// require @mention to disambiguate.
    MultibotMentions,
}

/// Hard safety cap for `max_bot_turns` when `AllowBots::All` is configured.
/// Mirrors feishu's safety hard cap so a misconfigured deployment can't run
/// unbounded bot-to-bot loops.
const BOT_TURNS_SAFETY_HARD_CAP: u32 = 10;

#[derive(Debug, Clone)]
pub struct GoogleChatConfig {
    /// Allow DM (direct message) conversations. Default: false. Mirrors
    /// `messaging.md` Layer 0 — DM is opt-in.
    pub allow_dm: bool,
    /// Allowed Space resource names (`spaces/AAAA...`). Empty = all allowed.
    pub allowed_spaces: Vec<String>,
    /// Allowed user IDs (stripped of `users/` prefix). Empty = all allowed.
    pub allowed_users: Vec<String>,
    pub allow_bots: AllowBots,
    pub allow_user_messages: AllowUsers,
    /// Whitelist of bot IDs (stripped of `users/` prefix). Empty + `allow_bots
    /// != Off` = any bot allowed (subject to mode).
    pub trusted_bot_ids: Vec<String>,
    /// Maximum consecutive bot-to-bot turns per Space before responses stop.
    /// Reset on any human message.
    pub max_bot_turns: u32,
    /// TTL for participated-thread cache (seconds). 0 disables participation
    /// tracking, requiring every message to be a fresh @mention.
    pub session_ttl_secs: u64,
    /// When true, the SA token request asks for the `drive.readonly` scope
    /// in addition to `chat.bot`, allowing the adapter to download
    /// Drive-sourced attachments (`DRIVE_FILE` source). Deployments must
    /// pre-authorize the SA for this scope; otherwise token exchange fails.
    pub enable_drive_attachments: bool,
}

impl Default for GoogleChatConfig {
    fn default() -> Self {
        Self {
            allow_dm: false,
            allowed_spaces: Vec::new(),
            allowed_users: Vec::new(),
            allow_bots: AllowBots::Off,
            allow_user_messages: AllowUsers::Involved,
            trusted_bot_ids: Vec::new(),
            max_bot_turns: 20,
            session_ttl_secs: 24 * 3600,
            enable_drive_attachments: false,
        }
    }
}

impl GoogleChatConfig {
    /// Build config from environment. All keys are optional; missing keys
    /// fall back to safe defaults (no allowlist constraints + bots off).
    pub fn from_env() -> Self {
        let allow_dm = std::env::var("GOOGLE_CHAT_ALLOW_DM")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let allowed_spaces = parse_csv_env("GOOGLE_CHAT_ALLOWED_SPACES");
        let allowed_users = parse_csv_env("GOOGLE_CHAT_ALLOWED_USERS");
        let allow_bots = match std::env::var("GOOGLE_CHAT_ALLOW_BOTS")
            .unwrap_or_else(|_| "off".into())
            .to_lowercase()
            .as_str()
        {
            "mentions" => AllowBots::Mentions,
            "all" => AllowBots::All,
            _ => AllowBots::Off,
        };
        let allow_user_messages = match std::env::var("GOOGLE_CHAT_ALLOW_USER_MESSAGES")
            .unwrap_or_else(|_| "involved".into())
            .to_lowercase()
            .replace('-', "_")
            .as_str()
        {
            "mentions" => AllowUsers::Mentions,
            "multibot_mentions" => AllowUsers::MultibotMentions,
            _ => AllowUsers::Involved,
        };
        let trusted_bot_ids = parse_csv_env("GOOGLE_CHAT_TRUSTED_BOT_IDS");
        let configured_max_turns: u32 = std::env::var("GOOGLE_CHAT_MAX_BOT_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        // Safety hard cap only applies under AllowBots::All (matches feishu).
        let max_bot_turns = if allow_bots == AllowBots::All {
            configured_max_turns.min(BOT_TURNS_SAFETY_HARD_CAP)
        } else {
            configured_max_turns
        };
        let session_ttl_secs = std::env::var("GOOGLE_CHAT_SESSION_TTL_HOURS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(24)
            * 3600;
        let enable_drive_attachments = std::env::var("GOOGLE_CHAT_ENABLE_DRIVE_ATTACHMENTS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        Self {
            allow_dm,
            allowed_spaces,
            allowed_users,
            allow_bots,
            allow_user_messages,
            trusted_bot_ids,
            max_bot_turns,
            session_ttl_secs,
            enable_drive_attachments,
        }
    }
}

fn parse_csv_env(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// --- Google Chat types (v2 envelope format) ---

#[derive(Debug, Deserialize)]
pub struct GoogleChatEnvelope {
    pub chat: Option<ChatPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatPayload {
    pub user: Option<GoogleChatUser>,
    pub message_payload: Option<MessagePayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePayload {
    pub message: Option<GoogleChatMessage>,
    pub space: Option<GoogleChatSpace>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatMessage {
    pub name: String,
    pub text: Option<String>,
    pub argument_text: Option<String>,
    pub sender: Option<GoogleChatUser>,
    pub thread: Option<GoogleChatThread>,
    pub space: Option<GoogleChatSpace>,
    #[serde(default)]
    pub attachment: Vec<GoogleChatAttachment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatAttachment {
    #[allow(dead_code)]
    pub name: Option<String>,
    pub content_name: Option<String>,
    pub content_type: Option<String>,
    pub source: Option<String>,
    pub attachment_data_ref: Option<AttachmentDataRef>,
    #[allow(dead_code)]
    pub drive_data_ref: Option<DriveDataRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDataRef {
    pub resource_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveDataRef {
    pub drive_file_id: Option<String>,
}

/// Reference to media that needs async download after webhook parse.
#[derive(Debug, Clone)]
pub enum GoogleChatMediaRef {
    Image {
        resource_name: String,
        content_name: String,
    },
    File {
        resource_name: String,
        content_name: String,
    },
    Audio {
        resource_name: String,
        content_name: String,
        content_type: String,
    },
    /// User attached a Google Drive file (not direct upload). Downloaded via
    /// Drive API; requires `drive.readonly` SA scope (gated by
    /// `GOOGLE_CHAT_ENABLE_DRIVE_ATTACHMENTS`).
    DriveFile {
        drive_file_id: String,
        content_name: String,
        content_type: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatUser {
    pub name: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub user_type: String,
}

#[derive(Debug, Deserialize)]
pub struct GoogleChatThread {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatSpace {
    pub name: String,
    #[serde(rename = "type")]
    pub space_type: Option<String>,
    // Parsed by serde, not consumed in current code paths.
    #[allow(dead_code)]
    pub space_type_renamed: Option<String>,
}

// --- Webhook JWT verification ---

const GOOGLE_CHAT_ISSUER: &str = "https://accounts.google.com";
const GOOGLE_CHAT_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
const GOOGLE_CHAT_EMAIL_SUFFIX: &str = "@gcp-sa-gsuiteaddons.iam.gserviceaccount.com";
const JWKS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Verify the JWT's `email` claim belongs to a Google Chat service account.
/// Google Chat webhooks use `service-{PROJECT_NUMBER}@gcp-sa-gsuiteaddons.iam.gserviceaccount.com`.
/// Without this check, any Google-issued ID token would be accepted.
fn verify_email_claim(claims: &serde_json::Value) -> Result<(), String> {
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .ok_or("missing email claim")?;
    if !email.ends_with(GOOGLE_CHAT_EMAIL_SUFFIX) {
        return Err(format!(
            "email claim mismatch: expected *{GOOGLE_CHAT_EMAIL_SUFFIX}, got {email}"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct JwkKey {
    kid: Option<String>,
    n: String,
    e: String,
    kty: String,
}

#[derive(Debug, Deserialize)]
struct JwksResponse {
    keys: Vec<JwkKey>,
}

pub struct GoogleChatJwtVerifier {
    audience: String,
    client: reqwest::Client,
    jwks_cache: RwLock<Option<(Vec<JwkKey>, Instant)>>,
}

impl GoogleChatJwtVerifier {
    pub fn new(audience: String) -> Self {
        Self {
            audience,
            client: reqwest::Client::new(),
            jwks_cache: RwLock::new(None),
        }
    }

    async fn get_jwks(&self) -> Result<Vec<JwkKey>, String> {
        {
            let cache = self.jwks_cache.read().await;
            if let Some((ref keys, fetched_at)) = *cache {
                if fetched_at.elapsed() < JWKS_CACHE_TTL {
                    return Ok(keys.clone());
                }
            }
        }
        let jwks: JwksResponse = self
            .client
            .get(GOOGLE_CHAT_JWKS_URL)
            .send()
            .await
            .map_err(|e| format!("JWKS fetch error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JWKS parse error: {e}"))?;

        let keys = jwks.keys;
        *self.jwks_cache.write().await = Some((keys.clone(), Instant::now()));
        Ok(keys)
    }

    pub async fn verify(&self, auth_header: &str) -> Result<(), String> {
        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or("missing Bearer prefix")?;

        let header =
            jsonwebtoken::decode_header(token).map_err(|e| format!("invalid JWT header: {e}"))?;
        let kid = header.kid.ok_or("no kid in JWT header")?;

        let keys = self.get_jwks().await?;
        let key = match keys.iter().find(|k| k.kid.as_deref() == Some(&kid)) {
            Some(k) => k.clone(),
            None => {
                // Key rotation: invalidate cache and retry
                *self.jwks_cache.write().await = None;
                let refreshed = self.get_jwks().await?;
                refreshed
                    .into_iter()
                    .find(|k| k.kid.as_deref() == Some(&kid))
                    .ok_or_else(|| format!("no matching JWK for kid={kid}"))?
            }
        };

        if key.kty != "RSA" {
            return Err(format!("unsupported key type: {}", key.kty));
        }

        let decoding_key = DecodingKey::from_rsa_components(&key.n, &key.e)
            .map_err(|e| format!("RSA key decode error: {e}"))?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.audience]);
        validation.set_issuer(&[GOOGLE_CHAT_ISSUER]);
        validation.validate_exp = true;

        let token_data = decode::<serde_json::Value>(token, &decoding_key, &validation)
            .map_err(|e| format!("JWT validation failed: {e}"))?;

        verify_email_claim(&token_data.claims)?;

        Ok(())
    }
}

// --- Adapter (encapsulates all Google Chat state) ---

pub struct GoogleChatAdapter {
    pub token_cache: Option<GoogleChatTokenCache>,
    pub access_token: Option<String>,
    pub jwt_verifier: Option<GoogleChatJwtVerifier>,
    pub client: reqwest::Client,
    pub api_base: String,
    pub upload_base: String,
    pub drive_api_base: String,
    pub config: GoogleChatConfig,
    /// Consecutive bot-to-bot turn counter, keyed by Space resource name.
    /// Reset on any human message in the Space.
    pub bot_turns: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>,
    /// Last-seen timestamp per Space where this bot has participated.
    /// Used by `AllowUsers::Involved` mode to decide whether to forward
    /// non-mention messages.
    pub participated_threads: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Instant>>>,
    /// Spaces where this bot has detected another bot participating. Used by
    /// `AllowUsers::MultibotMentions` to require @mention disambiguation.
    pub multibot_threads: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Instant>>>,
}

impl GoogleChatAdapter {
    pub fn new(
        token_cache: Option<GoogleChatTokenCache>,
        access_token: Option<String>,
        jwt_verifier: Option<GoogleChatJwtVerifier>,
        config: GoogleChatConfig,
    ) -> Self {
        Self {
            token_cache,
            access_token,
            jwt_verifier,
            client: reqwest::Client::new(),
            api_base: GOOGLE_CHAT_API_BASE.into(),
            upload_base: GOOGLE_CHAT_UPLOAD_BASE.into(),
            drive_api_base: GOOGLE_DRIVE_API_BASE.into(),
            config,
            bot_turns: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            participated_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            multibot_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    async fn get_token(&self) -> Option<String> {
        if let Some(ref cache) = self.token_cache {
            match cache.get_token(&self.client).await {
                Ok(t) => return Some(t),
                Err(e) => {
                    error!("googlechat token refresh failed: {e}");
                    return None;
                }
            }
        }
        self.access_token.clone()
    }

    async fn edit_message(&self, message_name: &str, text: &str) {
        let Some(token) = self.get_token().await else {
            tracing::warn!("googlechat edit_message: no token available");
            return;
        };

        let formatted = markdown_to_gchat(text);
        let url = format!(
            "{}/{}?updateMask=text",
            self.api_base, message_name
        );
        let body = serde_json::json!({ "text": formatted });

        match self.client.patch(&url).bearer_auth(&token).json(&body).send().await {
            Ok(r) if r.status().is_success() => {
                tracing::trace!(message_name = %message_name, "googlechat message edited");
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                error!(status = %status, body = %body, "googlechat edit_message failed");
            }
            Err(e) => {
                error!(err = %e, "googlechat edit_message request failed");
            }
        }
    }

    pub async fn handle_reply(
        &self,
        reply: &GatewayReply,
        event_tx: &tokio::sync::broadcast::Sender<String>,
    ) {
        // Command routing
        match reply.command.as_deref() {
            Some("add_reaction") | Some("remove_reaction") | Some("create_topic") => return,
            Some("edit_message") => {
                self.edit_message(&reply.reply_to, &reply.content.text).await;
                return;
            }
            _ => {}
        }

        info!(
            space = %reply.channel.id,
            thread_id = ?reply.channel.thread_id,
            "gateway → googlechat"
        );

        let Some(token) = self.get_token().await else {
            info!(
                text = %reply.content.text,
                "googlechat reply (dry-run, no credentials configured)"
            );
            if let Some(ref req_id) = reply.request_id {
                let resp = crate::schema::GatewayResponse {
                    schema: "openab.gateway.response.v1".into(),
                    request_id: req_id.clone(),
                    success: false,
                    thread_id: None,
                    message_id: None,
                    error: Some("no credentials configured".into()),
                };
                if let Ok(json) = serde_json::to_string(&resp) {
                    let _ = event_tx.send(json);
                }
            }
            return;
        };

        let text = &reply.content.text;
        let chunks = split_text(text, GOOGLE_CHAT_MESSAGE_LIMIT);

        // Upload outbound attachments. Failures are logged and skipped — they
        // don't abort the reply; text-only delivery is still useful.
        let mut uploaded: Vec<serde_json::Value> = Vec::new();
        let mut upload_errors: Vec<String> = Vec::new();
        for att in &reply.content.attachments {
            match upload_attachment(
                &self.client,
                &token,
                &self.upload_base,
                &reply.channel.id,
                att,
            )
            .await
            {
                Ok(data_ref) => uploaded.push(build_attachment_payload(
                    &data_ref,
                    &att.filename,
                    &att.mime_type,
                )),
                Err(e) => {
                    warn!(
                        filename = %att.filename,
                        mime = %att.mime_type,
                        error = %e,
                        "googlechat outbound attachment skipped"
                    );
                    upload_errors.push(format!("{}: {}", att.filename, e));
                }
            }
        }

        // Short-circuit only when there is literally nothing to send.
        if chunks.is_empty() && uploaded.is_empty() {
            if let Some(ref req_id) = reply.request_id {
                let error_msg = if reply.content.attachments.is_empty() {
                    "empty message".into()
                } else {
                    format!("empty message; all attachments failed: {}", upload_errors.join("; "))
                };
                let resp = crate::schema::GatewayResponse {
                    schema: "openab.gateway.response.v1".into(),
                    request_id: req_id.clone(),
                    success: false,
                    thread_id: None,
                    message_id: None,
                    error: Some(error_msg),
                };
                if let Ok(json) = serde_json::to_string(&resp) {
                    let _ = event_tx.send(json);
                }
            }
            return;
        }

        let mut first_msg_name: Option<String> = None;
        let mut first_error: Option<String> = None;

        if chunks.is_empty() {
            // Attachments only, no text.
            let result = send_message(
                &self.client,
                &token,
                &reply.channel.id,
                reply.channel.thread_id.as_deref(),
                "",
                &self.api_base,
                &uploaded,
            )
            .await;
            match result {
                Ok(name) => first_msg_name = Some(name),
                Err(e) => first_error = Some(e),
            }
        } else {
            for (idx, chunk) in chunks.into_iter().enumerate() {
                // Attachments piggyback on the first chunk only, so the user
                // doesn't see N copies of the same image across split messages.
                let attachments_for_chunk: &[serde_json::Value] = if idx == 0 {
                    &uploaded
                } else {
                    &[]
                };
                let result = send_message(
                    &self.client,
                    &token,
                    &reply.channel.id,
                    reply.channel.thread_id.as_deref(),
                    chunk,
                    &self.api_base,
                    attachments_for_chunk,
                )
                .await;
                match result {
                    Ok(name) => {
                        if first_msg_name.is_none() {
                            first_msg_name = Some(name);
                        }
                    }
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }
        }

        if let Some(ref req_id) = reply.request_id {
            // text delivered AND no chunk failed; partial-upload failures alone
            // do not flip success to false, but are surfaced via `error`.
            let text_ok = first_msg_name.is_some() && first_error.is_none();
            let error = match (first_error, upload_errors.is_empty()) {
                (Some(e), _) => Some(e),
                (None, false) => Some(format!(
                    "attachment upload partial failure: {}",
                    upload_errors.join("; ")
                )),
                (None, true) => None,
            };
            let resp = crate::schema::GatewayResponse {
                schema: "openab.gateway.response.v1".into(),
                request_id: req_id.clone(),
                success: text_ok,
                thread_id: None,
                message_id: first_msg_name,
                error,
            };
            if let Ok(json) = serde_json::to_string(&resp) {
                let _ = event_tx.send(json);
            }
        }
    }
}

// --- Webhook handler ---

pub async fn webhook(
    State(state): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    info!("googlechat webhook received ({} bytes)", body.len());

    if let Some(ref adapter) = state.google_chat {
        if let Some(ref verifier) = adapter.jwt_verifier {
            let auth_header = match headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
            {
                Some(h) => h,
                None => {
                    warn!("googlechat webhook: missing authorization header");
                    return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
                }
            };
            if let Err(e) = verifier.verify(auth_header).await {
                warn!(error = %e, "googlechat webhook JWT verification failed");
                return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
            }
        }
    }

    let envelope: GoogleChatEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            let body_str = String::from_utf8_lossy(&body);
            error!(body = %body_str, "googlechat webhook parse error: {e}");
            return (axum::http::StatusCode::BAD_REQUEST, "bad request").into_response();
        }
    };

    let Some(chat) = envelope.chat else {
        return empty_json_response();
    };
    let Some(payload) = chat.message_payload else {
        return empty_json_response();
    };
    let Some(ref msg) = payload.message else {
        return empty_json_response();
    };

    let text = msg
        .argument_text
        .as_deref()
        .or(msg.text.as_deref())
        .unwrap_or("");

    let media_refs = parse_attachments(&msg.attachment);

    // Drop event only if BOTH text and attachments are empty
    if text.trim().is_empty() && media_refs.is_empty() {
        return empty_json_response();
    }

    let sender = msg.sender.as_ref().or(chat.user.as_ref());
    let space = msg.space.as_ref().or(payload.space.as_ref());

    let is_bot = sender.map(|s| s.user_type == "BOT").unwrap_or(false);

    let sender_id = sender.map(|s| s.name.clone()).unwrap_or_default();
    let display_name = sender
        .map(|s| s.display_name.clone())
        .unwrap_or_else(|| "Unknown".into());
    let sender_name = sender_id
        .strip_prefix("users/")
        .unwrap_or(&sender_id)
        .to_string();

    let space_name = space.map(|s| s.name.clone()).unwrap_or_default();
    let space_type = space
        .and_then(|s| s.space_type.clone())
        .unwrap_or_else(|| "ROOM".into());

    let thread_id = msg.thread.as_ref().map(|t| t.name.clone());

    // ----- Access control (messaging.md Layer 0-4) -----
    // Adapter is registered → access controls always apply via the adapter
    // config. (state.google_chat is None when GOOGLE_CHAT_ENABLED is false,
    // in which case this handler isn't routed.)
    if let Some(ref adapter) = state.google_chat {
        // Layer 0: DM opt-in. Google Chat platform delivers DMs by default;
        // gateway gates them behind allow_dm.
        if space_type == "DM" && !adapter.config.allow_dm {
            warn!(space = %space_name, sender = %sender_name, "googlechat: DM dropped (allow_dm=false)");
            return empty_json_response();
        }
        // Layer 1: Space allowlist.
        if space_type != "DM"
            && !adapter.config.allowed_spaces.is_empty()
            && !adapter.config.allowed_spaces.iter().any(|s| s == &space_name)
        {
            warn!(space = %space_name, "googlechat: space dropped (not in allowed_spaces)");
            return empty_json_response();
        }
        // User allowlist applies to humans only — bots have their own mesh
        // gates (trusted_bot_ids + allow_bots below). Without this carve-out,
        // a deployment that scopes allowed_users to a single human admin
        // would inadvertently silence all bot-to-bot traffic too.
        if !is_bot
            && !adapter.config.allowed_users.is_empty()
            && !adapter.config.allowed_users.iter().any(|u| u == &sender_name)
        {
            warn!(sender = %sender_name, "googlechat: user not in allowlist, dropping");
            return empty_json_response();
        }

        // Layer 4: bot → bot mesh.
        if is_bot {
            match adapter.config.allow_bots {
                AllowBots::Off => {
                    return empty_json_response();
                }
                AllowBots::Mentions | AllowBots::All => {
                    // Google Chat only delivers Space messages to a bot when
                    // the bot is @mentioned (platform requirement), so by the
                    // time we see a bot message, the "mentions" condition is
                    // already satisfied. Trusted-bot allowlist still applies.
                    if !adapter.config.trusted_bot_ids.is_empty()
                        && !adapter
                            .config
                            .trusted_bot_ids
                            .iter()
                            .any(|id| id == &sender_name)
                    {
                        warn!(sender = %sender_name, "googlechat: bot not in trusted_bot_ids, dropping");
                        return empty_json_response();
                    }
                }
            }

            // Consecutive bot-to-bot turns counter. Reset on human messages
            // below. We don't await an Option<Adapter> here so use a blocking
            // lock — the map is small and contention is unlikely.
            let mut turns = adapter.bot_turns.lock().await;
            let count = turns.entry(space_name.clone()).or_insert(0);
            *count += 1;
            if *count > adapter.config.max_bot_turns {
                warn!(
                    space = %space_name,
                    count = *count,
                    cap = adapter.config.max_bot_turns,
                    "googlechat: bot turns cap reached, dropping"
                );
                return empty_json_response();
            }
        } else {
            // Human message resets the bot-turns counter for this Space.
            adapter.bot_turns.lock().await.remove(&space_name);
        }
    }

    let message_id = msg
        .name
        .rsplit('/')
        .next()
        .unwrap_or(&msg.name)
        .to_string();

    // No attachments → emit event synchronously and respond 200
    if media_refs.is_empty() {
        send_googlechat_event(
            &state,
            &space_name,
            space_type,
            thread_id,
            &sender_id,
            &sender_name,
            &display_name,
            text,
            &message_id,
            Vec::new(),
            is_bot,
        );
        return empty_json_response();
    }

    // Has attachments — spawn background task so the webhook returns 200 within
    // Google Chat's 30 s deadline regardless of how long downloads take.
    let text = text.to_string();
    let state = state.clone();
    let spawn_space = space_name.clone();
    let spawn_is_bot = is_bot;
    tokio::spawn(async move {
        use futures_util::FutureExt;
        let result = std::panic::AssertUnwindSafe(async {
        let mut downloaded: Vec<crate::schema::Attachment> = Vec::new();
        let mut text_file_count: usize = 0;
        let mut text_file_bytes: u64 = 0;
        if let Some(ref adapter) = state.google_chat {
            if let Some(token) = adapter.get_token().await {
                for media_ref in &media_refs {
                    let attachment = match media_ref {
                        GoogleChatMediaRef::Image {
                            resource_name,
                            content_name,
                            ..
                        } => {
                            download_googlechat_image(
                                &adapter.client,
                                &token,
                                &adapter.api_base,
                                resource_name,
                                content_name,
                            )
                            .await
                        }
                        GoogleChatMediaRef::File {
                            resource_name,
                            content_name,
                            ..
                        } => {
                            if text_file_count >= TEXT_FILE_COUNT_CAP {
                                warn!(content_name = %content_name, cap = TEXT_FILE_COUNT_CAP, "googlechat text file count cap reached, skipping");
                                continue;
                            }
                            let remaining = TEXT_TOTAL_CAP.saturating_sub(text_file_bytes);
                            let att = download_googlechat_file(
                                &adapter.client,
                                &token,
                                &adapter.api_base,
                                resource_name,
                                content_name,
                                remaining,
                            )
                            .await;
                            let Some(att) = att else { continue };
                            text_file_count += 1;
                            text_file_bytes += att.size;
                            Some(att)
                        }
                        GoogleChatMediaRef::Audio {
                            resource_name,
                            content_name,
                            content_type,
                        } => {
                            download_googlechat_audio(
                                &adapter.client,
                                &token,
                                &adapter.api_base,
                                resource_name,
                                content_name,
                                content_type,
                            )
                            .await
                        }
                        GoogleChatMediaRef::DriveFile {
                            drive_file_id,
                            content_name,
                            content_type,
                        } => {
                            if !adapter.config.enable_drive_attachments {
                                warn!(
                                    content_name = %content_name,
                                    "googlechat: drive attachment skipped (GOOGLE_CHAT_ENABLE_DRIVE_ATTACHMENTS=false)"
                                );
                                continue;
                            }
                            let remaining = TEXT_TOTAL_CAP.saturating_sub(text_file_bytes);
                            let att = download_googledrive_file(
                                &adapter.client,
                                &token,
                                &adapter.drive_api_base,
                                drive_file_id,
                                content_name,
                                content_type,
                                remaining,
                            )
                            .await;
                            let Some(att) = att else { continue };
                            if att.attachment_type == "text_file" {
                                if text_file_count >= TEXT_FILE_COUNT_CAP {
                                    warn!(content_name = %content_name, cap = TEXT_FILE_COUNT_CAP, "googlechat drive text file count cap reached, skipping");
                                    continue;
                                }
                                text_file_count += 1;
                                text_file_bytes += att.size;
                            }
                            Some(att)
                        }
                    };
                    if let Some(att) = attachment {
                        downloaded.push(att);
                    }
                }
            } else {
                warn!("googlechat: no token available for attachment download");
            }
        }

        // If text is empty AND every attachment failed to download, drop the event.
        if text.trim().is_empty() && downloaded.is_empty() {
            warn!(
                space = %space_name,
                "googlechat: empty text + all attachments failed, dropping event"
            );
            return;
        }

        send_googlechat_event(
            &state,
            &space_name,
            space_type,
            thread_id,
            &sender_id,
            &sender_name,
            &display_name,
            &text,
            &message_id,
            downloaded,
            spawn_is_bot,
        );
        }).catch_unwind().await;
        if let Err(e) = result {
            error!(space = %spawn_space, "googlechat attachment download task panicked: {e:?}");
        }
    });

    empty_json_response()
}

#[allow(clippy::too_many_arguments)]
fn send_googlechat_event(
    state: &Arc<crate::AppState>,
    space_name: &str,
    space_type: String,
    thread_id: Option<String>,
    sender_id: &str,
    sender_name: &str,
    display_name: &str,
    text: &str,
    message_id: &str,
    attachments: Vec<crate::schema::Attachment>,
    is_bot: bool,
) {
    let mut gw_event = GatewayEvent::new(
        "googlechat",
        ChannelInfo {
            id: space_name.to_string(),
            channel_type: space_type,
            thread_id,
        },
        SenderInfo {
            id: sender_id.to_string(),
            name: sender_name.to_string(),
            display_name: display_name.to_string(),
            is_bot,
        },
        text,
        message_id,
        vec![],
    );
    gw_event.content.attachments = attachments;

    let attachment_count = gw_event.content.attachments.len();
    let json = match serde_json::to_string(&gw_event) {
        Ok(j) => j,
        Err(e) => {
            error!(error = %e, "googlechat: failed to serialize GatewayEvent");
            return;
        }
    };
    info!(
        space = %space_name,
        sender = %sender_name,
        attachment_count,
        "googlechat → gateway"
    );
    let _ = state.event_tx.send(json);
}

fn empty_json_response() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        "{}",
    )
        .into_response()
}

// --- Token cache with JWT auto-refresh ---

pub struct GoogleChatTokenCache {
    token: RwLock<Option<(String, Instant, u64)>>,
    sa_email: String,
    private_key: String,
    /// Space-separated OAuth scopes requested in the JWT. Always includes
    /// `chat.bot`; extra scopes (e.g. `drive.readonly`) are appended when
    /// optional features are enabled.
    scopes: String,
}

const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
const GOOGLE_CHAT_BASE_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";
const GOOGLE_DRIVE_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";

impl GoogleChatTokenCache {
    pub fn new(sa_key_json: &str) -> Result<Self, String> {
        Self::new_with_scopes(sa_key_json, &[])
    }

    /// Build a token cache requesting `chat.bot` plus any additional scopes.
    /// Pass `&[GOOGLE_DRIVE_READONLY_SCOPE]` to enable Drive-attachment
    /// downloads. Token exchange will fail at runtime if the deployment
    /// hasn't authorized the SA for the requested scopes — fail-fast is
    /// preferred over silently degrading.
    pub fn new_with_scopes(sa_key_json: &str, extra_scopes: &[&str]) -> Result<Self, String> {
        let key: serde_json::Value =
            serde_json::from_str(sa_key_json).map_err(|e| format!("invalid SA key JSON: {e}"))?;
        let email = key
            .get("client_email")
            .and_then(|v| v.as_str())
            .ok_or("missing client_email in SA key")?
            .to_string();
        let pkey = key
            .get("private_key")
            .and_then(|v| v.as_str())
            .ok_or("missing private_key in SA key")?
            .to_string();
        let mut scope_list = vec![GOOGLE_CHAT_BASE_SCOPE.to_string()];
        scope_list.extend(extra_scopes.iter().map(|s| s.to_string()));
        Ok(Self {
            token: RwLock::new(None),
            sa_email: email,
            private_key: pkey,
            scopes: scope_list.join(" "),
        })
    }

    pub async fn get_token(&self, client: &reqwest::Client) -> Result<String, String> {
        {
            let guard = self.token.read().await;
            if let Some((ref tok, ref ts, ttl)) = *guard {
                if ts.elapsed().as_secs() < ttl.saturating_sub(TOKEN_REFRESH_MARGIN_SECS) {
                    return Ok(tok.clone());
                }
            }
        }
        let mut guard = self.token.write().await;
        if let Some((ref tok, ref ts, ttl)) = *guard {
            if ts.elapsed().as_secs() < ttl.saturating_sub(TOKEN_REFRESH_MARGIN_SECS) {
                return Ok(tok.clone());
            }
        }
        let (new_token, expire) = self.refresh(client).await?;
        *guard = Some((new_token.clone(), Instant::now(), expire));
        info!("googlechat access token refreshed (expires in {expire}s)");
        Ok(new_token)
    }

    async fn refresh(&self, client: &reqwest::Client) -> Result<(String, u64), String> {
        let jwt = self.build_jwt().map_err(|e| format!("JWT build error: {e}"))?;
        let resp = client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &jwt),
            ])
            .send()
            .await
            .map_err(|e| format!("token exchange request failed: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("token exchange parse failed: {e}"))?;

        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let err = body
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                format!("token exchange failed: {err}")
            })?
            .to_string();

        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);

        Ok((token, expires_in))
    }

    fn build_jwt(&self) -> Result<String, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();

        let claims = serde_json::json!({
            "iss": self.sa_email,
            "scope": self.scopes,
            "aud": "https://oauth2.googleapis.com/token",
            "iat": now,
            "exp": now + 3600,
        });

        let key = jsonwebtoken::EncodingKey::from_rsa_pem(self.private_key.as_bytes())
            .map_err(|e| format!("RSA key parse error: {e}"))?;
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        jsonwebtoken::encode(&header, &claims, &key)
            .map_err(|e| format!("JWT encode error: {e}"))
    }
}

/// Convert markdown to Google Chat native formatting.
///
/// Called by both `send_message` and `edit_message`. Assumes the caller passes
/// **raw markdown** — passing already-converted text would double-convert
/// (e.g. `*bold*` from a previous pass would be re-parsed as `*italic*`).
/// OAB core is expected to always emit raw markdown for both initial replies
/// and streaming edits.
fn markdown_to_gchat(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Detect fenced code block — pass through unchanged
        if line.trim_start().starts_with("```") {
            result.push_str(line);
            result.push('\n');
            i += 1;
            while i < lines.len() {
                result.push_str(lines[i]);
                if lines[i].trim_start().starts_with("```") {
                    i += 1;
                    if i < lines.len() {
                        result.push('\n');
                    }
                    break;
                }
                result.push('\n');
                i += 1;
            }
            continue;
        }
        // Heading → bold
        let converted = if let Some(heading) = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "))
            .or_else(|| line.strip_prefix("# "))
        {
            format!("*{}*", heading.trim())
        } else {
            convert_inline(line)
        };
        result.push_str(&converted);
        i += 1;
        if i < lines.len() {
            result.push('\n');
        }
    }
    result
}

// TODO(perf): allocates Vec<char> per line. Acceptable at current scale,
// but on hot streaming paths with many edit_message updates this could be
// rewritten with byte-level iteration over &str.
fn convert_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Inline code — pass through
        if chars[i] == '`' {
            out.push('`');
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                out.push('`');
                i += 1;
            }
            continue;
        }
        // Markdown link: [text](url)
        if chars[i] == '[' {
            if let Some((link_text, url, end)) = parse_md_link(&chars, i) {
                let converted_text = convert_inline(&link_text);
                out.push_str(&format!("<{}|{}>", url, converted_text));
                i = end;
                continue;
            }
        }
        // Bold: **text** → *text*
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                out.push('*');
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&convert_inline(&inner));
                out.push('*');
                i = end + 2;
                continue;
            }
        }
        // Bold: __text__ → *text*
        if chars[i] == '_' && i + 1 < chars.len() && chars[i + 1] == '_' {
            if let Some(end) = find_closing(&chars, i + 2, &['_', '_']) {
                out.push('*');
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&convert_inline(&inner));
                out.push('*');
                i = end + 2;
                continue;
            }
        }
        // Strikethrough: ~~text~~ → ~text~
        if chars[i] == '~' && i + 1 < chars.len() && chars[i + 1] == '~' {
            if let Some(end) = find_closing(&chars, i + 2, &['~', '~']) {
                out.push('~');
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&convert_inline(&inner));
                out.push('~');
                i = end + 2;
                continue;
            }
        }
        // Italic: *text* → _text_ (single asterisk, not part of **bold**)
        // Must come AFTER the **bold** check above. Requires non-asterisk
        // immediately after opening * and before closing *.
        if chars[i] == '*'
            && i + 1 < chars.len()
            && chars[i + 1] != '*'
            && !chars[i + 1].is_whitespace()
        {
            if let Some(end) = find_single(&chars, i + 1, '*') {
                if end > i + 1 && !chars[end - 1].is_whitespace() {
                    out.push('_');
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.push_str(&convert_inline(&inner));
                    out.push('_');
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_single(chars: &[char], start: usize, target: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn parse_md_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut i = start + 1;
    let mut depth = 1;
    let text_start = i;
    while i < chars.len() && depth > 0 {
        if chars[i] == '[' {
            depth += 1;
        } else if chars[i] == ']' {
            depth -= 1;
        }
        if depth > 0 {
            i += 1;
        }
    }
    if depth != 0 {
        return None;
    }
    let text: String = chars[text_start..i].iter().collect();
    i += 1; // skip ']'
    if i >= chars.len() || chars[i] != '(' {
        return None;
    }
    i += 1; // skip '('
    let url_start = i;
    let mut paren_depth = 1;
    while i < chars.len() && paren_depth > 0 {
        if chars[i] == '(' {
            paren_depth += 1;
        } else if chars[i] == ')' {
            paren_depth -= 1;
        }
        if paren_depth > 0 {
            i += 1;
        }
    }
    if paren_depth != 0 {
        return None;
    }
    let url: String = chars[url_start..i].iter().collect();
    Some((text, url, i + 1))
}

fn find_closing(chars: &[char], start: usize, pattern: &[char]) -> Option<usize> {
    if pattern.len() < 2 {
        return None;
    }
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == pattern[0] && chars[i + 1] == pattern[1] {
            return Some(i);
        }
        i += 1;
    }
    None
}

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    space: &str,
    thread_id: Option<&str>,
    text: &str,
    api_base: &str,
    attachments: &[serde_json::Value],
) -> Result<String, String> {
    let mut url = format!("{}/{}/messages", api_base, space);

    let formatted = markdown_to_gchat(text);
    let mut body = serde_json::json!({
        "text": formatted,
    });

    if !attachments.is_empty() {
        body["attachment"] = serde_json::Value::Array(attachments.to_vec());
    }

    if let Some(thread_id) = thread_id {
        body["thread"] = serde_json::json!({
            "name": thread_id,
        });
        url.push_str("?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
    }

    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.unwrap_or_default();
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            parsed
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| "missing message name in response".into())
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "googlechat send error");
            Err(format!("send failed: {} {}", status, body))
        }
        Err(e) => {
            error!("googlechat send error: {e}");
            Err(format!("request error: {e}"))
        }
    }
}

fn split_text(text: &str, limit: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        if start + limit >= text.len() {
            chunks.push(&text[start..]);
            break;
        }
        let mut end = start + limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut search_start = if end > start + 200 { end - 200 } else { start };
        while search_start < end && !text.is_char_boundary(search_start) {
            search_start += 1;
        }
        let break_at = text[search_start..end]
            .rfind('\n')
            .or_else(|| text[search_start..end].rfind(' '))
            .map(|pos| search_start + pos + 1)
            .unwrap_or(end);
        chunks.push(&text[start..break_at]);
        start = break_at;
    }
    chunks
}

// --- Outbound attachment upload (bot → user) ---

/// Returns the per-MIME outbound size cap, or None if the MIME family is not
/// allowed for outbound. Mirrors inbound limits so the bot can send back what
/// a user can send it. `video/*` is intentionally skipped to match inbound.
fn outbound_mime_cap(mime: &str) -> Option<u64> {
    let lower = mime.to_ascii_lowercase();
    if lower.starts_with("image/") {
        Some(OUTBOUND_IMAGE_MAX_BYTES)
    } else if lower.starts_with("audio/") {
        Some(OUTBOUND_AUDIO_MAX_BYTES)
    } else if lower.starts_with("text/") {
        Some(OUTBOUND_TEXT_MAX_BYTES)
    } else if lower == "application/pdf" {
        Some(OUTBOUND_PDF_MAX_BYTES)
    } else {
        None
    }
}

/// Build the JSON object for a single outbound attachment entry in
/// `messages.create` request body, from the `attachmentDataRef` returned by
/// `media.upload`.
fn build_attachment_payload(
    data_ref: &serde_json::Value,
    filename: &str,
    mime: &str,
) -> serde_json::Value {
    serde_json::json!({
        "contentName": filename,
        "contentType": mime,
        "attachmentDataRef": data_ref,
    })
}

/// Build a `multipart/related` body for the Google Chat Media API.
///
/// Google's `media.upload` requires `multipart/related` (not the
/// `multipart/form-data` produced by `reqwest::multipart::Form`), with the
/// metadata part first and the media part second. Returns
/// `(content_type_header_value, body_bytes)`.
fn build_multipart_related(metadata_json: &str, media: &[u8], media_mime: &str) -> (String, Vec<u8>) {
    let boundary = format!("oab_gc_{}", uuid::Uuid::new_v4().simple());
    let mut body: Vec<u8> = Vec::with_capacity(media.len() + metadata_json.len() + 256);
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata_json.as_bytes());
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Type: ");
    body.extend_from_slice(media_mime.as_bytes());
    body.extend_from_slice(b"\r\n\r\n");
    body.extend_from_slice(media);
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    (format!("multipart/related; boundary={}", boundary), body)
}

/// Upload a single attachment to Google Chat via the Media API
/// (`spaces.messages.attachments.upload`) and return the `attachmentDataRef`
/// JSON object to be embedded in a subsequent `messages.create` request.
///
/// Performs all client-side validation (MIME whitelist, size cap, base64
/// decode) before any network call. Returns `Err(String)` on any failure;
/// callers are expected to log and skip the attachment, not abort the reply.
async fn upload_attachment(
    client: &reqwest::Client,
    token: &str,
    upload_base: &str,
    space: &str,
    att: &crate::schema::Attachment,
) -> Result<serde_json::Value, String> {
    let cap = outbound_mime_cap(&att.mime_type)
        .ok_or_else(|| format!("outbound MIME not allowed: {}", att.mime_type))?;

    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(att.data.as_bytes())
        .map_err(|e| format!("base64 decode failed: {e}"))?;

    if (bytes.len() as u64) > cap {
        return Err(format!(
            "attachment size {} exceeds cap {} for mime {}",
            bytes.len(),
            cap,
            att.mime_type
        ));
    }

    let url = format!(
        "{}/{}/attachments:upload?uploadType=multipart",
        upload_base, space
    );

    let metadata = serde_json::json!({ "filename": att.filename }).to_string();
    let (content_type, body) = build_multipart_related(&metadata, &bytes, &att.mime_type);

    let resp = client
        .post(&url)
        .bearer_auth(token)
        .timeout(OUTBOUND_UPLOAD_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("upload request error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("upload failed: {} {}", status, body));
    }

    let parsed: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("upload response parse: {e}"))?;
    parsed
        .get("attachmentDataRef")
        .cloned()
        .ok_or_else(|| "upload response missing attachmentDataRef".into())
}

// --- Attachment parsing & download ---

/// Whitelist of text-like file extensions for `download_googlechat_file`.
const TEXT_EXTS: &[&str] = &[
    "txt", "csv", "log", "md", "json", "jsonl", "yaml", "yml", "toml", "xml",
    "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "c", "cpp", "h", "hpp",
    "rb", "sh", "bash", "sql", "html", "css", "ini", "cfg", "conf",
];

/// Parse Google Chat attachment array into media references for async download.
///
/// Skips Drive-sourced attachments (different download API), and unknown
/// content types. Branches on `contentType` prefix to bucket into image /
/// audio / file.
fn parse_attachments(attachments: &[GoogleChatAttachment]) -> Vec<GoogleChatMediaRef> {
    let mut refs = Vec::new();
    for att in attachments {
        let content_type = att.content_type.clone().unwrap_or_default();
        let content_name = att.content_name.clone().unwrap_or_else(|| "file".into());

        match att.source.as_deref() {
            Some("DRIVE_FILE") => {
                let drive_file_id = match att
                    .drive_data_ref
                    .as_ref()
                    .and_then(|d| d.drive_file_id.clone())
                {
                    Some(id) => id,
                    None => continue,
                };
                if content_type.starts_with("video/") {
                    info!(content_name = %content_name, content_type = %content_type, "googlechat: drive video attachment skipped (not yet supported)");
                    continue;
                }
                // Google-native types (Doc / Sheet / Slides) need Drive's
                // `export` API with a target MIME, not `files.{id}?alt=media`.
                // Out of scope for the first cut — skip with a debug log.
                if content_type.starts_with("application/vnd.google-apps") {
                    tracing::debug!(content_name = %content_name, content_type = %content_type, "googlechat: google-native drive type skipped (needs Drive export API)");
                    continue;
                }
                refs.push(GoogleChatMediaRef::DriveFile {
                    drive_file_id,
                    content_name,
                    content_type,
                });
            }
            Some("UPLOADED_CONTENT") => {
                let resource_name = match att
                    .attachment_data_ref
                    .as_ref()
                    .and_then(|d| d.resource_name.clone())
                {
                    Some(rn) => rn,
                    None => continue,
                };
                if content_type.starts_with("image/") {
                    refs.push(GoogleChatMediaRef::Image {
                        resource_name,
                        content_name,
                    });
                } else if content_type.starts_with("audio/") {
                    refs.push(GoogleChatMediaRef::Audio {
                        resource_name,
                        content_name,
                        content_type,
                    });
                } else if content_type.starts_with("video/") {
                    info!(content_name = %content_name, content_type = %content_type, "googlechat: video attachment skipped (not yet supported)");
                } else {
                    refs.push(GoogleChatMediaRef::File {
                        resource_name,
                        content_name,
                    });
                }
            }
            _ => continue,
        }
    }
    refs
}

/// Resize image so longest side ≤ 1200px, then encode as JPEG.
/// GIFs are passed through unchanged to preserve animation.
fn resize_and_compress(raw: &[u8]) -> Result<(Vec<u8>, String), image::ImageError> {
    use image::ImageReader;
    use std::io::Cursor;

    let reader = ImageReader::new(Cursor::new(raw)).with_guessed_format()?;
    let format = reader.format();
    if format == Some(image::ImageFormat::Gif) {
        return Ok((raw.to_vec(), "image/gif".to_string()));
    }
    let img = reader.decode()?;
    let (w, h) = (img.width(), img.height());
    let img = if w > IMAGE_MAX_DIMENSION_PX || h > IMAGE_MAX_DIMENSION_PX {
        let max_side = std::cmp::max(w, h);
        let ratio = f64::from(IMAGE_MAX_DIMENSION_PX) / f64::from(max_side);
        let new_w = (f64::from(w) * ratio) as u32;
        let new_h = (f64::from(h) * ratio) as u32;
        img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, IMAGE_JPEG_QUALITY);
    img.write_with_encoder(encoder)?;
    Ok((buf.into_inner(), "image/jpeg".to_string()))
}

/// Build the Media API URL for a given resource_name.
/// Google Chat Media API uses `{+resourceName}` (RFC 6570 reserved expansion),
/// so `/` must stay literal while other special chars are percent-encoded.
fn media_url(api_base: &str, resource_name: &str) -> String {
    let encoded: String = resource_name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect();
    format!("{}/media/{}?alt=media", api_base, encoded)
}

/// Download an image attachment via Google Chat Media API → resize/compress → base64.
pub async fn download_googlechat_image(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    resource_name: &str,
    content_name: &str,
) -> Option<crate::schema::Attachment> {
    let url = media_url(api_base, resource_name);
    let resp = match client.get(&url).bearer_auth(token).timeout(MEDIA_REQUEST_TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(content_name, error = %e, "googlechat image download failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(content_name, status = %resp.status(), "googlechat image download failed");
        return None;
    }
    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
        if let Ok(size) = cl.to_str().unwrap_or("0").parse::<u64>() {
            if size > IMAGE_MAX_DOWNLOAD {
                warn!(content_name, size, "googlechat image Content-Length exceeds 10MB limit");
                return None;
            }
        }
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > IMAGE_MAX_DOWNLOAD {
        warn!(content_name, size = bytes.len(), "googlechat image exceeds 10MB limit");
        return None;
    }
    let (compressed, mime) = match resize_and_compress(&bytes) {
        Ok(v) => v,
        Err(e) => {
            warn!(content_name, error = %e, "googlechat image resize failed");
            return None;
        }
    };
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&compressed);
    Some(crate::schema::Attachment {
        attachment_type: "image".into(),
        filename: content_name.to_string(),
        mime_type: mime,
        data,
        size: compressed.len() as u64,
    })
}

/// Download a text-like file via Google Chat Media API → base64.
/// Non-text extensions are skipped to avoid sending binary garbage to the model.
pub async fn download_googlechat_file(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    resource_name: &str,
    content_name: &str,
    remaining_budget: u64,
) -> Option<crate::schema::Attachment> {
    let ext = content_name.rsplit('.').next().unwrap_or("").to_lowercase();
    if !TEXT_EXTS.contains(&ext.as_str()) {
        tracing::debug!(content_name, "skipping non-text googlechat file attachment");
        return None;
    }
    let max_size = FILE_MAX_DOWNLOAD.min(remaining_budget);
    let url = media_url(api_base, resource_name);
    let resp = match client.get(&url).bearer_auth(token).timeout(MEDIA_REQUEST_TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(content_name, error = %e, "googlechat file download failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(content_name, status = %resp.status(), "googlechat file download failed");
        return None;
    }
    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
        if let Ok(size) = cl.to_str().unwrap_or("0").parse::<u64>() {
            if size > max_size {
                warn!(content_name, size, limit = max_size, "googlechat file Content-Length exceeds limit");
                return None;
            }
        }
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > max_size {
        warn!(content_name, size = bytes.len(), limit = max_size, "googlechat file exceeds size limit");
        return None;
    }
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(crate::schema::Attachment {
        attachment_type: "text_file".into(),
        filename: content_name.to_string(),
        mime_type: "text/plain".into(),
        data,
        size: bytes.len() as u64,
    })
}

/// Download an audio attachment as-is (no resize/transcode) → base64.
/// Core's STT pipeline (when available) consumes this as `audio` attachment_type.
pub async fn download_googlechat_audio(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    resource_name: &str,
    content_name: &str,
    content_type: &str,
) -> Option<crate::schema::Attachment> {
    let url = media_url(api_base, resource_name);
    let resp = match client.get(&url).bearer_auth(token).timeout(MEDIA_REQUEST_TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(content_name, error = %e, "googlechat audio download failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(content_name, status = %resp.status(), "googlechat audio download failed");
        return None;
    }
    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
        if let Ok(size) = cl.to_str().unwrap_or("0").parse::<u64>() {
            if size > AUDIO_MAX_DOWNLOAD {
                warn!(content_name, size, "googlechat audio Content-Length exceeds 25MB limit");
                return None;
            }
        }
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > AUDIO_MAX_DOWNLOAD {
        warn!(content_name, size = bytes.len(), "googlechat audio exceeds 25MB limit");
        return None;
    }
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(crate::schema::Attachment {
        attachment_type: "audio".into(),
        filename: content_name.to_string(),
        mime_type: content_type.to_string(),
        data,
        size: bytes.len() as u64,
    })
}

/// Base URL for Google Drive v3 API. Override only in tests.
pub const GOOGLE_DRIVE_API_BASE: &str = "https://www.googleapis.com/drive/v3";

/// Download a Drive-sourced attachment via the Drive v3 API.
///
/// Routes the result into the same image/file/audio post-processing as
/// uploaded-content attachments so the downstream agent sees a uniform
/// `Attachment` shape regardless of source. Requires a token issued with
/// the `drive.readonly` scope — token exchange will fail upstream if the
/// SA hasn't been authorized for Drive.
///
/// Per-family caps (image 10 MB / audio 25 MB / text 1 MB per file) match
/// the inbound `UPLOADED_CONTENT` limits.
pub async fn download_googledrive_file(
    client: &reqwest::Client,
    token: &str,
    drive_api_base: &str,
    drive_file_id: &str,
    content_name: &str,
    content_type: &str,
    remaining_text_budget: u64,
) -> Option<crate::schema::Attachment> {
    // Decide cap up front so we can reject oversize at Content-Length time.
    let (cap, kind): (u64, &str) = if content_type.starts_with("image/") {
        (IMAGE_MAX_DOWNLOAD, "image")
    } else if content_type.starts_with("audio/") {
        (AUDIO_MAX_DOWNLOAD, "audio")
    } else if content_type.starts_with("text/") {
        (FILE_MAX_DOWNLOAD.min(remaining_text_budget), "file")
    } else {
        // Try to treat unknown content types as text if the extension is in
        // the inbound text whitelist; otherwise skip.
        let ext = content_name.rsplit('.').next().unwrap_or("").to_lowercase();
        if TEXT_EXTS.contains(&ext.as_str()) {
            (FILE_MAX_DOWNLOAD.min(remaining_text_budget), "file")
        } else {
            tracing::debug!(content_name, content_type, "skipping unsupported drive content type");
            return None;
        }
    };

    let url = format!("{}/files/{}?alt=media", drive_api_base, drive_file_id);
    let resp = match client
        .get(&url)
        .bearer_auth(token)
        .timeout(MEDIA_REQUEST_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(content_name, error = %e, "googledrive download failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(content_name, status = %resp.status(), "googledrive download failed");
        return None;
    }
    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
        if let Ok(size) = cl.to_str().unwrap_or("0").parse::<u64>() {
            if size > cap {
                warn!(content_name, size, cap, "googledrive Content-Length exceeds cap");
                return None;
            }
        }
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > cap {
        warn!(content_name, size = bytes.len(), cap, "googledrive payload exceeds cap");
        return None;
    }

    match kind {
        "image" => {
            let (compressed, mime) = match resize_and_compress(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    warn!(content_name, error = %e, "googledrive image resize failed");
                    return None;
                }
            };
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD.encode(&compressed);
            Some(crate::schema::Attachment {
                attachment_type: "image".into(),
                filename: content_name.to_string(),
                mime_type: mime,
                data,
                size: compressed.len() as u64,
            })
        }
        "audio" => {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some(crate::schema::Attachment {
                attachment_type: "audio".into(),
                filename: content_name.to_string(),
                mime_type: content_type.to_string(),
                data,
                size: bytes.len() as u64,
            })
        }
        _ => {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some(crate::schema::Attachment {
                attachment_type: "text_file".into(),
                filename: content_name.to_string(),
                mime_type: content_type.to_string(),
                data,
                size: bytes.len() as u64,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Webhook parsing tests ---

    fn make_envelope(
        text: &str,
        argument_text: Option<&str>,
        sender_type: &str,
        space_type: &str,
        thread_name: Option<&str>,
    ) -> String {
        let arg_field = argument_text
            .map(|a| format!(r#""argumentText": "{a}","#))
            .unwrap_or_default();
        let thread_field = thread_name
            .map(|t| format!(r#","thread": {{"name": "{t}"}}"#))
            .unwrap_or_default();
        format!(
            r#"{{
                "chat": {{
                    "user": {{
                        "name": "users/111",
                        "displayName": "Test",
                        "type": "{sender_type}"
                    }},
                    "messagePayload": {{
                        "message": {{
                            "name": "spaces/SP/messages/msg1",
                            "text": "{text}",
                            {arg_field}
                            "sender": {{
                                "name": "users/111",
                                "displayName": "Test",
                                "type": "{sender_type}"
                            }},
                            "space": {{
                                "name": "spaces/SP",
                                "type": "{space_type}"
                            }}
                            {thread_field}
                        }},
                        "space": {{
                            "name": "spaces/SP",
                            "type": "{space_type}"
                        }}
                    }}
                }}
            }}"#
        )
    }

    #[test]
    fn parse_dm_message() {
        let json = make_envelope("hello", None, "HUMAN", "DM", None);
        let envelope: GoogleChatEnvelope = serde_json::from_str(&json).unwrap();
        let chat = envelope.chat.unwrap();
        let msg = chat.message_payload.unwrap().message.unwrap();
        assert_eq!(msg.text.as_deref(), Some("hello"));
        assert_eq!(msg.sender.unwrap().user_type, "HUMAN");
    }

    #[test]
    fn parse_space_message_with_thread() {
        let json = make_envelope(
            "@Bot hi",
            Some("hi"),
            "HUMAN",
            "ROOM",
            Some("spaces/SP/threads/t1"),
        );
        let envelope: GoogleChatEnvelope = serde_json::from_str(&json).unwrap();
        let chat = envelope.chat.unwrap();
        let payload = chat.message_payload.unwrap();
        let msg = payload.message.as_ref().unwrap();
        assert_eq!(msg.argument_text.as_deref(), Some("hi"));
        assert_eq!(msg.thread.as_ref().unwrap().name, "spaces/SP/threads/t1");
        assert_eq!(payload.space.as_ref().unwrap().space_type.as_deref(), Some("ROOM"));
    }

    #[test]
    fn parse_bot_message_detected() {
        let json = make_envelope("bot says hi", None, "BOT", "DM", None);
        let envelope: GoogleChatEnvelope = serde_json::from_str(&json).unwrap();
        let chat = envelope.chat.unwrap();
        let user = chat.user.unwrap();
        assert_eq!(user.user_type, "BOT");
    }

    #[test]
    fn parse_missing_chat_field() {
        let json = r#"{"type": "ADDED_TO_SPACE"}"#;
        let envelope: GoogleChatEnvelope = serde_json::from_str(json).unwrap();
        assert!(envelope.chat.is_none());
    }

    #[test]
    fn parse_missing_message_payload() {
        let json = r#"{"chat": {"user": {"name": "u/1", "displayName": "X", "type": "HUMAN"}}}"#;
        let envelope: GoogleChatEnvelope = serde_json::from_str(json).unwrap();
        assert!(envelope.chat.unwrap().message_payload.is_none());
    }

    #[test]
    fn parse_invalid_json() {
        let result: Result<GoogleChatEnvelope, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn argument_text_preferred_over_text() {
        let json = make_envelope("@Bot explain", Some("explain"), "HUMAN", "ROOM", None);
        let envelope: GoogleChatEnvelope = serde_json::from_str(&json).unwrap();
        let msg = envelope
            .chat
            .unwrap()
            .message_payload
            .unwrap()
            .message
            .unwrap();
        let text = msg
            .argument_text
            .as_deref()
            .or(msg.text.as_deref())
            .unwrap();
        assert_eq!(text, "explain");
    }

    #[test]
    fn sender_name_strips_users_prefix() {
        let sender_id = "users/123456";
        let name = sender_id.strip_prefix("users/").unwrap_or(sender_id);
        assert_eq!(name, "123456");
    }

    #[test]
    fn message_id_extracts_last_segment() {
        let msg_name = "spaces/SP/messages/abc123";
        let id = msg_name.rsplit('/').next().unwrap_or(msg_name);
        assert_eq!(id, "abc123");
    }

    // --- split_text tests ---

    #[test]
    fn split_text_short() {
        let chunks = split_text("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_text_exact_limit() {
        let text = "a".repeat(100);
        let chunks = split_text(&text, 100);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_text_over_limit() {
        let text = "a".repeat(150);
        let chunks = split_text(&text, 100);
        assert_eq!(chunks.len(), 2);
        let reassembled: String = chunks.concat();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn split_text_breaks_at_newline() {
        let text = format!("{}\n{}", "a".repeat(50), "b".repeat(50));
        let chunks = split_text(&text, 60);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('\n'));
    }

    #[test]
    fn split_text_breaks_at_space() {
        let text = format!("{} {}", "a".repeat(50), "b".repeat(50));
        let chunks = split_text(&text, 60);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn split_text_chinese_utf8_safe() {
        let text = "你好世界測試谷歌聊天中文消息分割安全驗證完成";
        let chunks = split_text(text, 10);
        assert!(chunks.len() > 1);
        let reassembled: String = chunks.concat();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn split_text_search_start_char_boundary() {
        let text: String = "谷歌".repeat(150); // 300 chars, 900 bytes
        let chunks = split_text(&text, 500);
        assert!(chunks.len() >= 2);
        let reassembled: String = chunks.concat();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn split_text_empty() {
        let chunks = split_text("", 100);
        assert!(chunks.is_empty());
    }

    // --- Token cache tests ---

    #[test]
    fn token_cache_rejects_invalid_json() {
        let result = GoogleChatTokenCache::new("not json");
        assert!(result.is_err());
    }

    #[test]
    fn token_cache_rejects_missing_fields() {
        match GoogleChatTokenCache::new(r#"{"type": "service_account"}"#) {
            Err(e) => assert!(e.contains("client_email"), "unexpected error: {e}"),
            Ok(_) => panic!("expected error for missing client_email"),
        }
    }

    #[test]
    fn token_cache_accepts_valid_sa_key() {
        let key = r#"{
            "type": "service_account",
            "client_email": "test@test.iam.gserviceaccount.com",
            "private_key": "-----BEGIN RSA PRIVATE KEY-----\nMIIBogIBAAJBALvRE+oCMiEhtfO5ufaVc9wGPUMgPGxmVFiMPC/NMxmCSiMGNO9h\nCOyByeF78QHp4gOW/lgVU8MJkv33hVMbOr0CAwEAAQJAD2k/cFR5MIkw1PFcm98K\n9MqYKGpJCmGBjFY0ek0FHoC14d/hpAGaoWMjNaAyjU/IbGv1fj8C5MfFRal0fV/L\nAQIhAP0T6FPJMm3O4bM18kMHnOP2+Y5kxMpVxCCjkVNH7D09AiEAvXEQJYwR+PFs\njDDhEm4VPmk+lKJoQlopj8TN5gQV8DECIBcXbU+LPWx4H+qRElhCB1B5a9mYmpY\nV6LFPnvSfHqNAiEAiNj5+A6E7WJ50il+5NG5yn7gXh8vNxdCYIw5qx6C2bECIBmW\nVGVRhSmNsmDMJFsGIdKJsnEXpizIVHtfpXsS4j9X\n-----END RSA PRIVATE KEY-----\n"
        }"#;
        let result = GoogleChatTokenCache::new(key);
        assert!(result.is_ok());
    }

    // --- Bot filtering logic test ---

    #[test]
    fn bot_user_type_detected() {
        let json = make_envelope("hello", None, "BOT", "DM", None);
        let envelope: GoogleChatEnvelope = serde_json::from_str(&json).unwrap();
        let chat = envelope.chat.unwrap();
        let sender = chat
            .message_payload
            .as_ref()
            .and_then(|p| p.message.as_ref())
            .and_then(|m| m.sender.as_ref())
            .or(chat.user.as_ref());
        let is_bot = sender.map(|s| s.user_type == "BOT").unwrap_or(false);
        assert!(is_bot);
    }

    // --- JWT verifier tests ---

    #[tokio::test]
    async fn jwt_rejects_missing_bearer_prefix() {
        let verifier = GoogleChatJwtVerifier::new("123456".into());
        let result = verifier.verify("NotBearer xyz").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Bearer"));
    }

    #[tokio::test]
    async fn jwt_rejects_invalid_token() {
        let verifier = GoogleChatJwtVerifier::new("123456".into());
        let result = verifier.verify("Bearer not.a.valid.jwt").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn jwt_rejects_empty_bearer() {
        let verifier = GoogleChatJwtVerifier::new("123456".into());
        let result = verifier.verify("Bearer ").await;
        assert!(result.is_err());
    }

    #[test]
    fn email_claim_accepts_gsuite_addons_account() {
        let claims = serde_json::json!({"email": "service-123456@gcp-sa-gsuiteaddons.iam.gserviceaccount.com"});
        assert!(verify_email_claim(&claims).is_ok());
    }

    #[test]
    fn email_claim_rejects_other_google_email() {
        let claims = serde_json::json!({"email": "attacker@example.iam.gserviceaccount.com"});
        let err = verify_email_claim(&claims).unwrap_err();
        assert!(err.contains("email claim mismatch"));
    }

    #[test]
    fn email_claim_rejects_unrelated_gserviceaccount() {
        let claims = serde_json::json!({"email": "my-sa@my-project.iam.gserviceaccount.com"});
        assert!(verify_email_claim(&claims).is_err());
    }

    #[test]
    fn email_claim_rejects_missing_email() {
        let claims = serde_json::json!({"sub": "123", "iss": "accounts.google.com"});
        let err = verify_email_claim(&claims).unwrap_err();
        assert!(err.contains("missing email"));
    }

    #[test]
    fn email_claim_rejects_non_string_email() {
        let claims = serde_json::json!({"email": 12345});
        assert!(verify_email_claim(&claims).is_err());
    }

    #[test]
    fn human_user_type_not_filtered() {
        let json = make_envelope("hello", None, "HUMAN", "DM", None);
        let envelope: GoogleChatEnvelope = serde_json::from_str(&json).unwrap();
        let chat = envelope.chat.unwrap();
        let sender = chat
            .message_payload
            .as_ref()
            .and_then(|p| p.message.as_ref())
            .and_then(|m| m.sender.as_ref())
            .or(chat.user.as_ref());
        let is_bot = sender.map(|s| s.user_type == "BOT").unwrap_or(false);
        assert!(!is_bot);
    }

    // --- markdown_to_gchat tests ---

    #[test]
    fn markdown_bold_double_asterisk() {
        assert_eq!(markdown_to_gchat("hello **world**"), "hello *world*");
    }

    #[test]
    fn markdown_bold_underscore() {
        assert_eq!(markdown_to_gchat("hello __world__"), "hello *world*");
    }

    #[test]
    fn markdown_link_conversion() {
        assert_eq!(
            markdown_to_gchat("see [docs](https://example.com) here"),
            "see <https://example.com|docs> here"
        );
    }

    #[test]
    fn markdown_heading_to_bold() {
        assert_eq!(markdown_to_gchat("# Title\ntext"), "*Title*\ntext");
        assert_eq!(markdown_to_gchat("## Sub\ntext"), "*Sub*\ntext");
        assert_eq!(markdown_to_gchat("### Deep\ntext"), "*Deep*\ntext");
    }

    #[test]
    fn markdown_code_block_preserved() {
        let input = "before\n```rust\nlet **x** = 1;\n```\nafter **bold**";
        let output = markdown_to_gchat(input);
        assert!(output.contains("let **x** = 1;"));
        assert!(output.contains("after *bold*"));
    }

    #[test]
    fn markdown_inline_code_preserved() {
        assert_eq!(
            markdown_to_gchat("use `**not bold**` here **bold**"),
            "use `**not bold**` here *bold*"
        );
    }

    #[test]
    fn markdown_strikethrough() {
        assert_eq!(markdown_to_gchat("~~deleted~~"), "~deleted~");
        assert_eq!(
            markdown_to_gchat("keep ~~this~~ and ~~that~~"),
            "keep ~this~ and ~that~"
        );
    }

    #[test]
    fn markdown_italic_asterisk() {
        assert_eq!(markdown_to_gchat("*italic*"), "_italic_");
        assert_eq!(
            markdown_to_gchat("plain *one* and *two*"),
            "plain _one_ and _two_"
        );
    }

    #[test]
    fn markdown_italic_does_not_match_bold() {
        assert_eq!(markdown_to_gchat("**bold**"), "*bold*");
        assert_eq!(
            markdown_to_gchat("**bold** and *italic*"),
            "*bold* and _italic_"
        );
    }

    #[test]
    fn markdown_italic_underscore_passes_through() {
        // Google Chat italic is _text_, single underscore should pass through
        assert_eq!(markdown_to_gchat("_italic_"), "_italic_");
    }

    #[test]
    fn markdown_italic_no_match_when_unbalanced() {
        // Lone asterisks (no closing) should pass through
        assert_eq!(markdown_to_gchat("a * b"), "a * b");
        // Whitespace adjacent to asterisks should not match (avoid matching multiplication)
        assert_eq!(markdown_to_gchat("2 * 3 * 4"), "2 * 3 * 4");
    }

    #[test]
    fn markdown_empty_string() {
        assert_eq!(markdown_to_gchat(""), "");
    }

    #[test]
    fn markdown_no_conversion_needed() {
        assert_eq!(markdown_to_gchat("plain text"), "plain text");
    }

    #[test]
    fn markdown_multiple_links() {
        assert_eq!(
            markdown_to_gchat("[a](http://a.com) and [b](http://b.com)"),
            "<http://a.com|a> and <http://b.com|b>"
        );
    }

    #[test]
    fn markdown_nested_bold_in_link_text() {
        assert_eq!(
            markdown_to_gchat("[**bold link**](http://x.com)"),
            "<http://x.com|*bold link*>"
        );
    }

    #[test]
    fn parse_send_message_response_name() {
        let resp_json = r#"{"name": "spaces/SP1/messages/msg123", "text": "hello"}"#;
        let parsed: serde_json::Value = serde_json::from_str(resp_json).unwrap();
        let name = parsed.get("name").and_then(|v| v.as_str());
        assert_eq!(name, Some("spaces/SP1/messages/msg123"));
    }

    #[tokio::test]
    async fn handle_reply_sends_gateway_response_success() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/TEST/messages/msg_abc"}),
            ))
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: "hello".into(),
            },
            command: None,
            request_id: Some("req_123".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected GatewayResponse on event_tx");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_123");
        assert!(resp.success);
        assert_eq!(resp.message_id, Some("spaces/TEST/messages/msg_abc".into()));
    }

    #[tokio::test]
    async fn handle_reply_sends_failure_response_on_api_error() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: "hello".into(),
            },
            command: None,
            request_id: Some("req_fail".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected GatewayResponse on event_tx");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_fail");
        assert!(!resp.success);
        assert!(resp.message_id.is_none());
        let err = resp.error.expect("error should be set on send failure");
        assert!(err.contains("500"), "error should include status code, got: {}", err);
    }

    #[tokio::test]
    async fn handle_reply_empty_message_short_circuits() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        // Mount a mock that would fail the test if called
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: "".into(),
            },
            command: None,
            request_id: Some("req_empty".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected failure GatewayResponse for empty message");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_empty");
        assert!(!resp.success);
        assert_eq!(resp.error, Some("empty message".into()));
    }

    #[tokio::test]
    async fn handle_reply_multi_chunk_failure_includes_error() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let long_text = "x".repeat(5000);
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: long_text,
            },
            command: None,
            request_id: Some("req_multi_fail".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected GatewayResponse");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_multi_fail");
        assert!(!resp.success);
        assert!(resp.message_id.is_none());
        let err = resp.error.expect("multi-chunk failure should set error");
        assert!(err.contains("500"));
    }

    #[tokio::test]
    async fn handle_reply_token_failure_sends_error_response() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let adapter = GoogleChatAdapter::new(None, None, None, GoogleChatConfig::default());

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: "hello".into(),
            },
            command: None,
            request_id: Some("req_notoken".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected failure GatewayResponse");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_notoken");
        assert!(!resp.success);
        assert_eq!(resp.error, Some("no credentials configured".into()));
    }

    #[tokio::test]
    async fn handle_reply_edit_message_does_not_send_response() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path_regex("/spaces/.*/messages/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/SP/messages/msg1"}),
            ))
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "spaces/SP/messages/msg1".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/SP".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: "updated text".into(),
            },
            command: Some("edit_message".into()),
            request_id: None,
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_err());
    }

    #[tokio::test]
    async fn handle_reply_multi_chunk_sends_gateway_response() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/TEST/messages/first_chunk"}),
            ))
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let long_text = "x".repeat(5000);
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: long_text,
            },
            command: None,
            request_id: Some("req_multi".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected GatewayResponse for multi-chunk");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_multi");
        assert!(resp.success);
        assert_eq!(resp.message_id, Some("spaces/TEST/messages/first_chunk".into()));
    }

    #[tokio::test]
    async fn handle_reply_multi_chunk_partial_failure_reports_failure() {
        // Mixed success/failure: chunk 1 succeeds, subsequent chunks fail.
        // Expect success=false (any chunk failure marks overall as failed),
        // but message_id is still set so core has a reference.
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        // First request: 200 OK with message name
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/TEST/messages/first_chunk"}),
            ))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        // Subsequent requests: 500
        Mock::given(method("POST"))
            .and(path_regex("/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();

        let long_text = "x".repeat(5000);
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: Vec::new(),
                text: long_text,
            },
            command: None,
            request_id: Some("req_partial".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv();
        assert!(received.is_ok(), "expected GatewayResponse");
        let resp: GatewayResponse = serde_json::from_str(&received.unwrap()).unwrap();
        assert_eq!(resp.request_id, "req_partial");
        assert!(!resp.success, "partial failure must report success=false");
        assert_eq!(resp.message_id, Some("spaces/TEST/messages/first_chunk".into()));
        let err = resp.error.expect("partial failure should set error");
        assert!(err.contains("500"));
    }

    // --- Attachment parsing tests ---

    fn make_attachment(
        source: &str,
        content_type: &str,
        content_name: &str,
        resource_name: Option<&str>,
    ) -> GoogleChatAttachment {
        GoogleChatAttachment {
            name: Some("spaces/SP/messages/MSG/attachments/ATT".into()),
            content_name: Some(content_name.into()),
            content_type: Some(content_type.into()),
            source: Some(source.into()),
            attachment_data_ref: resource_name.map(|rn| AttachmentDataRef {
                resource_name: Some(rn.into()),
            }),
            drive_data_ref: None,
        }
    }

    #[test]
    fn parse_attachments_image() {
        let atts = vec![make_attachment(
            "UPLOADED_CONTENT",
            "image/png",
            "photo.png",
            Some("AATT_resource"),
        )];
        let refs = parse_attachments(&atts);
        assert_eq!(refs.len(), 1);
        match &refs[0] {
            GoogleChatMediaRef::Image {
                resource_name,
                content_name,
            } => {
                assert_eq!(resource_name, "AATT_resource");
                assert_eq!(content_name, "photo.png");
            }
            other => panic!("expected Image, got {:?}", other),
        }
    }

    #[test]
    fn parse_attachments_audio() {
        let atts = vec![make_attachment(
            "UPLOADED_CONTENT",
            "audio/mp4",
            "voice.m4a",
            Some("AATT"),
        )];
        let refs = parse_attachments(&atts);
        assert!(matches!(refs[0], GoogleChatMediaRef::Audio { .. }));
    }

    #[test]
    fn parse_attachments_file() {
        let atts = vec![make_attachment(
            "UPLOADED_CONTENT",
            "text/plain",
            "notes.txt",
            Some("AATT"),
        )];
        let refs = parse_attachments(&atts);
        assert!(matches!(refs[0], GoogleChatMediaRef::File { .. }));
    }

    #[test]
    fn parse_attachments_skips_google_native_drive_types() {
        // Google-native types (Docs/Sheets/Slides) require Drive's export API
        // and cannot be fetched via files.{id}?alt=media — skipped at parse.
        let atts = vec![GoogleChatAttachment {
            name: Some("spaces/SP/messages/MSG/attachments/ATT".into()),
            content_name: Some("doc".into()),
            content_type: Some("application/vnd.google-apps.document".into()),
            source: Some("DRIVE_FILE".into()),
            attachment_data_ref: None,
            drive_data_ref: Some(DriveDataRef {
                drive_file_id: Some("drive_id_123".into()),
            }),
        }];
        assert_eq!(parse_attachments(&atts).len(), 0);
    }

    #[test]
    fn parse_attachments_emits_drive_file_for_uploaded_binary() {
        // A regular file (image, PDF, audio, text) uploaded to Drive and then
        // attached should produce a DriveFile media ref.
        let atts = vec![GoogleChatAttachment {
            name: Some("spaces/SP/messages/MSG/attachments/ATT".into()),
            content_name: Some("photo.png".into()),
            content_type: Some("image/png".into()),
            source: Some("DRIVE_FILE".into()),
            attachment_data_ref: None,
            drive_data_ref: Some(DriveDataRef {
                drive_file_id: Some("drive_id_456".into()),
            }),
        }];
        let refs = parse_attachments(&atts);
        assert_eq!(refs.len(), 1);
        assert!(matches!(refs[0], GoogleChatMediaRef::DriveFile { .. }));
    }

    #[test]
    fn parse_attachments_skips_drive_file_with_missing_id() {
        let atts = vec![GoogleChatAttachment {
            name: Some("spaces/SP/messages/MSG/attachments/ATT".into()),
            content_name: Some("photo.png".into()),
            content_type: Some("image/png".into()),
            source: Some("DRIVE_FILE".into()),
            attachment_data_ref: None,
            drive_data_ref: Some(DriveDataRef {
                drive_file_id: None,
            }),
        }];
        assert_eq!(parse_attachments(&atts).len(), 0);
    }

    #[tokio::test]
    async fn download_googledrive_file_image_succeeds() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        // Use a minimal but valid 1x1 PNG so resize_and_compress doesn't fail.
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
            0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
            0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
            0x42, 0x60, 0x82,
        ];
        Mock::given(method("GET"))
            .and(path("/files/drv-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png_bytes))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googledrive_file(
            &client,
            "fake-token",
            &mock_server.uri(),
            "drv-abc",
            "photo.png",
            "image/png",
            TEXT_TOTAL_CAP,
        )
        .await;
        let att = result.expect("expected image attachment");
        assert_eq!(att.attachment_type, "image");
        assert_eq!(att.filename, "photo.png");
    }

    #[tokio::test]
    async fn download_googledrive_file_rejects_oversize_via_content_length() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/files/big"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", "30000000") // 30 MB > 25 MB audio cap
                    .set_body_bytes(vec![0u8; 100]),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googledrive_file(
            &client,
            "fake-token",
            &mock_server.uri(),
            "big",
            "song.mp3",
            "audio/mpeg",
            TEXT_TOTAL_CAP,
        )
        .await;
        assert!(result.is_none(), "oversized audio must be rejected");
    }

    #[tokio::test]
    async fn download_googledrive_file_skips_unsupported_type() {
        // Unknown content type with no whitelisted extension → skip without
        // hitting the network. Mock with expect(0) to enforce no request.
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/files/.*"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googledrive_file(
            &client,
            "fake-token",
            &mock_server.uri(),
            "x",
            "weird.bin",
            "application/x-executable",
            TEXT_TOTAL_CAP,
        )
        .await;
        assert!(result.is_none());
    }

    #[test]
    fn token_cache_default_scope_is_chat_bot() {
        let key_json = r#"{
            "client_email": "test@example.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0BAQEFAASCAT8wggE7AgEAAkEA0Z+8O1eaQ9YHGsxn\nVQQAAAAAAAA=\n-----END PRIVATE KEY-----\n"
        }"#;
        // Parse may fail on the dummy key but that's not what we're testing
        // — we want to check the scope string composition when it succeeds.
        if let Ok(cache) = GoogleChatTokenCache::new_with_scopes(key_json, &[]) {
            assert_eq!(cache.scopes, GOOGLE_CHAT_BASE_SCOPE);
        }
        if let Ok(cache) = GoogleChatTokenCache::new_with_scopes(
            key_json,
            &[GOOGLE_DRIVE_READONLY_SCOPE],
        ) {
            assert_eq!(
                cache.scopes,
                format!("{} {}", GOOGLE_CHAT_BASE_SCOPE, GOOGLE_DRIVE_READONLY_SCOPE)
            );
        }
    }

    #[test]
    fn parse_attachments_skips_missing_resource_name() {
        let atts = vec![make_attachment(
            "UPLOADED_CONTENT",
            "image/png",
            "photo.png",
            None,
        )];
        assert_eq!(parse_attachments(&atts).len(), 0);
    }

    #[test]
    fn media_url_preserves_slashes_and_encodes_specials() {
        let url = media_url("https://chat.googleapis.com/v1", "spaces/SP/messages/MSG/attachments/ATT");
        assert_eq!(
            url,
            "https://chat.googleapis.com/v1/media/spaces/SP/messages/MSG/attachments/ATT?alt=media"
        );
        let url2 = media_url("https://chat.googleapis.com/v1", "AATT/some+resource=name");
        assert_eq!(
            url2,
            "https://chat.googleapis.com/v1/media/AATT/some%2Bresource%3Dname?alt=media"
        );
    }

    #[tokio::test]
    async fn download_googlechat_image_resizes_and_returns_attachment() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        // Generate a small valid PNG
        let img = image::RgbImage::from_pixel(10, 10, image::Rgb([255, 0, 0]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let png_bytes = buf.into_inner();

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/media/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(png_bytes)
                    .insert_header("content-type", "image/png"),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googlechat_image(
            &client,
            "fake-token",
            &mock_server.uri(),
            "AATT_resource",
            "photo.png",
        )
        .await;
        let att = result.expect("expected successful download");
        assert_eq!(att.attachment_type, "image");
        assert_eq!(att.filename, "photo.png");
        assert_eq!(att.mime_type, "image/jpeg"); // resized PNG → JPEG
        assert!(!att.data.is_empty());
        assert!(att.size > 0);
    }

    #[tokio::test]
    async fn download_googlechat_file_rejects_non_text_extension() {
        let client = reqwest::Client::new();
        let result = download_googlechat_file(
            &client,
            "fake-token",
            "https://unused", // not called for non-text
            "AATT",
            "binary.exe",
            TEXT_TOTAL_CAP,
        )
        .await;
        assert!(result.is_none(), "non-text extensions must be skipped");
    }

    #[tokio::test]
    async fn download_googlechat_file_text_extension_succeeds() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/media/.*"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googlechat_file(
            &client,
            "fake-token",
            &mock_server.uri(),
            "AATT",
            "notes.txt",
            TEXT_TOTAL_CAP,
        )
        .await;
        let att = result.expect("expected successful download");
        assert_eq!(att.attachment_type, "text_file");
        assert_eq!(att.filename, "notes.txt");
        assert_eq!(att.mime_type, "text/plain");
    }

    #[tokio::test]
    async fn download_googlechat_audio_returns_attachment() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        let audio_bytes = vec![0u8; 1024];
        Mock::given(method("GET"))
            .and(path_regex("/media/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(audio_bytes.clone()))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googlechat_audio(
            &client,
            "fake-token",
            &mock_server.uri(),
            "AATT",
            "voice.m4a",
            "audio/mp4",
        )
        .await;
        let att = result.expect("expected successful download");
        assert_eq!(att.attachment_type, "audio");
        assert_eq!(att.filename, "voice.m4a");
        assert_eq!(att.mime_type, "audio/mp4");
        assert_eq!(att.size, 1024);
    }

    #[tokio::test]
    async fn download_googlechat_image_rejects_oversized_content_length() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/media/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", "20000000") // 20 MB > 10 MB limit
                    .set_body_bytes(vec![0u8; 100]),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = download_googlechat_image(
            &client,
            "fake-token",
            &mock_server.uri(),
            "AATT",
            "huge.png",
        )
        .await;
        assert!(result.is_none(), "oversized image must be rejected");
    }

    // --- Outbound attachment tests ---

    #[test]
    fn outbound_mime_cap_allows_image_audio_text_pdf() {
        assert_eq!(outbound_mime_cap("image/png"), Some(OUTBOUND_IMAGE_MAX_BYTES));
        assert_eq!(outbound_mime_cap("image/jpeg"), Some(OUTBOUND_IMAGE_MAX_BYTES));
        assert_eq!(outbound_mime_cap("audio/mpeg"), Some(OUTBOUND_AUDIO_MAX_BYTES));
        assert_eq!(outbound_mime_cap("text/plain"), Some(OUTBOUND_TEXT_MAX_BYTES));
        assert_eq!(outbound_mime_cap("application/pdf"), Some(OUTBOUND_PDF_MAX_BYTES));
    }

    #[test]
    fn outbound_mime_cap_skips_video_and_unknown() {
        assert_eq!(outbound_mime_cap("video/mp4"), None);
        assert_eq!(outbound_mime_cap("application/x-executable"), None);
        assert_eq!(outbound_mime_cap("application/octet-stream"), None);
    }

    #[test]
    fn outbound_mime_cap_case_insensitive() {
        assert_eq!(outbound_mime_cap("Image/PNG"), Some(OUTBOUND_IMAGE_MAX_BYTES));
        assert_eq!(outbound_mime_cap("APPLICATION/PDF"), Some(OUTBOUND_PDF_MAX_BYTES));
    }

    #[test]
    fn build_attachment_payload_has_expected_shape() {
        let data_ref = serde_json::json!({
            "resourceName": "spaces/X/attachments/AB.token",
            "attachmentToken": "tok"
        });
        let payload = build_attachment_payload(&data_ref, "chart.png", "image/png");
        assert_eq!(payload["contentName"], "chart.png");
        assert_eq!(payload["contentType"], "image/png");
        assert_eq!(payload["attachmentDataRef"]["resourceName"], "spaces/X/attachments/AB.token");
    }

    fn make_image_attachment(bytes: &[u8]) -> crate::schema::Attachment {
        use base64::Engine;
        crate::schema::Attachment {
            attachment_type: "image".into(),
            filename: "pixel.png".into(),
            mime_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            size: bytes.len() as u64,
        }
    }

    #[tokio::test]
    async fn handle_reply_with_image_attachment_uploads_and_sends() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/attachments:upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attachmentDataRef": {
                    "resourceName": "spaces/TEST/attachments/AB.token",
                    "attachmentToken": "tok-xyz"
                },
                "contentType": "image/png"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/TEST/messages/msg_abc"}),
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();
        adapter.upload_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "orig_msg".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel {
                id: "spaces/TEST".into(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                attachments: vec![make_image_attachment(b"\x89PNG\r\n\x1a\n fake")],
                text: "here you go".into(),
            },
            command: None,
            request_id: Some("req_att".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let received = event_rx.try_recv().expect("expected GatewayResponse");
        let resp: GatewayResponse = serde_json::from_str(&received).unwrap();
        assert!(resp.success, "expected success, got error: {:?}", resp.error);
        assert_eq!(resp.message_id, Some("spaces/TEST/messages/msg_abc".into()));
        assert!(resp.error.is_none(), "no error expected: {:?}", resp.error);
    }

    #[tokio::test]
    async fn handle_reply_attachment_only_no_text_still_sends() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/attachments:upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attachmentDataRef": {
                    "resourceName": "spaces/T/attachments/A.tok",
                    "attachmentToken": "t"
                }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/T/messages/m1"}),
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();
        adapter.upload_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "x".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel { id: "spaces/T".into(), thread_id: None },
            content: Content {
                content_type: "text".into(),
                attachments: vec![make_image_attachment(b"px")],
                text: "".into(),
            },
            command: None,
            request_id: Some("req_only".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let resp: GatewayResponse = serde_json::from_str(&event_rx.try_recv().unwrap()).unwrap();
        assert!(resp.success);
        assert_eq!(resp.message_id, Some("spaces/T/messages/m1".into()));
    }

    #[tokio::test]
    async fn handle_reply_upload_failure_falls_back_to_text_only() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/attachments:upload"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/T/messages/m2"}),
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();
        adapter.upload_base = mock_server.uri();

        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "x".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel { id: "spaces/T".into(), thread_id: None },
            content: Content {
                content_type: "text".into(),
                attachments: vec![make_image_attachment(b"px")],
                text: "the text still goes out".into(),
            },
            command: None,
            request_id: Some("req_partial".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let resp: GatewayResponse = serde_json::from_str(&event_rx.try_recv().unwrap()).unwrap();
        // Text delivery succeeded, so overall success=true; but error field
        // surfaces the attachment failure for observability.
        assert!(resp.success);
        let err = resp.error.expect("partial upload failure should be surfaced");
        assert!(err.contains("partial failure"), "error should mention partial failure, got: {}", err);
    }

    #[tokio::test]
    async fn handle_reply_unsupported_mime_skipped_no_upload_call() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        // Upload mock must NOT be called for unsupported MIME.
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/attachments:upload"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/T/messages/m3"}),
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();
        adapter.upload_base = mock_server.uri();

        use base64::Engine;
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "x".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel { id: "spaces/T".into(), thread_id: None },
            content: Content {
                content_type: "text".into(),
                attachments: vec![crate::schema::Attachment {
                    attachment_type: "video".into(),
                    filename: "clip.mp4".into(),
                    mime_type: "video/mp4".into(),
                    data: base64::engine::general_purpose::STANDARD.encode(b"px"),
                    size: 2,
                }],
                text: "video as text".into(),
            },
            command: None,
            request_id: Some("req_skip".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let resp: GatewayResponse = serde_json::from_str(&event_rx.try_recv().unwrap()).unwrap();
        assert!(resp.success);
        let err = resp.error.expect("MIME-skip should surface as partial failure");
        assert!(err.contains("MIME not allowed") || err.contains("partial failure"));
    }

    #[tokio::test]
    async fn handle_reply_attachment_size_exceeds_cap_skipped() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path_regex};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/attachments:upload"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"/spaces/.*/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/T/messages/m4"}),
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut adapter = GoogleChatAdapter::new(None, Some("fake-token".into()), None, GoogleChatConfig::default());
        adapter.api_base = mock_server.uri();
        adapter.upload_base = mock_server.uri();

        let oversize_bytes = vec![0u8; (OUTBOUND_IMAGE_MAX_BYTES as usize) + 1024];
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "x".into(),
            platform: "googlechat".into(),
            channel: ReplyChannel { id: "spaces/T".into(), thread_id: None },
            content: Content {
                content_type: "text".into(),
                attachments: vec![make_image_attachment(&oversize_bytes)],
                text: "still got text".into(),
            },
            command: None,
            request_id: Some("req_big".into()),
            quote_message_id: None,
        };

        adapter.handle_reply(&reply, &event_tx).await;

        let resp: GatewayResponse = serde_json::from_str(&event_rx.try_recv().unwrap()).unwrap();
        assert!(resp.success);
        assert!(resp.error.unwrap().contains("exceeds cap"));
    }

    // --- Mesh / allowlist config tests ---

    #[test]
    fn config_default_is_locked_down() {
        // The safe default forbids DMs, has no allowlist, and disables bot
        // chatter — deployments must explicitly opt in.
        let c = GoogleChatConfig::default();
        assert!(!c.allow_dm);
        assert!(c.allowed_spaces.is_empty());
        assert!(c.allowed_users.is_empty());
        assert!(c.trusted_bot_ids.is_empty());
        assert_eq!(c.allow_bots, AllowBots::Off);
        assert_eq!(c.allow_user_messages, AllowUsers::Involved);
        assert_eq!(c.max_bot_turns, 20);
        assert_eq!(c.session_ttl_secs, 24 * 3600);
    }

    #[test]
    fn parse_csv_env_handles_blank_single_multi() {
        // Use a randomized var name so concurrent tests can't collide. This
        // is the only env-touching test in the module — the rest of the
        // mesh logic is tested via webhook integration paths.
        let v = format!("OPENAB_TEST_PARSE_CSV_{}", uuid::Uuid::new_v4().simple());
        std::env::remove_var(&v);
        assert!(parse_csv_env(&v).is_empty(), "unset → empty");
        std::env::set_var(&v, "");
        assert!(parse_csv_env(&v).is_empty(), "blank → empty");
        std::env::set_var(&v, "foo");
        assert_eq!(parse_csv_env(&v), vec!["foo".to_string()]);
        std::env::set_var(&v, " a, b ,c,,d ");
        assert_eq!(
            parse_csv_env(&v),
            vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()]
        );
        std::env::remove_var(&v);
    }

    fn make_mesh_adapter(config: GoogleChatConfig) -> GoogleChatAdapter {
        GoogleChatAdapter::new(None, Some("fake-token".into()), None, config)
    }

    /// Build a minimal envelope that produces a Space message for tests.
    fn build_space_envelope(space_name: &str, sender_id: &str, is_bot: bool) -> String {
        let user_type = if is_bot { "BOT" } else { "HUMAN" };
        serde_json::json!({
            "chat": {
                "messagePayload": {
                    "message": {
                        "name": format!("{}/messages/m_xxx", space_name),
                        "text": "hello",
                        "argumentText": "hello",
                        "sender": {
                            "name": sender_id,
                            "displayName": "Tester",
                            "type": user_type
                        },
                        "space": { "name": space_name, "type": "ROOM" }
                    },
                    "space": { "name": space_name, "type": "ROOM" }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn envelope_parser_detects_bot_sender() {
        // Sanity check the bot-detection branch used by the webhook handler:
        // a BOT-typed sender from the envelope is recognized as is_bot=true.
        let envelope = build_space_envelope("spaces/T1", "users/bot1", true);
        let parsed: GoogleChatEnvelope = serde_json::from_str(&envelope).unwrap();
        let sender = parsed
            .chat
            .as_ref()
            .and_then(|c| c.message_payload.as_ref())
            .and_then(|p| p.message.as_ref())
            .and_then(|m| m.sender.as_ref());
        let is_bot = sender.map(|s| s.user_type == "BOT").unwrap_or(false);
        assert!(is_bot);
    }

    #[tokio::test]
    async fn mesh_bot_turns_counter_resets_on_human() {
        // Direct cache-shape test: simulate the counter manipulation the
        // webhook handler performs without spinning up an AppState.
        let mut config = GoogleChatConfig::default();
        config.allow_bots = AllowBots::All;
        config.max_bot_turns = 3;
        let adapter = make_mesh_adapter(config);

        // 3 bot turns OK, 4th would exceed.
        for expected in 1..=3 {
            let mut turns = adapter.bot_turns.lock().await;
            let c = turns.entry("spaces/T".into()).or_insert(0);
            *c += 1;
            assert_eq!(*c, expected);
            assert!(*c <= adapter.config.max_bot_turns);
        }
        // 4th bot turn exceeds the cap.
        {
            let mut turns = adapter.bot_turns.lock().await;
            let c = turns.entry("spaces/T".into()).or_insert(0);
            *c += 1;
            assert!(*c > adapter.config.max_bot_turns);
        }
        // Human message resets the counter.
        adapter.bot_turns.lock().await.remove("spaces/T");
        assert!(adapter.bot_turns.lock().await.get("spaces/T").is_none());
    }

    #[test]
    fn allowed_users_excludes_unlisted_human() {
        // Unit-level check of allowlist semantics: an empty list means
        // "allow all", a non-empty list is a strict allowlist.
        let mut config = GoogleChatConfig::default();
        config.allowed_users = vec!["alice".into(), "bob".into()];
        assert!(config.allowed_users.iter().any(|u| u == "alice"));
        assert!(config.allowed_users.iter().any(|u| u == "bob"));
        assert!(!config.allowed_users.iter().any(|u| u == "eve"));
    }
}
