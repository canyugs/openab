use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Controls whether the bot processes messages from other Discord bots.
///
/// Inspired by Hermes Agent's `DISCORD_ALLOW_BOTS` 3-value design:
/// - `Off` (default): ignore all bot messages (safe default, no behavior change)
/// - `Mentions`: only process bot messages that @mention this bot (natural loop breaker)
/// - `All`: process all bot messages (capped at `MAX_CONSECUTIVE_BOT_TURNS`)
///
/// The bot's own messages are always ignored regardless of this setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AllowBots {
    #[default]
    Off,
    Mentions,
    All,
}

impl<'de> Deserialize<'de> for AllowBots {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "off" | "none" | "false" => Ok(Self::Off),
            "mentions" => Ok(Self::Mentions),
            "all" | "true" => Ok(Self::All),
            other => Err(serde::de::Error::unknown_variant(other, &["off", "mentions", "all"])),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub discord: Option<DiscordConfig>,
    pub slack: Option<SlackConfig>,
    pub agent: AgentConfig,
    #[serde(default)]
    pub pool: PoolConfig,
    #[serde(default)]
    pub reactions: ReactionsConfig,
    #[serde(default)]
    pub stt: SttConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SttConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
    #[serde(default = "default_stt_base_url")]
    pub base_url: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            model: default_stt_model(),
            base_url: default_stt_base_url(),
        }
    }
}

fn default_stt_model() -> String { "whisper-large-v3-turbo".into() }
fn default_stt_base_url() -> String { "https://api.groq.com/openai/v1".into() }

#[derive(Debug, Deserialize)]
pub struct DiscordConfig {
    pub bot_token: String,
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allow_bot_messages: AllowBots,
    /// When non-empty, only bot messages from these IDs pass the bot gate.
    /// Combines with `allow_bot_messages`: the mode check runs first, then
    /// the allowlist filters further. Empty = allow any bot (mode permitting).
    /// Only relevant when `allow_bot_messages` is `"mentions"` or `"all"`;
    /// ignored when `"off"` since all bot messages are rejected before this check.
    #[serde(default)]
    pub trusted_bot_ids: Vec<String>,
    #[serde(default)]
    pub allow_user_messages: AllowUsers,
}

/// Controls whether the bot responds to user messages in threads without @mention.
///
/// - `Involved` (default): respond to thread messages only if the bot has participated
///   in the thread (posted at least one message, or the thread parent @mentions the bot).
///   Channel/MPDM messages always require @mention. DMs always process (implicit mention).
/// - `Mentions`: always require @mention, even in threads the bot is participating in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AllowUsers {
    #[default]
    Involved,
    Mentions,
}

impl<'de> Deserialize<'de> for AllowUsers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "involved" => Ok(Self::Involved),
            "mentions" => Ok(Self::Mentions),
            other => Err(serde::de::Error::unknown_variant(other, &["involved", "mentions"])),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allow_bot_messages: AllowBots,
    /// Bot User IDs (U...) allowed to interact when allow_bot_messages is
    /// "mentions" or "all". Find via Slack UI: click bot profile → Copy member ID.
    /// Empty = allow any bot (mode permitting).
    #[serde(default)]
    pub trusted_bot_ids: Vec<String>,
    #[serde(default)]
    pub allow_user_messages: AllowUsers,
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct PoolConfig {
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_ttl_hours")]
    pub session_ttl_hours: u64,
}

#[derive(Debug, Deserialize)]
pub struct ReactionsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub remove_after_reply: bool,
    #[serde(default)]
    pub emojis: ReactionEmojis,
    #[serde(default)]
    pub timing: ReactionTiming,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionEmojis {
    #[serde(default = "emoji_queued")]
    pub queued: String,
    #[serde(default = "emoji_thinking")]
    pub thinking: String,
    #[serde(default = "emoji_tool")]
    pub tool: String,
    #[serde(default = "emoji_coding")]
    pub coding: String,
    #[serde(default = "emoji_web")]
    pub web: String,
    #[serde(default = "emoji_done")]
    pub done: String,
    #[serde(default = "emoji_error")]
    pub error: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionTiming {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_stall_soft_ms")]
    pub stall_soft_ms: u64,
    #[serde(default = "default_stall_hard_ms")]
    pub stall_hard_ms: u64,
    #[serde(default = "default_done_hold_ms")]
    pub done_hold_ms: u64,
    #[serde(default = "default_error_hold_ms")]
    pub error_hold_ms: u64,
}

// --- defaults ---

fn default_working_dir() -> String { "/tmp".into() }
fn default_max_sessions() -> usize { 10 }
fn default_ttl_hours() -> u64 { 4 }
fn default_true() -> bool { true }

fn emoji_queued() -> String { "👀".into() }
fn emoji_thinking() -> String { "🤔".into() }
fn emoji_tool() -> String { "🔥".into() }
fn emoji_coding() -> String { "👨‍💻".into() }
fn emoji_web() -> String { "⚡".into() }
fn emoji_done() -> String { "🆗".into() }
fn emoji_error() -> String { "😱".into() }

fn default_debounce_ms() -> u64 { 700 }
fn default_stall_soft_ms() -> u64 { 10_000 }
fn default_stall_hard_ms() -> u64 { 30_000 }
fn default_done_hold_ms() -> u64 { 1_500 }
fn default_error_hold_ms() -> u64 { 2_500 }

impl Default for PoolConfig {
    fn default() -> Self {
        Self { max_sessions: default_max_sessions(), session_ttl_hours: default_ttl_hours() }
    }
}

impl Default for ReactionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            remove_after_reply: false,
            emojis: ReactionEmojis::default(),
            timing: ReactionTiming::default(),
        }
    }
}

impl Default for ReactionEmojis {
    fn default() -> Self {
        Self {
            queued: emoji_queued(), thinking: emoji_thinking(), tool: emoji_tool(),
            coding: emoji_coding(), web: emoji_web(), done: emoji_done(), error: emoji_error(),
        }
    }
}

impl Default for ReactionTiming {
    fn default() -> Self {
        Self {
            debounce_ms: default_debounce_ms(), stall_soft_ms: default_stall_soft_ms(),
            stall_hard_ms: default_stall_hard_ms(), done_hold_ms: default_done_hold_ms(),
            error_hold_ms: default_error_hold_ms(),
        }
    }
}

// --- loading ---

fn expand_env_vars(raw: &str) -> String {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();
    re.replace_all(raw, |caps: &regex::Captures| {
        std::env::var(&caps[1]).unwrap_or_default()
    })
    .into_owned()
}

pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    let expanded = expand_env_vars(&raw);
    let config: Config = toml::from_str(&expanded)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
    Ok(config)
}

/// Parse an integer env var, falling back to `default` when unset/empty,
/// but returning an error for malformed values so typos surface immediately.
fn parse_int_env<T>(key: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(s) if !s.trim().is_empty() => s
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{key} must be a valid integer: {e}")),
        _ => Ok(default),
    }
}

/// Build a Config entirely from `OPENAB_*` environment variables.
///
/// Naming: `OPENAB_<SECTION>_<FIELD>` — single underscore throughout.
///
/// Examples:
///   OPENAB_DISCORD_BOT_TOKEN        → [discord] bot_token
///   OPENAB_DISCORD_ALLOWED_CHANNELS → [discord] allowed_channels (comma-separated)
///   OPENAB_SLACK_BOT_TOKEN          → [slack] bot_token
///   OPENAB_SLACK_APP_TOKEN          → [slack] app_token
///   OPENAB_AGENT_COMMAND            → [agent] command
///   OPENAB_AGENT_ARGS               → [agent] args (comma-separated)
///   OPENAB_AGENT_WORKING_DIR        → [agent] working_dir
///   OPENAB_AGENT_ENV_<KEY>          → [agent] env.<KEY> (passthrough to child process)
///   OPENAB_POOL_MAX_SESSIONS        → [pool] max_sessions
///   OPENAB_POOL_SESSION_TTL_HOURS   → [pool] session_ttl_hours
///   OPENAB_STT_ENABLED              → [stt] enabled (true/false)
///   OPENAB_STT_API_KEY              → [stt] api_key (or auto-detect GROQ_API_KEY)
///   OPENAB_STT_MODEL                → [stt] model
///   OPENAB_STT_BASE_URL             → [stt] base_url
///   OPENAB_REACTIONS_ENABLED        → [reactions] enabled (true/false)
///
/// Also supported (not listed above for brevity):
///   OPENAB_DISCORD_ALLOW_BOT_MESSAGES  → off/mentions/all
///   OPENAB_DISCORD_TRUSTED_BOT_IDS     → CSV
///   OPENAB_DISCORD_ALLOW_USER_MESSAGES → involved/mentions
///   OPENAB_SLACK_ALLOWED_CHANNELS      → CSV
///   OPENAB_SLACK_ALLOWED_USERS         → CSV
///   OPENAB_SLACK_ALLOW_BOT_MESSAGES    → off/mentions/all
///   OPENAB_SLACK_TRUSTED_BOT_IDS       → CSV
///   OPENAB_SLACK_ALLOW_USER_MESSAGES   → involved/mentions
///
/// Not available via env (use config.toml for these):
///   [reactions] remove_after_reply, emojis.*, timing.*
pub fn load_config_from_env() -> anyhow::Result<Config> {
    use std::env;

    let env_or = |key: &str, default: &str| -> String {
        env::var(key).unwrap_or_else(|_| default.to_string())
    };

    let env_opt = |key: &str| -> Option<String> {
        env::var(key).ok().filter(|s| !s.is_empty())
    };

    let csv_to_vec = |key: &str| -> Vec<String> {
        env::var(key)
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let parse_allow_bots = |key: &str| -> AllowBots {
        match env::var(key).unwrap_or_default().to_lowercase().as_str() {
            "mentions" => AllowBots::Mentions,
            "all" | "true" => AllowBots::All,
            _ => AllowBots::Off,
        }
    };

    let parse_allow_users = |key: &str| -> AllowUsers {
        match env::var(key).unwrap_or_default().to_lowercase().as_str() {
            "mentions" => AllowUsers::Mentions,
            _ => AllowUsers::Involved,
        }
    };

    // [discord] — only if bot_token is set
    let discord = env_opt("OPENAB_DISCORD_BOT_TOKEN").map(|bot_token| DiscordConfig {
        bot_token,
        allowed_channels: csv_to_vec("OPENAB_DISCORD_ALLOWED_CHANNELS"),
        allowed_users: csv_to_vec("OPENAB_DISCORD_ALLOWED_USERS"),
        allow_bot_messages: parse_allow_bots("OPENAB_DISCORD_ALLOW_BOT_MESSAGES"),
        trusted_bot_ids: csv_to_vec("OPENAB_DISCORD_TRUSTED_BOT_IDS"),
        allow_user_messages: parse_allow_users("OPENAB_DISCORD_ALLOW_USER_MESSAGES"),
    });

    // [slack] — only if both bot_token and app_token are set
    let slack = env_opt("OPENAB_SLACK_BOT_TOKEN").and_then(|bot_token| {
        env_opt("OPENAB_SLACK_APP_TOKEN").map(|app_token| SlackConfig {
            bot_token,
            app_token,
            allowed_channels: csv_to_vec("OPENAB_SLACK_ALLOWED_CHANNELS"),
            allowed_users: csv_to_vec("OPENAB_SLACK_ALLOWED_USERS"),
            allow_bot_messages: parse_allow_bots("OPENAB_SLACK_ALLOW_BOT_MESSAGES"),
            trusted_bot_ids: csv_to_vec("OPENAB_SLACK_TRUSTED_BOT_IDS"),
            allow_user_messages: parse_allow_users("OPENAB_SLACK_ALLOW_USER_MESSAGES"),
        })
    });

    // [agent] — command is required
    let command = env_or("OPENAB_AGENT_COMMAND", "");
    if command.is_empty() {
        anyhow::bail!("OPENAB_AGENT_COMMAND is required when using env-only config");
    }

    let agent = AgentConfig {
        command,
        args: csv_to_vec("OPENAB_AGENT_ARGS"),
        working_dir: env_or("OPENAB_AGENT_WORKING_DIR", &default_working_dir()),
        env: env::vars()
            .filter(|(k, _)| k.starts_with("OPENAB_AGENT_ENV_"))
            .map(|(k, v)| {
                let key = k.strip_prefix("OPENAB_AGENT_ENV_").unwrap().to_string();
                (key, v)
            })
            .collect(),
    };

    // [pool] — fail loudly on invalid integers so typos in env vars surface immediately
    let pool = PoolConfig {
        max_sessions: parse_int_env("OPENAB_POOL_MAX_SESSIONS", default_max_sessions())?,
        session_ttl_hours: parse_int_env("OPENAB_POOL_SESSION_TTL_HOURS", default_ttl_hours())?,
    };

    // [stt]
    let stt = SttConfig {
        enabled: env_or("OPENAB_STT_ENABLED", "false").parse().unwrap_or(false),
        api_key: env_or("OPENAB_STT_API_KEY", ""),
        model: env_or("OPENAB_STT_MODEL", &default_stt_model()),
        base_url: env_or("OPENAB_STT_BASE_URL", &default_stt_base_url()),
    };

    // [reactions] — only enabled/disabled via env; emojis/timing use config.toml
    let mut reactions = ReactionsConfig::default();
    if let Ok(v) = env::var("OPENAB_REACTIONS_ENABLED") {
        reactions.enabled = v == "true" || v == "1";
    }

    Ok(Config {
        discord,
        slack,
        agent,
        pool,
        reactions,
        stt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;

    // Env vars are process-global; serialize all env-config tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: set env vars, run closure, then clean up.
    fn with_env_vars<F: FnOnce()>(vars: &[(&str, &str)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        // Clean all OPENAB_ vars first to isolate tests
        for (k, _) in env::vars() {
            if k.starts_with("OPENAB_") {
                env::remove_var(&k);
            }
        }
        for (k, v) in vars {
            env::set_var(k, v);
        }
        f();
        for (k, _) in vars {
            env::remove_var(k);
        }
    }

    #[test]
    fn env_config_minimal_discord() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "claude-agent-acp"),
                ("OPENAB_DISCORD_BOT_TOKEN", "test-token"),
            ],
            || {
                let cfg = load_config_from_env().unwrap();
                assert_eq!(cfg.agent.command, "claude-agent-acp");
                assert!(cfg.discord.is_some());
                assert_eq!(cfg.discord.as_ref().unwrap().bot_token, "test-token");
                assert!(cfg.slack.is_none());
            },
        );
    }

    #[test]
    fn env_config_discord_and_slack() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "kiro-cli"),
                ("OPENAB_AGENT_ARGS", "acp,--trust-all-tools"),
                ("OPENAB_AGENT_WORKING_DIR", "/home/agent"),
                ("OPENAB_DISCORD_BOT_TOKEN", "discord-tok"),
                ("OPENAB_DISCORD_ALLOWED_CHANNELS", "111,222"),
                ("OPENAB_SLACK_BOT_TOKEN", "xoxb-slack"),
                ("OPENAB_SLACK_APP_TOKEN", "xapp-slack"),
                ("OPENAB_POOL_MAX_SESSIONS", "5"),
            ],
            || {
                let cfg = load_config_from_env().unwrap();
                assert_eq!(cfg.agent.command, "kiro-cli");
                assert_eq!(cfg.agent.args, vec!["acp", "--trust-all-tools"]);
                assert_eq!(cfg.agent.working_dir, "/home/agent");

                let d = cfg.discord.as_ref().unwrap();
                assert_eq!(d.bot_token, "discord-tok");
                assert_eq!(d.allowed_channels, vec!["111", "222"]);

                let s = cfg.slack.as_ref().unwrap();
                assert_eq!(s.bot_token, "xoxb-slack");
                assert_eq!(s.app_token, "xapp-slack");

                assert_eq!(cfg.pool.max_sessions, 5);
            },
        );
    }

    #[test]
    fn env_config_no_adapter_is_valid() {
        with_env_vars(
            &[("OPENAB_AGENT_COMMAND", "claude-agent-acp")],
            || {
                let cfg = load_config_from_env().unwrap();
                assert!(cfg.discord.is_none());
                assert!(cfg.slack.is_none());
            },
        );
    }

    #[test]
    fn env_config_missing_command_fails() {
        with_env_vars(&[], || {
            let result = load_config_from_env();
            assert!(result.is_err());
        });
    }

    #[test]
    fn env_config_invalid_pool_int_fails_loudly() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "claude"),
                ("OPENAB_POOL_MAX_SESSIONS", "not-a-number"),
            ],
            || {
                let err = load_config_from_env().unwrap_err();
                let msg = format!("{err}");
                assert!(
                    msg.contains("OPENAB_POOL_MAX_SESSIONS"),
                    "error should name the offending var, got: {msg}"
                );
            },
        );
    }

    #[test]
    fn env_config_empty_pool_int_uses_default() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "claude"),
                ("OPENAB_POOL_MAX_SESSIONS", ""),
            ],
            || {
                let cfg = load_config_from_env().unwrap();
                assert_eq!(cfg.pool.max_sessions, default_max_sessions());
            },
        );
    }

    #[test]
    fn env_config_agent_env_passthrough() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "claude"),
                ("OPENAB_AGENT_ENV_ANTHROPIC_API_KEY", "sk-test"),
                ("OPENAB_AGENT_ENV_CUSTOM_VAR", "hello"),
            ],
            || {
                let cfg = load_config_from_env().unwrap();
                assert_eq!(cfg.agent.env.get("ANTHROPIC_API_KEY").unwrap(), "sk-test");
                assert_eq!(cfg.agent.env.get("CUSTOM_VAR").unwrap(), "hello");
            },
        );
    }

    #[test]
    fn env_config_stt() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "claude"),
                ("OPENAB_STT_ENABLED", "true"),
                ("OPENAB_STT_API_KEY", "gsk-test"),
                ("OPENAB_STT_MODEL", "whisper-1"),
            ],
            || {
                let cfg = load_config_from_env().unwrap();
                assert!(cfg.stt.enabled);
                assert_eq!(cfg.stt.api_key, "gsk-test");
                assert_eq!(cfg.stt.model, "whisper-1");
                assert_eq!(cfg.stt.base_url, default_stt_base_url());
            },
        );
    }

    #[test]
    fn env_config_reactions_disabled() {
        with_env_vars(
            &[
                ("OPENAB_AGENT_COMMAND", "claude"),
                ("OPENAB_REACTIONS_ENABLED", "false"),
            ],
            || {
                let cfg = load_config_from_env().unwrap();
                assert!(!cfg.reactions.enabled);
            },
        );
    }
}
