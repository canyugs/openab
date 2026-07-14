//! Restricted LLM interpretation for one finalized Discord voice turn.
//!
//! The interpreter returns a structured semantic decision. It is deliberately
//! not given Discord, ACP, shell, or filesystem tools; the voice broker remains
//! the only component allowed to mutate intent state or dispatch confirmed work.

use crate::config::VoiceIntentInterpreterConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 1_048_576;
const MAX_SPEECH_CHARS: usize = 500;
const MAX_TASK_CHARS: usize = 1_500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum VoiceTurnPhase {
    Idle,
    WaitingConfirmation { destination: String, task: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceTurnInput {
    pub transcript: String,
    pub phase: VoiceTurnPhase,
    pub available_destinations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceTurnDecision {
    Ignore,
    Reply { speech: String },
    Propose { destination: String, task: String },
    Revise { destination: String, task: String },
    Accept,
    Reject,
    Cancel,
}

#[async_trait]
pub trait VoiceTurnInterpreter: Send + Sync {
    async fn interpret(&self, input: &VoiceTurnInput) -> Result<VoiceTurnDecision>;
}

pub struct OpenAiVoiceTurnInterpreter {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiVoiceTurnInterpreter {
    pub fn new(config: &VoiceIntentInterpreterConfig) -> Result<Self> {
        config_for_runtime(config)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_seconds))
            .build()
            .context("failed to build voice turn interpreter HTTP client")?;
        Ok(Self {
            client,
            api_key: config.api_key.trim().to_string(),
            model: config.model.trim().to_string(),
            base_url: config.base_url.trim().trim_end_matches('/').to_string(),
        })
    }

    fn request_body(&self, input: &VoiceTurnInput) -> Result<Value> {
        let input = serde_json::to_string(input).context("failed to serialize voice turn input")?;
        Ok(json!({
            "model": self.model,
            "store": false,
            "max_output_tokens": 256,
            "instructions": INTERPRETER_INSTRUCTIONS,
            "input": input,
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "voice_turn_decision",
                    "strict": true,
                    "schema": decision_schema()
                }
            }
        }))
    }
}

#[async_trait]
impl VoiceTurnInterpreter for OpenAiVoiceTurnInterpreter {
    async fn interpret(&self, input: &VoiceTurnInput) -> Result<VoiceTurnDecision> {
        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&self.request_body(input)?)
            .send()
            .await
            .context("voice turn interpreter request failed")?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("failed to read voice turn interpreter response")?;
        if body.len() > MAX_RESPONSE_BYTES {
            bail!("voice turn interpreter response exceeded size limit");
        }
        if !status.is_success() {
            let message = String::from_utf8_lossy(&body);
            bail!("voice turn interpreter returned {status}: {message}");
        }
        let response: Value = serde_json::from_slice(&body)
            .context("voice turn interpreter returned invalid JSON")?;
        let output = extract_output_text(&response)
            .context("voice turn interpreter response did not contain output_text")?;
        let raw: RawVoiceTurnDecision = serde_json::from_str(output)
            .context("voice turn interpreter output did not match the decision schema")?;
        raw.validate()
    }
}

#[derive(Debug, Deserialize)]
struct RawVoiceTurnDecision {
    action: RawVoiceTurnAction,
    destination: String,
    task: String,
    speech: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawVoiceTurnAction {
    Ignore,
    Reply,
    Propose,
    Revise,
    Accept,
    Reject,
    Cancel,
}

impl RawVoiceTurnDecision {
    fn validate(self) -> Result<VoiceTurnDecision> {
        let destination = self.destination.trim().to_string();
        let task = self.task.trim().to_string();
        let speech = self.speech.trim().to_string();
        match self.action {
            RawVoiceTurnAction::Ignore => Ok(VoiceTurnDecision::Ignore),
            RawVoiceTurnAction::Reply => {
                ensure_bounded_nonempty("speech", &speech, MAX_SPEECH_CHARS)?;
                Ok(VoiceTurnDecision::Reply { speech })
            }
            RawVoiceTurnAction::Propose => {
                ensure_bounded_nonempty("destination", &destination, 100)?;
                ensure_bounded_nonempty("task", &task, MAX_TASK_CHARS)?;
                Ok(VoiceTurnDecision::Propose { destination, task })
            }
            RawVoiceTurnAction::Revise => {
                ensure_bounded_nonempty("destination", &destination, 100)?;
                ensure_bounded_nonempty("task", &task, MAX_TASK_CHARS)?;
                Ok(VoiceTurnDecision::Revise { destination, task })
            }
            RawVoiceTurnAction::Accept => Ok(VoiceTurnDecision::Accept),
            RawVoiceTurnAction::Reject => Ok(VoiceTurnDecision::Reject),
            RawVoiceTurnAction::Cancel => Ok(VoiceTurnDecision::Cancel),
        }
    }
}

fn ensure_bounded_nonempty(name: &str, value: &str, max_chars: usize) -> Result<()> {
    if value.is_empty() {
        bail!("voice turn interpreter returned an empty {name}");
    }
    if value.chars().count() > max_chars {
        bail!("voice turn interpreter returned an oversized {name}");
    }
    Ok(())
}

fn extract_output_text(response: &Value) -> Option<&str> {
    response.get("output")?.as_array()?.iter().find_map(|item| {
        item.get("content")?
            .as_array()?
            .iter()
            .find(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
    })
}

fn decision_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["ignore", "reply", "propose", "revise", "accept", "reject", "cancel"]
            },
            "destination": { "type": "string" },
            "task": { "type": "string" },
            "speech": { "type": "string" }
        },
        "required": ["action", "destination", "task", "speech"],
        "additionalProperties": false
    })
}

fn config_for_runtime(config: &VoiceIntentInterpreterConfig) -> Result<()> {
    if !config.enabled {
        bail!("voice turn interpreter is disabled");
    }
    if config.api_key.trim().is_empty() {
        bail!("voice turn interpreter API key is empty");
    }
    if config.model.trim().is_empty() {
        bail!("voice turn interpreter model is empty");
    }
    if config.base_url.trim().is_empty() {
        bail!("voice turn interpreter base URL is empty");
    }
    if config.request_timeout_seconds == 0 {
        bail!("voice turn interpreter timeout must be greater than zero");
    }
    Ok(())
}

const INTERPRETER_INSTRUCTIONS: &str = r#"
You are the dialogue policy for a hands-free coding assistant. The input is JSON
containing one finalized STT transcript, the current voice phase, and the exact
destinations that are available. Return only the schema-constrained decision.

You have no execution authority. Never claim that work has started or completed.
- ignore: filler, accidental speech, or speech that does not call for a response.
- reply: a short social/conversational response that needs no tools or research.
- propose: an actionable request or substantive question while idle. Preserve the
  user's intended task; destination must be "local" or one listed destination.
- accept/reject/cancel: resolve a pending confirmation only when that meaning is clear.
- revise: replace a pending task with the user's corrected meaning. Preserve the
  pending destination unless the user explicitly changes it.

Prefer ignore over replying to filler. Prefer propose over pretending to answer a
question that needs knowledge or tools. Keep speech natural, useful, and under two
short sentences. For fields unused by an action, return an empty string.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_each_bounded_decision_shape() {
        let reply = RawVoiceTurnDecision {
            action: RawVoiceTurnAction::Reply,
            destination: String::new(),
            task: String::new(),
            speech: "好的。".into(),
        }
        .validate()
        .unwrap();
        assert_eq!(
            reply,
            VoiceTurnDecision::Reply {
                speech: "好的。".into()
            }
        );

        let proposal = RawVoiceTurnDecision {
            action: RawVoiceTurnAction::Propose,
            destination: "local".into(),
            task: "查看 issue 1368".into(),
            speech: String::new(),
        }
        .validate()
        .unwrap();
        assert_eq!(
            proposal,
            VoiceTurnDecision::Propose {
                destination: "local".into(),
                task: "查看 issue 1368".into()
            }
        );
    }

    #[test]
    fn rejects_missing_or_oversized_action_content() {
        let missing = RawVoiceTurnDecision {
            action: RawVoiceTurnAction::Reply,
            destination: String::new(),
            task: String::new(),
            speech: String::new(),
        };
        assert!(missing.validate().is_err());

        let oversized = RawVoiceTurnDecision {
            action: RawVoiceTurnAction::Propose,
            destination: "local".into(),
            task: "x".repeat(MAX_TASK_CHARS + 1),
            speech: String::new(),
        };
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn extracts_structured_output_text() {
        let response = json!({
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": "{\"action\":\"ignore\",\"destination\":\"\",\"task\":\"\",\"speech\":\"\"}"
                }]
            }]
        });
        assert!(extract_output_text(&response)
            .expect("output text")
            .contains("ignore"));
    }

    #[test]
    fn request_uses_strict_schema_without_tools() {
        let interpreter = OpenAiVoiceTurnInterpreter::new(&VoiceIntentInterpreterConfig {
            enabled: true,
            api_key: "test".into(),
            ..VoiceIntentInterpreterConfig::default()
        })
        .unwrap();
        let body = interpreter
            .request_body(&VoiceTurnInput {
                transcript: "嗯".into(),
                phase: VoiceTurnPhase::Idle,
                available_destinations: vec!["local".into()],
            })
            .unwrap();
        assert_eq!(body.pointer("/text/format/strict"), Some(&json!(true)));
        assert!(body.get("tools").is_none());
        assert_eq!(body.get("store"), Some(&json!(false)));
    }
}
