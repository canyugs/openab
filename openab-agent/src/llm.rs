use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::Arc;

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Tool definition sent to the LLM.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Events streamed back from the LLM.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Stop,
    #[allow(dead_code)]
    Error(String),
}

/// Trait for LLM providers.
pub trait LlmProvider: Send + Sync {
    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>>;

    /// Identifier of the model this provider talks to. Surfaced as
    /// `CreateMessageResult.model` when serving MCP sampling so the requesting
    /// server learns which model produced the response.
    fn model(&self) -> &str;
}

/// Shared, cloneable handle to an `LlmProvider`. A newtype over
/// `Arc<dyn LlmProvider>` purely so structs that hold one (the MCP runtime
/// manager + per-connection client handler) can keep deriving `Debug` —
/// `dyn LlmProvider` is not `Debug`, so the derive would otherwise fail.
#[derive(Clone)]
pub struct SharedLlmProvider(pub Arc<dyn LlmProvider>);

impl std::fmt::Debug for SharedLlmProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedLlmProvider(..)")
    }
}

impl std::ops::Deref for SharedLlmProvider {
    type Target = dyn LlmProvider;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Select an `LlmProvider` from an explicit `choice` (`anthropic` /
/// `anthropic-oauth` / `openai` / `codex`) or, for any other value, auto-detect
/// (Anthropic API key, then Claude subscription OAuth, then codex OAuth). The
/// `anthropic` choice itself auto-falls-back from API key to OAuth. Shared by
/// the ACP session path and MCP sampling so both honor the same
/// `OPENAB_AGENT_PROVIDER` selection and credential fallback.
pub fn select_provider(choice: &str) -> Result<Box<dyn LlmProvider>, String> {
    match choice {
        "anthropic" => Ok(Box::new(AnthropicProvider::auto()?)),
        "anthropic-oauth" | "claude" => Ok(Box::new(AnthropicProvider::from_oauth_store()?)),
        "openai" | "codex" => Ok(Box::new(OpenAiProvider::from_auth_store()?)),
        _ => match AnthropicProvider::auto() {
            Ok(p) => Ok(Box::new(p)),
            Err(_) => match OpenAiProvider::from_auth_store() {
                Ok(p) => Ok(Box::new(p)),
                Err(e) => Err(format!(
                    "No credentials: set ANTHROPIC_API_KEY, or run `openab-agent auth anthropic-oauth` / `auth codex-oauth`. {e}"
                )),
            },
        },
    }
}

/// Build the default shared provider for non-session background use (MCP
/// sampling). Honors `OPENAB_AGENT_PROVIDER`; returns `None` when no
/// credentials are available so the caller can simply decline to advertise
/// the `sampling` capability rather than fail.
pub fn default_provider() -> Option<SharedLlmProvider> {
    let choice = std::env::var("OPENAB_AGENT_PROVIDER").unwrap_or_default();
    select_provider(&choice)
        .ok()
        .map(|b| SharedLlmProvider(Arc::from(b)))
}

/// How an `AnthropicProvider` authenticates to the Messages API.
enum AnthropicAuth {
    /// `ANTHROPIC_API_KEY` → `x-api-key`, plain system prompt.
    ApiKey(String),
    /// Claude Pro/Max subscription OAuth → `Bearer` + Claude Code identity
    /// headers/system block. The live token is fetched (and refreshed) per call
    /// from the `anthropic-oauth` tenant in auth.json.
    OAuth,
}

/// Anthropic Claude provider.
pub struct AnthropicProvider {
    auth: AnthropicAuth,
    model: String,
    max_tokens: u32,
    client: reqwest::Client,
}

fn anthropic_model_from_env() -> String {
    std::env::var("OPENAB_AGENT_MODEL").unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string())
}

fn anthropic_max_tokens() -> u32 {
    std::env::var("OPENAB_AGENT_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192)
}

/// openab-agent's built-in tools mapped to Claude Code's canonical casing. The
/// `claude-code-20250219` beta (sent with OAuth tokens) expects these names, so
/// they're rewritten on the way out and restored on the way back. Unknown names
/// (e.g. MCP tools) pass through unchanged, matching Pi's behaviour.
const CC_TOOL_NAMES: &[(&str, &str)] = &[
    ("read", "Read"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("bash", "Bash"),
];

fn to_claude_code_name(name: &str) -> String {
    CC_TOOL_NAMES
        .iter()
        .find(|(lc, _)| *lc == name)
        .map(|(_, cc)| (*cc).to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str) -> String {
    CC_TOOL_NAMES
        .iter()
        .find(|(_, cc)| *cc == name)
        .map(|(lc, _)| (*lc).to_string())
        .unwrap_or_else(|| name.to_string())
}

impl AnthropicProvider {
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;
        if api_key.is_empty() {
            return Err("ANTHROPIC_API_KEY is empty".to_string());
        }
        Ok(Self {
            auth: AnthropicAuth::ApiKey(api_key),
            model: anthropic_model_from_env(),
            max_tokens: anthropic_max_tokens(),
            client: reqwest::Client::new(),
        })
    }

    /// Claude Pro/Max OAuth. Verifies a stored `anthropic-oauth` token exists;
    /// the live token is fetched (and refreshed) at call time.
    pub fn from_oauth_store() -> Result<Self, String> {
        crate::auth::load_tokens_for(crate::auth::ANTHROPIC_NAMESPACE)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            auth: AnthropicAuth::OAuth,
            model: anthropic_model_from_env(),
            max_tokens: anthropic_max_tokens(),
            client: reqwest::Client::new(),
        })
    }

    /// Prefer an explicit API key, else a stored Claude subscription OAuth token.
    pub fn auto() -> Result<Self, String> {
        Self::from_env().or_else(|_| Self::from_oauth_store())
    }

    /// `auto()` with an explicit model override.
    pub fn auto_with_model(model: &str) -> Result<Self, String> {
        let mut p = Self::auto()?;
        p.model = model.to_string();
        Ok(p)
    }

    /// `from_oauth_store()` with an explicit model override.
    pub fn from_oauth_store_with_model(model: &str) -> Result<Self, String> {
        let mut p = Self::from_oauth_store()?;
        p.model = model.to_string();
        Ok(p)
    }

    fn is_oauth(&self) -> bool {
        matches!(self.auth, AnthropicAuth::OAuth)
    }

    fn build_request_body(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> Value {
        let oauth = self.is_oauth();
        let msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let content: Vec<Value> = m
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
                        ContentBlock::ToolUse { id, name, input } => {
                            let name = if oauth { to_claude_code_name(name) } else { name.clone() };
                            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let mut v = json!({
                                "type": "tool_result",
                                "tool_use_id": tool_use_id,
                                "content": content
                            });
                            if let Some(true) = is_error {
                                v["is_error"] = json!(true);
                            }
                            v
                        }
                    })
                    .collect();
                json!({ "role": &m.role, "content": content })
            })
            .collect();

        let mut body = json!({
            "model": &self.model,
            "max_tokens": self.max_tokens,
            "messages": msgs,
        });

        // OAuth tokens MUST carry the Claude Code identity as the first system
        // block, with the real prompt appended. API-key callers send a plain
        // string (unchanged behaviour).
        if oauth {
            body["system"] = json!([
                { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
                { "type": "text", "text": system },
            ]);
        } else {
            body["system"] = json!(system);
        }

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    let name = if oauth { to_claude_code_name(&t.name) } else { t.name.clone() };
                    json!({
                        "name": name,
                        "description": &t.description,
                        "input_schema": &t.input_schema
                    })
                })
                .collect();
            body["tools"] = json!(tool_defs);
        }

        body
    }
}

impl LlmProvider for AnthropicProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>> {
        Box::pin(async move {
            let body = self.build_request_body(system, messages, tools);
            let oauth = self.is_oauth();
            let max_retries = 3u32;

            for attempt in 0..=max_retries {
                let mut req = self
                    .client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                req = match &self.auth {
                    AnthropicAuth::ApiKey(key) => req.header("x-api-key", key),
                    AnthropicAuth::OAuth => {
                        // Claude Pro/Max: Bearer + Claude Code identity headers.
                        let token = crate::auth::get_valid_token_for(
                            crate::auth::ANTHROPIC_NAMESPACE,
                        )
                        .await?;
                        req.header("authorization", format!("Bearer {token}"))
                            .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
                            .header("user-agent", "claude-cli/1.0.0")
                            .header("x-app", "cli")
                            .header("anthropic-dangerous-direct-browser-access", "true")
                    }
                };

                let resp = req
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow!("HTTP request failed: {e}"))?;

                let status = resp.status();

                // Retry on 429 (rate limit) or 529 (overloaded)
                if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < max_retries {
                    let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                    continue;
                }

                // 401 on OAuth: token may have expired mid-request; force a
                // refresh and retry once before surfacing the error.
                if oauth && status.as_u16() == 401 && attempt < max_retries {
                    let _ =
                        crate::auth::force_refresh_for(crate::auth::ANTHROPIC_NAMESPACE).await;
                    continue;
                }

                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("Anthropic API error {status}: {text}"));
                }

                let response: Value = resp
                    .json()
                    .await
                    .map_err(|e| anyhow!("Failed to parse response: {e}"))?;

                let mut events = parse_anthropic_response(&response)?;
                // Restore openab-agent's lowercase tool names from the Claude
                // Code canonical casing the model echoes back under OAuth.
                if oauth {
                    for ev in &mut events {
                        if let LlmEvent::ToolUse { name, .. } = ev {
                            *name = from_claude_code_name(name);
                        }
                    }
                }
                return Ok(events);
            }

            Err(anyhow!("Anthropic API: max retries exceeded"))
        })
    }
}

fn parse_anthropic_response(response: &Value) -> Result<Vec<LlmEvent>> {
    let mut events = Vec::new();

    let content = response
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("missing content in response"))?;

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    events.push(LlmEvent::Text(text.to_string()));
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(json!({}));
                events.push(LlmEvent::ToolUse { id, name, input });
            }
            _ => {}
        }
    }

    let stop_reason = response
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .unwrap_or("end_turn");

    if stop_reason != "tool_use" {
        events.push(LlmEvent::Stop);
    }

    Ok(events)
}

// === OpenAI-compatible Provider (for Codex subscription via OAuth) ===

pub struct OpenAiProvider {
    base_url: String,
    model: String,
    #[allow(dead_code)]
    max_tokens: u32,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Create provider using stored OAuth token from ~/.openab/agent/auth.json
    pub fn from_auth_store() -> Result<Self, String> {
        // Just verify tokens exist; actual token is fetched at call time
        crate::auth::load_tokens().map_err(|e| e.to_string())?;
        Ok(Self {
            base_url: std::env::var("OPENAB_AGENT_OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()),
            model: std::env::var("OPENAB_AGENT_OPENAI_MODEL")
                .or_else(|_| std::env::var("OPENAB_AGENT_MODEL"))
                .unwrap_or_else(|_| "gpt-5.4-mini".to_string()),
            max_tokens: std::env::var("OPENAB_AGENT_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8192),
            client: reqwest::Client::new(),
        })
    }

    /// Create provider with a specific model override.
    pub fn from_auth_store_with_model(model: &str) -> Result<Self, String> {
        let mut p = Self::from_auth_store()?;
        p.model = model.to_string();
        Ok(p)
    }
}

impl LlmProvider for OpenAiProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>> {
        Box::pin(async move {
            // Build Responses API input format
            let mut oai_messages: Vec<Value> = vec![];
            for m in messages {
                if m.role == "user" {
                    // User text messages
                    let texts: Vec<&str> = m
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !texts.is_empty() {
                        oai_messages.push(json!({"role": "user", "content": [{"type": "input_text", "text": texts.join("")}]}));
                    }
                    // Tool results as function_call_output
                    for b in &m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            oai_messages.push(json!({"type": "function_call_output", "call_id": tool_use_id, "output": content}));
                        }
                    }
                } else if m.role == "assistant" {
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text } => {
                                oai_messages.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                oai_messages.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": input.to_string()}));
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Build Responses API body
            let mut body = json!({
                "model": &self.model,
                "store": false,
                "stream": true,
                "instructions": system,
                "input": oai_messages,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
            });

            if !tools.is_empty() {
                let resp_tools: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": &t.name,
                            "description": &t.description,
                            "parameters": &t.input_schema
                        })
                    })
                    .collect();
                body["tools"] = json!(resp_tools);
            }

            let max_retries = 3u32;
            for attempt in 0..=max_retries {
                let token = crate::auth::get_valid_token().await?;
                // Extract account ID from JWT for chatgpt backend API
                let account_id = extract_account_id_from_jwt(&token);
                let mut req = self
                    .client
                    .post(format!("{}/codex/responses", self.base_url))
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .header("originator", "openab-agent");
                if let Some(ref aid) = account_id {
                    req = req.header("chatgpt-account-id", aid);
                }
                let resp = req
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow!("HTTP request failed: {e}"))?;

                let status = resp.status();
                if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < max_retries {
                    let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                    continue;
                }

                // 401: token may have expired mid-request, force refresh and retry
                if status.as_u16() == 401 && attempt < max_retries {
                    let _ = crate::auth::force_refresh().await;
                    continue;
                }

                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("OpenAI API error {status}: {text}"));
                }

                // Parse SSE stream - collect output items from response.output_item.done events
                let text = resp
                    .text()
                    .await
                    .map_err(|e| anyhow!("Failed to read response: {e}"))?;
                let mut output_items: Vec<Value> = Vec::new();
                for line in text.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            break;
                        }
                        if let Ok(event) = serde_json::from_str::<Value>(data) {
                            let event_type =
                                event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if event_type == "response.output_item.done" {
                                if let Some(item) = event.get("item") {
                                    output_items.push(item.clone());
                                }
                            }
                        }
                    }
                }
                if output_items.is_empty() {
                    return Err(anyhow!(
                        "No output items in SSE stream. Raw: {}",
                        &text[..text.len().min(500)]
                    ));
                }
                let response = json!({"output": output_items});
                return parse_openai_response(&response);
            }
            Err(anyhow!("OpenAI API: max retries exceeded"))
        })
    }
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut payload = parts[1].to_string();
    while !payload.len().is_multiple_of(4) {
        payload.push('=');
    }
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(&payload)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD
                .decode(&payload)
                .ok()
        })?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(|s| s.to_string())
}

fn parse_openai_response(response: &Value) -> Result<Vec<LlmEvent>> {
    let mut events = Vec::new();

    // Handle Responses API format (output array)
    if let Some(output) = response.get("output").and_then(|o| o.as_array()) {
        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    events.push(LlmEvent::Text(text.to_string()));
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_str = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                    events.push(LlmEvent::ToolUse { id, name, input });
                }
                _ => {}
            }
        }
        events.push(LlmEvent::Stop);
        return Ok(events);
    }

    // Fallback: Chat Completions format
    let choice = response
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("No choices in response"))?;

    let message = choice.get("message").ok_or_else(|| anyhow!("No message"))?;

    // Text content
    if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            events.push(LlmEvent::Text(content.to_string()));
        }
    }

    // Tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args_str = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            events.push(LlmEvent::ToolUse { id, name, input });
        }
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    if finish_reason != "tool_calls" {
        events.push(LlmEvent::Stop);
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_response() {
        let resp = json!({
            "content": [{"type": "text", "text": "Hello world"}],
            "stop_reason": "end_turn"
        });
        let events = parse_anthropic_response(&resp).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            LlmEvent::Text(t) => assert_eq!(t, "Hello world"),
            _ => panic!("expected Text event"),
        }
        assert!(matches!(events[1], LlmEvent::Stop));
    }

    #[test]
    fn test_parse_tool_use_response() {
        let resp = json!({
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "read", "input": {"path": "/tmp/x"}}
            ],
            "stop_reason": "tool_use"
        });
        let events = parse_anthropic_response(&resp).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "/tmp/x");
            }
            _ => panic!("expected ToolUse event"),
        }
    }

    fn test_provider(auth: AnthropicAuth) -> AnthropicProvider {
        AnthropicProvider {
            auth,
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn test_build_request_body() {
        let provider = test_provider(AnthropicAuth::ApiKey("test".to_string()));
        let messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }];
        let body = provider.build_request_body("system prompt", &messages, &[]);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 4096);
        // API-key mode keeps the plain-string system prompt.
        assert_eq!(body["system"], "system prompt");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_build_request_body_oauth_injects_claude_code_identity_and_caps_tools() {
        let provider = test_provider(AnthropicAuth::OAuth);
        let messages = vec![Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "read".to_string(),
                input: json!({"path": "/tmp/x"}),
            }],
        }];
        let tools = vec![ToolDef {
            name: "bash".to_string(),
            description: "run".to_string(),
            input_schema: json!({}),
        }];
        let body = provider.build_request_body("real prompt", &messages, &tools);
        // system[0] must be the Claude Code identity, real prompt appended.
        assert_eq!(
            body["system"][0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(body["system"][1]["text"], "real prompt");
        // tool def + assistant tool_use names normalised to CC casing.
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert_eq!(body["messages"][0]["content"][0]["name"], "Read");
    }

    #[test]
    fn test_claude_code_name_round_trip_and_passthrough() {
        assert_eq!(to_claude_code_name("read"), "Read");
        assert_eq!(from_claude_code_name("Read"), "read");
        // unknown (e.g. MCP) names pass through unchanged both ways.
        assert_eq!(to_claude_code_name("linear_search"), "linear_search");
        assert_eq!(from_claude_code_name("linear_search"), "linear_search");
    }

    #[test]
    fn test_parse_openai_text_response() {
        let resp = json!({
            "choices": [{"message": {"content": "Hello"}, "finish_reason": "stop"}]
        });
        let events = parse_openai_response(&resp).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], LlmEvent::Text(t) if t == "Hello"));
        assert!(matches!(events[1], LlmEvent::Stop));
    }

    #[test]
    fn test_parse_openai_tool_call_response() {
        let resp = json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"x.txt\"}"}}]
            }, "finish_reason": "tool_calls"}]
        });
        let events = parse_openai_response(&resp).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "x.txt");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_parse_openai_empty_choices() {
        let resp = json!({"choices": []});
        assert!(parse_openai_response(&resp).is_err());
    }
}
