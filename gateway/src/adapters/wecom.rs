use anyhow::Result;
use axum::extract::State;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub struct WecomConfig {
    pub corp_id: String,
    pub agent_id: String,
    pub secret: String,
    pub token: String,
    pub encoding_aes_key: String,
    pub webhook_path: String,
    pub group_require_mention: bool,
}

impl WecomConfig {
    pub fn from_env() -> Option<Self> {
        let corp_id = std::env::var("WECOM_CORP_ID").ok()?;
        let secret = std::env::var("WECOM_SECRET").ok()?;
        let token = std::env::var("WECOM_TOKEN").ok()?;
        let encoding_aes_key = std::env::var("WECOM_ENCODING_AES_KEY").ok()?;
        let agent_id = std::env::var("WECOM_AGENT_ID").unwrap_or_else(|_| "0".into());
        let webhook_path =
            std::env::var("WECOM_WEBHOOK_PATH").unwrap_or_else(|_| "/webhook/wecom".into());
        let group_require_mention = std::env::var("WECOM_GROUP_REQUIRE_MENTION")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        if encoding_aes_key.len() != 43 {
            warn!("WECOM_ENCODING_AES_KEY must be 43 characters, got {}", encoding_aes_key.len());
            return None;
        }

        info!(corp_id = %corp_id, agent_id = %agent_id, "wecom adapter configured");
        Some(Self {
            corp_id,
            agent_id,
            secret,
            token,
            encoding_aes_key,
            webhook_path,
            group_require_mention,
        })
    }
}

fn decode_aes_key(encoding_aes_key: &str) -> anyhow::Result<Vec<u8>> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    use base64::Engine;
    let padded = format!("{}=", encoding_aes_key);
    let config = GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true);
    let engine = GeneralPurpose::new(&base64::alphabet::STANDARD, config);
    engine
        .decode(&padded)
        .map_err(|e| anyhow::anyhow!("encoding_aes_key base64 decode failed: {e}"))
}

fn compute_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    use sha1::Digest;
    let mut parts = vec![token, timestamp, nonce, encrypt];
    parts.sort();
    let joined: String = parts.concat();
    let hash = sha1::Sha1::digest(joined.as_bytes());
    format!("{:x}", hash)
}

fn verify_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    encrypt: &str,
    expected: &str,
) -> bool {
    let computed = compute_signature(token, timestamp, nonce, encrypt);
    tracing::debug!(
        computed = %computed,
        expected = %expected,
        token_len = token.len(),
        encrypt_len = encrypt.len(),
        "signature comparison"
    );
    subtle::ConstantTimeEq::ct_eq(computed.as_bytes(), expected.as_bytes()).into()
}

fn decrypt_message(
    encoding_aes_key: &str,
    encrypted: &str,
    expected_corp_id: &str,
) -> anyhow::Result<String> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit};
    use base64::Engine;

    let key = decode_aes_key(encoding_aes_key)?;
    let iv = &key[..16];

    let cipher_bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted)
        .map_err(|e| anyhow::anyhow!("base64 decode failed: {e}"))?;

    if cipher_bytes.is_empty() || cipher_bytes.len() % 16 != 0 {
        anyhow::bail!("ciphertext length {} not a multiple of 16", cipher_bytes.len());
    }

    type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
    let decryptor = Aes256CbcDec::new_from_slices(&key, iv)
        .map_err(|e| anyhow::anyhow!("aes init failed: {e}"))?;

    let mut buf = cipher_bytes.to_vec();
    // WeCom uses PKCS7 with block_size=32, not 16. Decrypt without padding validation
    // and strip padding manually.
    let plaintext = decryptor
        .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buf)
        .map_err(|e| anyhow::anyhow!("aes decrypt failed: {e}"))?;

    // Strip WeCom PKCS7 padding (block_size=32): last byte indicates pad length (1-32)
    let pad_byte = *plaintext.last().ok_or_else(|| anyhow::anyhow!("empty plaintext"))? as usize;
    if pad_byte == 0 || pad_byte > 32 || pad_byte > plaintext.len() {
        anyhow::bail!("invalid wecom padding value: {pad_byte}");
    }
    let plaintext = &plaintext[..plaintext.len() - pad_byte];

    // Plaintext structure: random(16) + msg_len(4, big-endian) + msg + corp_id
    if plaintext.len() < 20 {
        anyhow::bail!("decrypted payload too short");
    }
    let msg_len =
        u32::from_be_bytes([plaintext[16], plaintext[17], plaintext[18], plaintext[19]]) as usize;
    if plaintext.len() < 20 + msg_len {
        anyhow::bail!("msg_len exceeds payload size");
    }
    let msg = &plaintext[20..20 + msg_len];
    let corp_id = &plaintext[20 + msg_len..];

    let corp_id_str =
        std::str::from_utf8(corp_id).map_err(|e| anyhow::anyhow!("corp_id not utf8: {e}"))?;
    if corp_id_str != expected_corp_id {
        anyhow::bail!("corp_id mismatch: expected {expected_corp_id}, got {corp_id_str}");
    }

    String::from_utf8(msg.to_vec()).map_err(|e| anyhow::anyhow!("message not utf8: {e}"))
}

// --- Deduplication ---

const DEDUPE_TTL_SECS: u64 = 30;
const DEDUPE_MAX_SIZE: usize = 10_000;

struct DedupeCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl DedupeCache {
    fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn check_and_insert(&self, msg_id: &str) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();

        if entries.len() >= DEDUPE_MAX_SIZE {
            entries.retain(|_, t| now.duration_since(*t).as_secs() < DEDUPE_TTL_SECS);
        }

        if let Some(t) = entries.get(msg_id) {
            if now.duration_since(*t).as_secs() < DEDUPE_TTL_SECS {
                return false;
            }
        }

        entries.insert(msg_id.to_string(), now);
        true
    }
}

// --- Token cache ---

pub const WECOM_API_BASE: &str = "https://qyapi.weixin.qq.com";
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;

pub struct WecomTokenCache {
    inner: RwLock<Option<(String, std::time::Instant, u64)>>,
    base_url: String,
}

impl WecomTokenCache {
    fn new() -> Self {
        Self {
            inner: RwLock::new(None),
            base_url: WECOM_API_BASE.into(),
        }
    }

    #[cfg(test)]
    fn with_base_url(base_url: String) -> Self {
        Self {
            inner: RwLock::new(None),
            base_url,
        }
    }

    pub async fn get_token(
        &self,
        client: &reqwest::Client,
        corp_id: &str,
        secret: &str,
    ) -> Result<String> {
        // Fast path: read lock
        {
            let guard = self.inner.read().await;
            if let Some((ref token, created_at, expires_in)) = *guard {
                let elapsed = created_at.elapsed().as_secs();
                if elapsed + TOKEN_REFRESH_MARGIN_SECS < expires_in {
                    return Ok(token.clone());
                }
            }
        }

        // Slow path: write lock + refresh
        let mut guard = self.inner.write().await;
        // Double-check after acquiring write lock
        if let Some((ref token, created_at, expires_in)) = *guard {
            let elapsed = created_at.elapsed().as_secs();
            if elapsed + TOKEN_REFRESH_MARGIN_SECS < expires_in {
                return Ok(token.clone());
            }
        }

        let url = format!(
            "{}/cgi-bin/gettoken?corpid={}&corpsecret={}",
            self.base_url, corp_id, secret
        );
        let resp: serde_json::Value = client.get(&url).send().await?.json().await?;

        let errcode = resp["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            anyhow::bail!(
                "wecom gettoken failed: errcode={}, errmsg={}",
                errcode,
                resp["errmsg"]
            );
        }

        let token = resp["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing access_token in response"))?
            .to_string();
        let expires_in = resp["expires_in"].as_u64().unwrap_or(7200);

        *guard = Some((token.clone(), std::time::Instant::now(), expires_in));
        Ok(token)
    }

    pub async fn force_refresh(
        &self,
        client: &reqwest::Client,
        corp_id: &str,
        secret: &str,
    ) -> Result<String> {
        let mut guard = self.inner.write().await;
        *guard = None;
        drop(guard);
        self.get_token(client, corp_id, secret).await
    }
}

// --- Adapter ---

pub struct WecomAdapter {
    pub config: WecomConfig,
    pub token_cache: WecomTokenCache,
    client: reqwest::Client,
    dedupe: DedupeCache,
}

impl WecomAdapter {
    pub fn new(config: WecomConfig) -> Self {
        Self {
            token_cache: WecomTokenCache::new(),
            client: reqwest::Client::new(),
            dedupe: DedupeCache::new(),
            config,
        }
    }

    pub async fn handle_reply(
        &self,
        reply: &crate::schema::GatewayReply,
        event_tx: &tokio::sync::broadcast::Sender<String>,
    ) {
        if let Some(cmd) = reply.command.as_deref() {
            match cmd {
                "add_reaction" | "remove_reaction" | "create_topic" | "edit_message" => {
                    info!(command = cmd, "wecom: ignoring unsupported command");
                    return;
                }
                _ => {}
            }
        }

        let text = &reply.content.text;
        if text.is_empty() {
            return;
        }

        let to_user = reply
            .channel
            .id
            .rsplit(':')
            .next()
            .unwrap_or(&reply.channel.id);

        info!(to_user = to_user, "wecom: sending reply");
        let chunks = split_text_lines(text, 2048);
        let mut msg_id = None;

        for chunk in &chunks {
            match self.send_text(to_user, chunk).await {
                Ok(id) => {
                    if msg_id.is_none() {
                        msg_id = Some(id);
                    }
                }
                Err(e) => warn!("wecom send failed: {e}"),
            }
        }

        if let Some(ref req_id) = reply.request_id {
            let resp = crate::schema::GatewayResponse {
                schema: "openab.gateway.response.v1".into(),
                request_id: req_id.clone(),
                success: msg_id.is_some(),
                thread_id: None,
                message_id: msg_id,
                error: None,
            };
            if let Ok(json) = serde_json::to_string(&resp) {
                let _ = event_tx.send(json);
            }
        }
    }

    async fn send_text(&self, to_user: &str, text: &str) -> Result<String> {
        let token = self
            .token_cache
            .get_token(&self.client, &self.config.corp_id, &self.config.secret)
            .await?;

        let agent_id = self.config.agent_id.parse::<u64>().unwrap_or(0);
        let body = serde_json::json!({
            "touser": to_user,
            "msgtype": "text",
            "agentid": agent_id,
            "text": { "content": text }
        });

        let resp = self.do_send(&token, &body).await?;
        let errcode = resp["errcode"].as_i64().unwrap_or(-1);

        if errcode == 42001 {
            let new_token = self
                .token_cache
                .force_refresh(&self.client, &self.config.corp_id, &self.config.secret)
                .await?;
            let retry_resp = self.do_send(&new_token, &body).await?;
            let retry_code = retry_resp["errcode"].as_i64().unwrap_or(-1);
            if retry_code != 0 {
                anyhow::bail!("wecom send retry failed: {}", retry_resp["errmsg"]);
            }
            Ok(retry_resp["msgid"].as_str().unwrap_or("").to_string())
        } else if errcode != 0 {
            anyhow::bail!(
                "wecom send failed: errcode={}, errmsg={}",
                errcode,
                resp["errmsg"]
            );
        } else {
            Ok(resp["msgid"].as_str().unwrap_or("").to_string())
        }
    }

    async fn do_send(
        &self,
        token: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/cgi-bin/message/send?access_token={}",
            self.token_cache.base_url, token
        );
        Ok(self.client.post(&url).json(body).send().await?.json().await?)
    }
}

// --- Handlers ---

fn handle_verify_request(
    token: &str,
    encoding_aes_key: &str,
    corp_id: &str,
    msg_signature: &str,
    timestamp: &str,
    nonce: &str,
    echostr: &str,
) -> anyhow::Result<String> {
    if !verify_signature(token, timestamp, nonce, echostr, msg_signature) {
        anyhow::bail!("signature verification failed");
    }
    decrypt_message(encoding_aes_key, echostr, corp_id)
}

// --- XML parsing ---

#[allow(dead_code)]
struct CallbackEnvelope {
    to_user_name: String,
    encrypt: String,
}

struct WecomMessage {
    from_user: String,
    msg_type: String,
    content: String,
    msg_id: String,
}

fn parse_envelope_xml(xml: &str) -> Result<CallbackEnvelope> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut to_user_name = String::new();
    let mut encrypt = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
            }
            Ok(Event::CData(e)) => {
                let text = String::from_utf8_lossy(&e).to_string();
                match current_tag.as_str() {
                    "ToUserName" => to_user_name = text,
                    "Encrypt" => encrypt = text,
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "ToUserName" => {
                        if to_user_name.is_empty() {
                            to_user_name = text;
                        }
                    }
                    "Encrypt" => {
                        if encrypt.is_empty() {
                            encrypt = text;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(_)) => {
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("xml parse error: {e}"),
            _ => {}
        }
    }

    if encrypt.is_empty() {
        anyhow::bail!("missing Encrypt field in callback XML");
    }
    Ok(CallbackEnvelope {
        to_user_name,
        encrypt,
    })
}

fn parse_message_xml(xml: &str) -> Result<WecomMessage> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut from_user = String::new();
    let mut msg_type = String::new();
    let mut content = String::new();
    let mut msg_id = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
            }
            Ok(Event::CData(e)) => {
                let text = String::from_utf8_lossy(&e).to_string();
                match current_tag.as_str() {
                    "FromUserName" => from_user = text,
                    "MsgType" => msg_type = text,
                    "Content" => content = text,
                    "MsgId" => msg_id = text,
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "FromUserName" => {
                        if from_user.is_empty() {
                            from_user = text;
                        }
                    }
                    "MsgType" => {
                        if msg_type.is_empty() {
                            msg_type = text;
                        }
                    }
                    "Content" => {
                        if content.is_empty() {
                            content = text;
                        }
                    }
                    "MsgId" => {
                        if msg_id.is_empty() {
                            msg_id = text;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(_)) => {
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("xml parse error: {e}"),
            _ => {}
        }
    }

    Ok(WecomMessage {
        from_user,
        msg_type,
        content,
        msg_id,
    })
}

fn strip_bot_mention(content: &str) -> String {
    let trimmed = content.trim_start();
    if trimmed.starts_with('@') {
        if let Some(rest) = trimmed.split_once(|c: char| c.is_whitespace()) {
            return rest.1.to_string();
        }
    }
    content.to_string()
}

fn split_text(text: &str, limit: usize) -> Vec<&str> {
    if text.len() <= limit {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + limit).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

fn split_text_lines(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let candidate_len = if current.is_empty() {
            line.len()
        } else {
            current.len() + 1 + line.len()
        };
        if candidate_len > limit && !current.is_empty() {
            chunks.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub async fn verify(
    State(state): State<Arc<crate::AppState>>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let wecom = match state.wecom.as_ref() {
        Some(w) => w,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let msg_signature = query.get("msg_signature").map(|s| s.as_str()).unwrap_or("");
    let timestamp = query.get("timestamp").map(|s| s.as_str()).unwrap_or("");
    let nonce = query.get("nonce").map(|s| s.as_str()).unwrap_or("");
    let echostr = query.get("echostr").map(|s| s.as_str()).unwrap_or("");

    info!(
        msg_signature = %msg_signature,
        timestamp = %timestamp,
        nonce = %nonce,
        echostr_len = echostr.len(),
        "wecom verify request received"
    );

    match handle_verify_request(
        &wecom.config.token,
        &wecom.config.encoding_aes_key,
        &wecom.config.corp_id,
        msg_signature,
        timestamp,
        nonce,
        echostr,
    ) {
        Ok(plaintext) => plaintext.into_response(),
        Err(e) => {
            warn!("wecom callback verification failed: {e}");
            axum::http::StatusCode::FORBIDDEN.into_response()
        }
    }
}

pub async fn webhook(
    State(state): State<Arc<crate::AppState>>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let wecom = match state.wecom.as_ref() {
        Some(w) => w,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let msg_signature = query.get("msg_signature").map(|s| s.as_str()).unwrap_or("");
    let timestamp = query.get("timestamp").map(|s| s.as_str()).unwrap_or("");
    let nonce = query.get("nonce").map(|s| s.as_str()).unwrap_or("");

    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
    };

    let envelope = match parse_envelope_xml(body_str) {
        Ok(e) => e,
        Err(e) => {
            warn!("wecom envelope parse error: {e}");
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
    };

    if !verify_signature(
        &wecom.config.token,
        timestamp,
        nonce,
        &envelope.encrypt,
        msg_signature,
    ) {
        warn!("wecom webhook signature verification failed");
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    info!(encrypt_len = envelope.encrypt.len(), "wecom: decrypting callback");
    let decrypted = match decrypt_message(
        &wecom.config.encoding_aes_key,
        &envelope.encrypt,
        &wecom.config.corp_id,
    ) {
        Ok(d) => {
            info!("wecom: decrypt ok");
            d
        }
        Err(e) => {
            warn!(encrypt_len = envelope.encrypt.len(), "wecom decrypt failed: {e}");
            return "success".into_response();
        }
    };

    let msg = match parse_message_xml(&decrypted) {
        Ok(m) => m,
        Err(e) => {
            warn!("wecom message parse error: {e}");
            return "success".into_response();
        }
    };

    if msg.msg_type != "text" {
        return "success".into_response();
    }

    if !wecom.dedupe.check_and_insert(&msg.msg_id) {
        return "success".into_response();
    }

    let text = if wecom.config.group_require_mention {
        strip_bot_mention(&msg.content)
    } else {
        msg.content.clone()
    };

    if text.trim().is_empty() {
        return "success".into_response();
    }

    let channel_id = format!("wecom:{}:{}", wecom.config.corp_id, msg.from_user);
    let event = crate::schema::GatewayEvent::new(
        "wecom",
        crate::schema::ChannelInfo {
            id: channel_id,
            channel_type: "direct".into(),
            thread_id: None,
        },
        crate::schema::SenderInfo {
            id: msg.from_user.clone(),
            name: msg.from_user.clone(),
            display_name: msg.from_user.clone(),
            is_bot: false,
        },
        &text,
        &msg.msg_id,
        vec![],
    );

    if let Ok(json) = serde_json::to_string(&event) {
        let _ = state.event_tx.send(json);
    }

    "success".into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_env_all_present() {
        std::env::set_var("WECOM_CORP_ID", "ww_test_corp");
        std::env::set_var("WECOM_SECRET", "test_secret");
        std::env::set_var("WECOM_TOKEN", "test_token");
        std::env::set_var("WECOM_ENCODING_AES_KEY", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG");
        std::env::set_var("WECOM_AGENT_ID", "1000002");

        let config = WecomConfig::from_env().unwrap();
        assert_eq!(config.corp_id, "ww_test_corp");
        assert_eq!(config.agent_id, "1000002");
        assert_eq!(config.webhook_path, "/webhook/wecom");
        assert!(config.group_require_mention);

        std::env::remove_var("WECOM_CORP_ID");
        std::env::remove_var("WECOM_SECRET");
        std::env::remove_var("WECOM_TOKEN");
        std::env::remove_var("WECOM_ENCODING_AES_KEY");
        std::env::remove_var("WECOM_AGENT_ID");
    }

    #[test]
    fn config_from_env_missing_required() {
        std::env::remove_var("WECOM_CORP_ID");
        std::env::remove_var("WECOM_SECRET");
        std::env::remove_var("WECOM_TOKEN");
        std::env::remove_var("WECOM_ENCODING_AES_KEY");
        assert!(WecomConfig::from_env().is_none());
    }

    fn encrypt_for_test(encoding_aes_key: &str, msg: &str, corp_id: &str) -> String {
        use aes::cipher::{BlockEncryptMut, KeyIvInit};
        use base64::Engine;

        let key = decode_aes_key(encoding_aes_key).unwrap();
        let iv = &key[..16];

        let msg_bytes = msg.as_bytes();
        let corp_id_bytes = corp_id.as_bytes();
        let msg_len = (msg_bytes.len() as u32).to_be_bytes();

        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(&[0u8; 16]); // random bytes (zeros for test)
        plaintext.extend_from_slice(&msg_len);
        plaintext.extend_from_slice(msg_bytes);
        plaintext.extend_from_slice(corp_id_bytes);

        // WeCom uses PKCS7 padding with block_size=32
        let block_size = 32;
        let pad_len = block_size - (plaintext.len() % block_size);
        for _ in 0..pad_len {
            plaintext.push(pad_len as u8);
        }

        // Encrypt with NoPadding since we already padded manually
        let total_len = plaintext.len();
        let mut buf = vec![0u8; total_len + 16]; // extra space just in case
        buf[..total_len].copy_from_slice(&plaintext);

        type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
        let encryptor = Aes256CbcEnc::new_from_slices(&key, iv).unwrap();
        let encrypted = encryptor
            .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buf, total_len)
            .unwrap();

        base64::engine::general_purpose::STANDARD.encode(encrypted)
    }

    #[test]
    fn aes_key_decode() {
        let key_str = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let key_bytes = decode_aes_key(key_str).unwrap();
        assert_eq!(key_bytes.len(), 32);
    }

    #[test]
    fn signature_verify() {
        let token = "testtoken";
        let timestamp = "1409659813";
        let nonce = "1372623149";
        let encrypt = "msg_encrypt_content";

        let sig = compute_signature(token, timestamp, nonce, encrypt);
        assert!(verify_signature(token, timestamp, nonce, encrypt, &sig));
        assert!(!verify_signature(
            token,
            timestamp,
            nonce,
            encrypt,
            "wrong_signature_value_here"
        ));
    }

    #[test]
    fn decrypt_wecom_payload() {
        let key_str = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";
        let msg = "hello world";

        let encrypted = encrypt_for_test(key_str, msg, corp_id);
        let decrypted = decrypt_message(key_str, &encrypted, corp_id).unwrap();
        assert_eq!(decrypted, msg);
    }

    #[test]
    fn verify_callback_echostr() {
        let token = "testtoken";
        let encoding_aes_key = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";
        let echostr_plain = "success_echo_string";

        let echostr_encrypted = encrypt_for_test(encoding_aes_key, echostr_plain, corp_id);
        let sig = compute_signature(token, "1409659813", "nonce123", &echostr_encrypted);

        let result = handle_verify_request(
            token,
            encoding_aes_key,
            corp_id,
            &sig,
            "1409659813",
            "nonce123",
            &echostr_encrypted,
        );
        assert_eq!(result.unwrap(), echostr_plain);
    }

    #[test]
    fn parse_text_message_xml() {
        let xml = r#"<xml><ToUserName><![CDATA[ww_test_corp]]></ToUserName><FromUserName><![CDATA[user001]]></FromUserName><CreateTime>1348831860</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[hello bot]]></Content><MsgId>1234567890123456</MsgId><AgentID>1000002</AgentID></xml>"#;

        let msg = parse_message_xml(xml).unwrap();
        assert_eq!(msg.from_user, "user001");
        assert_eq!(msg.msg_type, "text");
        assert_eq!(msg.content, "hello bot");
        assert_eq!(msg.msg_id, "1234567890123456");
    }

    #[test]
    fn parse_callback_envelope() {
        let xml = r#"<xml><ToUserName><![CDATA[ww_test_corp]]></ToUserName><Encrypt><![CDATA[some_encrypted_base64]]></Encrypt><AgentID><![CDATA[1000002]]></AgentID></xml>"#;

        let envelope = parse_envelope_xml(xml).unwrap();
        assert_eq!(envelope.to_user_name, "ww_test_corp");
        assert_eq!(envelope.encrypt, "some_encrypted_base64");
    }

    #[test]
    fn strip_bot_mention_removes_prefix() {
        assert_eq!(strip_bot_mention("@Bot hello"), "hello");
        assert_eq!(strip_bot_mention("no mention"), "no mention");
        assert_eq!(strip_bot_mention("@OnlyMention"), "@OnlyMention");
    }

    #[test]
    fn dedupe_rejects_duplicates() {
        let cache = DedupeCache::new();
        assert!(cache.check_and_insert("msg_001"));
        assert!(!cache.check_and_insert("msg_001"));
        assert!(cache.check_and_insert("msg_002"));
    }

    #[tokio::test]
    async fn token_refresh_success() {
        use wiremock::matchers::{method, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("corpid", "ww_test_corp"))
            .and(query_param("corpsecret", "test_secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errcode": 0,
                "errmsg": "ok",
                "access_token": "test_token_abc",
                "expires_in": 7200
            })))
            .expect(1)
            .mount(&server)
            .await;

        let cache = WecomTokenCache::with_base_url(server.uri());
        let client = reqwest::Client::new();
        let token = cache.get_token(&client, "ww_test_corp", "test_secret").await.unwrap();
        assert_eq!(token, "test_token_abc");

        // Second call uses cache (mock expects exactly 1 call)
        let token2 = cache.get_token(&client, "ww_test_corp", "test_secret").await.unwrap();
        assert_eq!(token2, "test_token_abc");
    }

    #[test]
    fn split_text_utf8_safe() {
        let text = "你好世界"; // 12 bytes (3 bytes per char)
        let chunks = split_text(text, 6);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "你好");
        assert_eq!(chunks[1], "世界");
    }

    #[test]
    fn split_text_within_limit() {
        let text = "short";
        let chunks = split_text(text, 100);
        assert_eq!(chunks, vec!["short"]);
    }

    #[test]
    fn full_webhook_decrypt_and_parse() {
        let token = "testtoken";
        let encoding_aes_key = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";
        let timestamp = "1409659813";
        let nonce = "nonce123";

        // Simulate the inner message
        let inner_xml = "<xml><ToUserName><![CDATA[ww_test_corp]]></ToUserName><FromUserName><![CDATA[user42]]></FromUserName><CreateTime>1348831860</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[ping]]></Content><MsgId>9999</MsgId><AgentID>1000002</AgentID></xml>";

        // Encrypt it
        let encrypted = encrypt_for_test(encoding_aes_key, inner_xml, corp_id);

        // Compute signature
        let sig = compute_signature(token, timestamp, nonce, &encrypted);

        // Verify signature
        assert!(verify_signature(token, timestamp, nonce, &encrypted, &sig));

        // Decrypt
        let decrypted = decrypt_message(encoding_aes_key, &encrypted, corp_id).unwrap();
        assert_eq!(decrypted, inner_xml);

        // Parse
        let msg = parse_message_xml(&decrypted).unwrap();
        assert_eq!(msg.from_user, "user42");
        assert_eq!(msg.msg_type, "text");
        assert_eq!(msg.content, "ping");
        assert_eq!(msg.msg_id, "9999");
    }

    #[test]
    fn full_webhook_non_text_skipped() {
        let encoding_aes_key = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";

        let inner_xml = "<xml><ToUserName><![CDATA[ww_test_corp]]></ToUserName><FromUserName><![CDATA[user42]]></FromUserName><CreateTime>1348831860</CreateTime><MsgType><![CDATA[image]]></MsgType><PicUrl><![CDATA[http://example.com/pic.jpg]]></PicUrl><MsgId>8888</MsgId><AgentID>1000002</AgentID></xml>";

        let encrypted = encrypt_for_test(encoding_aes_key, inner_xml, corp_id);
        let decrypted = decrypt_message(encoding_aes_key, &encrypted, corp_id).unwrap();
        let msg = parse_message_xml(&decrypted).unwrap();
        assert_eq!(msg.msg_type, "image");
        // In the real handler, this would return "success" without broadcasting
    }

    #[test]
    fn verify_rejects_wrong_signature() {
        let token = "testtoken";
        let encoding_aes_key = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";
        let echostr_plain = "test_echo";

        let echostr_encrypted = encrypt_for_test(encoding_aes_key, echostr_plain, corp_id);

        let result = handle_verify_request(
            token,
            encoding_aes_key,
            corp_id,
            "completely_wrong_signature",
            "1409659813",
            "nonce123",
            &echostr_encrypted,
        );
        assert!(result.is_err());
    }

    #[test]
    fn decrypt_with_large_padding_value() {
        // Verifies decryption works when WeCom's 32-byte padding exceeds 16
        let encoding_aes_key = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";
        // Choose a message where (16 + 4 + msg_len + corp_id_len) % 32 < 16,
        // producing a pad value > 16 which would fail with PKCS7/block_size=16.
        // 16 + 4 + 1 + 12 = 33 → 33 % 32 = 1 → pad = 31
        let msg = "x";
        let encrypted = encrypt_for_test(encoding_aes_key, msg, corp_id);
        let decrypted = decrypt_message(encoding_aes_key, &encrypted, corp_id).unwrap();
        assert_eq!(decrypted, msg);
    }

    #[test]
    fn decrypt_rejects_wrong_corp_id() {
        let encoding_aes_key = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE";
        let corp_id = "ww_test_corp";
        let msg = "hello";

        let encrypted = encrypt_for_test(encoding_aes_key, msg, corp_id);
        let result = decrypt_message(encoding_aes_key, &encrypted, "ww_other_corp");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("corp_id mismatch"));
    }
}
