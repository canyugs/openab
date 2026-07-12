use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;
use tracing::{error, warn};

use crate::acp::{ContentBlock, SessionPool};
use crate::acp_turn::TurnCompletion;
pub use crate::acp_turn_driver::{
    parse_output_directives, select_delivery_text, split_delivery, OutputDirectives,
};
use crate::acp_turn_driver::{AcpTurnDriver, TurnDeliveryContext};
// Preserve the pre-extraction crate-local paths for feature-gated callers and
// adapter tests even though the default library build does not call them.
#[allow(unused_imports)]
pub(crate) use crate::acp_turn_driver::{classify_empty_turn, finalize_body, SILENT_FAILURE_MSG};
use crate::config::ReactionsConfig;
use crate::error_display::format_user_error;
use crate::markdown::TableMode;
use crate::reactions::StatusReactionController;

#[cfg(test)]
use crate::acp_turn_driver::{compose_display, contains_bot_mention, ToolEntry, ToolState};
#[cfg(test)]
use crate::config::ToolDisplay;

// --- Platform-agnostic types ---

/// Identifies a channel or thread across platforms.
///
/// Used for **routing**: `channel_id` is the ID the adapter sends messages to.
/// For Discord threads, this is the thread's own channel ID (Discord API
/// requires it for `say`/`edit`). Use `parent_id` to find the parent channel.
///
/// Compare with `SenderContext`, which is **metadata for the agent**: there
/// `channel_id` is the parent channel and `thread_id` is the thread,
/// matching Slack's model for cross-platform consistency.
#[derive(Clone, Debug)]
pub struct ChannelRef {
    pub platform: String,
    pub channel_id: String,
    /// Thread within a channel (e.g. Slack thread_ts, Telegram topic_id).
    /// For Discord, threads are separate channels so this is None.
    pub thread_id: Option<String>,
    /// Parent channel if this is a thread-as-channel (Discord).
    pub parent_id: Option<String>,
    /// Originating gateway event ID, propagated back in `GatewayReply.reply_to`
    /// so the gateway can correlate replies with inbound events (e.g. LINE reply tokens).
    /// Excluded from Hash/Eq — two ChannelRefs pointing to the same channel are
    /// equal regardless of which event they originated from.
    pub origin_event_id: Option<String>,
}

impl PartialEq for ChannelRef {
    fn eq(&self, other: &Self) -> bool {
        self.platform == other.platform
            && self.channel_id == other.channel_id
            && self.thread_id == other.thread_id
            && self.parent_id == other.parent_id
    }
}

impl Eq for ChannelRef {}

impl std::hash::Hash for ChannelRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.platform.hash(state);
        self.channel_id.hash(state);
        self.thread_id.hash(state);
        self.parent_id.hash(state);
    }
}

/// Identifies a message across platforms.
#[derive(Clone, Debug)]
pub struct MessageRef {
    pub channel: ChannelRef,
    pub message_id: String,
}

/// Bundles per-message parameters for `AdapterRouter::handle_message`.
///
/// Introduced to reduce parameter count and make the signature extensible
/// (e.g. streaming policy, rate limit hints) without breaking call sites.
pub struct MessageContext {
    pub thread_channel: ChannelRef,
    pub sender_json: String,
    pub prompt: String,
    pub extra_blocks: Vec<ContentBlock>,
    pub trigger_msg: MessageRef,
    pub other_bot_present: bool,
}

/// Sender identity injected into prompts for downstream agent context.
///
/// This is **metadata for the agent** — `channel_id` always refers to the
/// logical parent channel, and `thread_id` identifies the thread (if any).
/// This convention is consistent across platforms (Slack, Discord, Telegram).
///
/// Compare with `ChannelRef`, which is used for **routing**: there
/// `channel_id` is the ID the adapter sends messages to (for Discord
/// threads, that's the thread's own channel ID, not the parent).
#[derive(Clone, Debug, Serialize)]
pub struct SenderContext {
    pub schema: String,
    pub sender_id: String,
    pub sender_name: String,
    pub display_name: String,
    pub channel: String,
    pub channel_id: String,
    /// Thread identifier, if the message is inside a thread.
    /// Slack: thread_ts. Discord: thread channel ID (channel_id holds the parent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub is_bot: bool,
    /// Platform message creation time (ISO 8601 UTC), if available.
    /// Discord/Slack: platform timestamp. Gateway: broker receive time (best-effort).
    /// Additive optional field — schema version stays openab.sender.v1 (no consumer
    /// breakage). If future additions require breaking changes, bump to v1.1+.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Platform message ID. Agents can use this to reply to a specific message
    /// via the `[[reply_to:<message_id>]]` output directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The platform user ID of the receiving bot/agent.
    /// Enables agents to identify themselves when multiple agents share the same backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_id: Option<String>,
}

// --- ChatAdapter trait ---

#[async_trait]
pub trait ChatAdapter: Send + Sync + 'static {
    /// Platform name for logging and session key namespacing.
    fn platform(&self) -> &'static str;

    /// Maximum message length (chars) for this platform; the router splits longer
    /// replies into multiple messages at this bound. Platform-specific (e.g. 2000
    /// for Discord; Slack uses its Block Kit `markdown` block cap).
    fn message_limit(&self) -> usize;

    /// Send a new message, returns a reference to the sent message.
    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef>;

    /// Create a thread from a trigger message, returns the thread channel ref.
    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> Result<ChannelRef>;

    /// Add a reaction/emoji to a message.
    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;

    /// Remove a reaction/emoji from a message.
    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;

    /// Edit an existing message in-place (for streaming updates).
    /// Default: unsupported (send-once only).
    async fn edit_message(&self, _msg: &MessageRef, _content: &str) -> Result<()> {
        Err(anyhow::anyhow!("edit_message not supported"))
    }

    /// Send a message as a reply to a specific message (Discord: message_reference).
    /// Default: falls back to plain send_message (ignores reply_to).
    async fn send_message_with_reply(
        &self,
        channel: &ChannelRef,
        content: &str,
        reply_to_message_id: &str,
    ) -> Result<MessageRef> {
        let _ = reply_to_message_id; // unused in default impl
        self.send_message(channel, content).await
    }

    /// Rename the thread/channel title. Default: no-op (not all platforms support it).
    async fn rename_thread(&self, _channel: &ChannelRef, _title: &str) -> Result<()> {
        Ok(())
    }

    /// Delete a message. Used to remove streaming placeholders when reply_to is set.
    /// Default: edits to zero-width space (fallback for platforms without delete support).
    async fn delete_message(&self, msg: &MessageRef) -> Result<()> {
        self.edit_message(msg, "\u{200b}").await
    }

    /// Whether this adapter streams via a native streaming API (Slack
    /// chat.startStream) rather than the post+edit loop. Default: false.
    /// `other_bot_present` lets adapters fall back to send-once in multi-bot
    /// threads (mirrors `use_streaming`'s #534 rule).
    fn uses_native_streaming(&self, _other_bot_present: bool) -> bool {
        false
    }

    /// Begin a native stream. The returned MessageRef is the handle for
    /// subsequent `stream_append` / `stream_finish`.
    /// Default: delegate to send_message (only called when uses_native_streaming).
    /// `recipient` is the per-turn `(user_id, team_id)` for platforms (Slack) that
    /// need it for the native stream open; ignored by the default impl.
    async fn stream_begin(
        &self,
        channel: &ChannelRef,
        _recipient: Option<(String, String)>,
    ) -> Result<MessageRef> {
        self.send_message(channel, "…").await
    }

    /// Append an INCREMENTAL delta to a native stream.
    /// Default: best-effort edit (only called when uses_native_streaming).
    async fn stream_append(&self, msg: &MessageRef, delta: &str) -> Result<()> {
        self.edit_message(msg, delta).await
    }

    /// Finish a native stream and write the COMPLETE final content.
    /// Default: delegate to edit_message.
    async fn stream_finish(&self, msg: &MessageRef, final_content: &str) -> Result<()> {
        self.edit_message(msg, final_content).await
    }

    /// Whether this adapter uses a status API (e.g. assistant.threads.setStatus)
    /// instead of emoji reactions for thinking/tool indicators. Independent of
    /// `uses_native_streaming` — status can work without content streaming.
    /// Default: false.
    fn uses_assistant_status(&self) -> bool {
        false
    }

    /// Set an ephemeral status line (e.g. "Thinking…", "Using <tool>…").
    /// Empty string clears it. Default: no-op (platforms without a status API).
    async fn set_status(&self, _channel: &ChannelRef, _status: &str) -> Result<()> {
        Ok(())
    }

    /// Whether this platform renders Markdown tables natively. When `true`, the
    /// router skips the `convert_tables` pre-pass (which rewrites tables into
    /// code blocks / bullet lists for platforms that cannot render them) and
    /// lets the platform render the raw Markdown table itself.
    /// Default: `false` (keep converting). Overridden by Slack (Block Kit
    /// `markdown` blocks / `markdown_text` stream chunks render tables natively).
    /// The `platform` parameter allows shared adapters (e.g. UnifiedGatewayAdapter)
    /// to make per-platform decisions.
    fn renders_native_tables(&self, platform: &str) -> bool {
        let _ = platform;
        false
    }

    /// Whether this adapter should use streaming edit (true) or send-once (false).
    /// `other_bot_present` indicates if another bot has posted in the current thread.
    /// Streaming should be disabled in multi-bot threads to avoid edit interference.
    /// NOTE: Slight race window exists — the multibot cache is checked before
    /// handle_message, so a bot arriving between the check and the response will
    /// not be detected until the next message. This is acceptable: the first
    /// response may stream, but subsequent ones will correctly use send-once.
    fn use_streaming(&self, other_bot_present: bool) -> bool;

    /// Whether to send the "…" placeholder message before streaming starts.
    /// Default: true. Platforms using drafts (e.g. Telegram Rich Messages) can
    /// return false to suppress the placeholder.
    fn show_streaming_placeholder(&self) -> bool {
        true
    }
}

// --- AdapterRouter ---

/// Shared logic for routing messages to ACP agents, managing sessions,
/// streaming edits, and controlling reactions. Platform-independent.
pub struct AdapterRouter {
    pool: Arc<SessionPool>,
    reactions_config: ReactionsConfig,
    table_mode: TableMode,
    prompt_hard_timeout: std::time::Duration,
    /// Polling cadence for the recv-loop liveness check (#732).
    liveness_check_interval: std::time::Duration,
    /// Workspace aliases from `[workspace.aliases]` config.
    workspace_aliases: std::collections::HashMap<String, String>,
    /// Bot home directory (security boundary for workspace directives).
    bot_home: std::path::PathBuf,
    /// Per-platform trust gate (L2 scope + L3 identity). Populated via
    /// [`AdapterRouter::with_trust`]; empty default = deny-all per platform
    /// (only consulted by paths wired to the gate — currently the gateway path).
    trust: crate::trust::PlatformTrustConfigs,
}

impl AdapterRouter {
    pub fn new(
        pool: Arc<SessionPool>,
        reactions_config: ReactionsConfig,
        table_mode: TableMode,
        prompt_hard_timeout_secs: u64,
        liveness_check_secs: u64,
        workspace_aliases: std::collections::HashMap<String, String>,
        bot_home: std::path::PathBuf,
    ) -> Self {
        if liveness_check_secs >= prompt_hard_timeout_secs {
            warn!(
                liveness_check_secs,
                prompt_hard_timeout_secs,
                "pool.liveness_check_secs >= pool.prompt_hard_timeout_secs; \
                 the hard ceiling will only fire after the next liveness tick \
                 and may be effectively bypassed. Lower liveness_check_secs."
            );
        }
        Self {
            pool,
            reactions_config,
            table_mode,
            prompt_hard_timeout: std::time::Duration::from_secs(prompt_hard_timeout_secs),
            liveness_check_interval: std::time::Duration::from_secs(liveness_check_secs),
            workspace_aliases,
            bot_home,
            trust: crate::trust::PlatformTrustConfigs::default(),
        }
    }

    /// Attach the per-platform trust registry (builder style, before `Arc`-wrapping).
    /// Keeps `new()`'s signature stable across its many call sites.
    pub fn with_trust(mut self, trust: crate::trust::PlatformTrustConfigs) -> Self {
        self.trust = trust;
        self
    }

    /// The single ingress trust gate: evaluate L2 (scope) + L3 (identity) for an
    /// inbound message. This is the long-term choke point — dispatch paths should
    /// only be reachable after an `Allow` here. Returns the [`Decision`] so the
    /// caller can echo on `DenyIdentity` (request-access UX) vs silently drop on
    /// `DenyScope`.
    pub fn gate_incoming(
        &self,
        platform: &str,
        channel_id: &str,
        is_dm: bool,
        sender_id: &str,
    ) -> crate::trust::Decision {
        self.trust.decide(platform, channel_id, is_dm, sender_id)
    }

    /// Access the underlying session pool (e.g. for config option queries).
    pub fn pool(&self) -> &Arc<SessionPool> {
        &self.pool
    }

    /// Access the reactions config (used by dispatch.rs).
    pub fn reactions_config(&self) -> &ReactionsConfig {
        &self.reactions_config
    }

    /// Workspace aliases for control directive resolution.
    pub fn workspace_aliases_map(&self) -> std::collections::HashMap<String, String> {
        self.workspace_aliases.clone()
    }

    /// Bot home path for workspace security boundary.
    pub fn bot_home_path(&self) -> std::path::PathBuf {
        self.bot_home.clone()
    }

    /// Pack one arrival event into ContentBlocks. Per-arrival layout:
    ///   Text { "<sender_context>\n{json}\n</sender_context>" }   <- delimiter
    ///   [Text blocks from extra_blocks (e.g. STT transcripts)]
    ///   Text { "{prompt}" }                                       <- omitted if empty
    ///   [non-Text blocks from extra_blocks (e.g. Image)]
    ///
    /// The sender_context block stands alone so it can serve as a structural
    /// delimiter between arrivals in batched dispatch — agents can scan for
    /// `<sender_context>` openers to find arrival boundaries. Within an arrival,
    /// transcript text precedes the typed prompt to match pre-batching adapter
    /// behavior (voice content first), and images trail the prompt as before.
    /// This is the single packing code path for both per-message and batched
    /// dispatch (ADR §3.5). For a batch of N messages, call this N times and
    /// concatenate.
    pub fn pack_arrival_event(
        sender_json: &str,
        prompt: &str,
        extra_blocks: Vec<ContentBlock>,
    ) -> Vec<ContentBlock> {
        let header = format!("<sender_context>\n{}\n</sender_context>", sender_json);
        let (texts, others): (Vec<_>, Vec<_>) = extra_blocks
            .into_iter()
            .partition(|b| matches!(b, ContentBlock::Text { .. }));
        let mut blocks = Vec::with_capacity(2 + texts.len() + others.len());
        blocks.push(ContentBlock::Text { text: header });
        blocks.extend(texts);
        if !prompt.is_empty() {
            blocks.push(ContentBlock::Text {
                text: prompt.to_string(),
            });
        }
        blocks.extend(others);
        blocks
    }

    /// Handle an incoming user message. The adapter is responsible for
    /// filtering, resolving the thread, and building the SenderContext.
    /// This method handles sender context injection, session management, and streaming.
    pub async fn handle_message(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        ctx: MessageContext,
    ) -> Result<()> {
        tracing::debug!(platform = adapter.platform(), "processing message");

        let content_blocks =
            Self::pack_arrival_event(&ctx.sender_json, &ctx.prompt, ctx.extra_blocks);

        let thread_key = format!(
            "{}:{}",
            adapter.platform(),
            ctx.thread_channel
                .thread_id
                .as_deref()
                .unwrap_or(&ctx.thread_channel.channel_id)
        );

        if let Err(e) = self.pool.get_or_create(&thread_key, None).await {
            let msg = format_user_error(&e.to_string());
            let _ = adapter
                .send_message(&ctx.thread_channel, &format!("⚠️ {msg}"))
                .await;
            error!("pool error: {e}");
            return Err(e);
        }

        // In assistant-status mode (e.g. Slack assistant_mode), status is conveyed
        // via assistant.threads.setStatus, so the emoji-reaction lifecycle is skipped
        // entirely — mirrors dispatch_batch so per-message and batched modes agree.
        let assistant_status = adapter.uses_assistant_status();

        let reactions = Arc::new(StatusReactionController::new(
            self.reactions_config.enabled,
            adapter.clone(),
            ctx.trigger_msg.clone(),
            self.reactions_config.emojis.clone(),
            self.reactions_config.timing.clone(),
        ));
        if !assistant_status {
            reactions.set_queued().await;
        }

        let result = self
            .stream_prompt(
                adapter,
                &thread_key,
                content_blocks,
                &ctx.thread_channel,
                reactions.clone(),
                ctx.other_bot_present,
            )
            .await;

        if !assistant_status {
            match &result {
                Ok(()) => reactions.set_done().await,
                Err(_) => reactions.set_error().await,
            }

            let hold_ms = if result.is_ok() {
                self.reactions_config.timing.done_hold_ms
            } else {
                self.reactions_config.timing.error_hold_ms
            };
            if self.reactions_config.remove_after_reply {
                let reactions = reactions;
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                    reactions.clear().await;
                });
            }
        }

        if let Err(ref e) = result {
            let _ = adapter
                .send_message(&ctx.thread_channel, &format!("⚠️ {e}"))
                .await;
        }

        result
    }

    async fn stream_prompt(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
    ) -> Result<()> {
        self.stream_prompt_blocks(
            adapter,
            thread_key,
            content_blocks,
            thread_channel,
            reactions,
            other_bot_present,
            // handle_message path (e.g. cron) is never Slack assistant-mode native
            // streaming, so no per-turn recipient — degrades to post+edit if it were.
            None,
        )
        .await
    }

    /// Drive one ACP turn with the given pre-packed ContentBlocks.
    ///
    /// This compatibility entrypoint preserves the public pre-Phase-0
    /// `Result<()>` contract. New execution paths that need authoritative
    /// output and independent execution/delivery state should call
    /// [`Self::stream_prompt_blocks_typed`].
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_prompt_blocks(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
        recipient: Option<(String, String)>,
    ) -> Result<()> {
        self.stream_prompt_blocks_typed(
            adapter,
            thread_key,
            content_blocks,
            thread_channel,
            reactions,
            other_bot_present,
            recipient,
        )
        .await?
        .legacy_dispatch
        .into_result()
    }

    /// Drive one ACP turn and return its typed authoritative completion.
    /// Called by both `handle_message` (per-message mode) and `dispatch::dispatch_batch`
    /// (batched mode).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_prompt_blocks_typed(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
        recipient: Option<(String, String)>,
    ) -> Result<TurnCompletion> {
        let context = TurnDeliveryContext {
            adapter: adapter.clone(),
            thread_channel: thread_channel.clone(),
            reactions,
            recipient,
            other_bot_present,
            table_mode: self.table_mode,
            tool_display: self.reactions_config.tool_display,
            narration_display: self.reactions_config.narration_display,
            prompt_hard_timeout: self.prompt_hard_timeout,
            liveness_check_interval: self.liveness_check_interval,
        };

        let connection = self.pool.connection_handle(thread_key).await?;
        AcpTurnDriver::spawn(connection, content_blocks, context)
            .wait()
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_delivery_text_send_once_keeps_only_final_block() {
        // Simulates: narration "n1" → tool (answer_start→2) → narration "n2"
        // → tool (answer_start→14) → final answer. In send-once mode only the
        // text after the last tool survives.
        let full = "n1[tool]n2[tool]the final answer";
        let answer_start = "n1[tool]n2[tool]".len();
        assert_eq!(
            select_delivery_text(full, answer_start, false),
            "the final answer"
        );
    }

    #[test]
    fn select_delivery_text_streaming_keeps_full_buffer() {
        // Streaming already showed the text live, so the whole buffer is kept
        // regardless of answer_start.
        let full = "narration then answer";
        assert_eq!(select_delivery_text(full, 10, true), full);
    }

    #[test]
    fn select_delivery_text_send_once_no_tools_keeps_everything() {
        // No tool ever completed → answer_start stays 0 → the whole (tool-free)
        // reply is delivered, including a leading session-reset notice.
        let full = "⚠️ _Session expired, starting fresh..._\n\njust the answer";
        assert_eq!(select_delivery_text(full, 0, false), full);
    }

    #[test]
    fn select_delivery_text_stale_offset_falls_back_to_full() {
        // A byte offset past the end (or a non-char-boundary) must not panic —
        // get(..) returns None and we fall back to the full buffer.
        let full = "abc";
        assert_eq!(select_delivery_text(full, 999, false), full);
        // 1 is a non-boundary inside the multi-byte '✓' (3 bytes); fallback.
        assert_eq!(select_delivery_text("✓x", 1, false), "✓x");
    }

    #[test]
    fn split_delivery_send_once_preserves_leading_directive_across_tools() {
        // Regression: a [[reply_to:...]] emitted at output start, followed by
        // narration + a tool, must survive even though the narration is dropped.
        let full = "[[reply_to:101]]\nlet me check...[tool]the final answer";
        let answer_start = "[[reply_to:101]]\nlet me check...[tool]".len();
        let (directives, body) = split_delivery(full, answer_start, false);
        assert_eq!(directives.reply_to.as_deref(), Some("101"));
        assert_eq!(body, "the final answer");
    }

    #[test]
    fn split_delivery_send_once_no_tools_strips_directive_from_body() {
        // No tool ran (answer_start == 0): the slice still carries the header,
        // so the body must have it stripped while directives are still parsed.
        let full = "[[reply_to:55]]\njust the answer";
        let (directives, body) = split_delivery(full, 0, false);
        assert_eq!(directives.reply_to.as_deref(), Some("55"));
        assert_eq!(body, "just the answer");
    }

    #[test]
    fn split_delivery_streaming_keeps_full_body_and_directive() {
        // Streaming keeps the full buffer; directive parsed and stripped once.
        let full = "[[reply_to:7]]\nnarration then answer";
        let (directives, body) = split_delivery(full, 5, true);
        assert_eq!(directives.reply_to.as_deref(), Some("7"));
        assert_eq!(body, "narration then answer");
    }

    // --- finalize_body: four-corner truth table for the reset re-prepend ---
    //
    // The send-once trimming logic in `stream_prompt_blocks` ends with an
    // inline branch that decides whether to re-prepend the session-reset
    // notice. Extracted into the pure helper `finalize_body` so each corner
    // of (reset, keep_full_text, answer_start) can be exercised without a live
    // ACP session. Mirrors the integration-level concern raised in PR #1115
    // peer review (howie group-review, "Important #3").

    #[test]
    fn finalize_body_reset_send_once_with_tools_prepends_notice() {
        // Reset turn, send-once trimming, a tool advanced answer_start past
        // the notice → the slice no longer contains it → re-prepend.
        let body = "the final answer".to_string();
        let out = finalize_body(true, false, 42, body);
        assert_eq!(
            out, "⚠️ _Session expired, starting fresh..._\n\nthe final answer",
            "send-once + reset + tool ran → notice must be re-prepended"
        );
    }

    #[test]
    fn finalize_body_reset_send_once_no_tools_passes_through() {
        // answer_start == 0 means the slice still equals the full buffer,
        // which already starts with the notice → re-prepending would
        // duplicate it.
        let body = "⚠️ _Session expired, starting fresh..._\n\nthe final answer".to_string();
        let out = finalize_body(true, false, 0, body.clone());
        assert_eq!(
            out, body,
            "send-once + reset + no tools → body already carries notice, pass through"
        );
    }

    #[test]
    fn finalize_body_reset_keep_full_passes_through() {
        // keep_full_text means the slice is the whole buffer (incl. the
        // notice) → must not duplicate, regardless of answer_start.
        let body = "⚠️ _Session expired, starting fresh..._\n\nnarration then answer".to_string();
        let out = finalize_body(true, true, 42, body.clone());
        assert_eq!(
            out, body,
            "keep_full_text → body already carries notice, pass through even with tools"
        );
    }

    #[test]
    fn finalize_body_no_reset_send_once_passes_through() {
        // Non-reset turn: there is no notice to manage regardless of other flags.
        let body = "the final answer".to_string();
        assert_eq!(
            finalize_body(false, false, 42, body.clone()),
            body,
            "no reset → never prepend (send-once + tools)"
        );
    }

    #[test]
    fn finalize_body_no_reset_keep_full_passes_through() {
        // Non-reset turn with keep_full_text: notice is absent, pass through.
        let body = "the final answer".to_string();
        assert_eq!(
            finalize_body(false, true, 0, body.clone()),
            body,
            "no reset → never prepend (keep_full + no tools)"
        );
    }

    /// Compile-time regression guard: use_streaming() is a required trait method
    /// (no default). Any adapter that forgets to implement it will fail to compile.
    /// This test documents the contract — see PR #503 / issue #502 for context.
    #[test]
    fn use_streaming_is_required_method() {
        // If use_streaming() had a default impl, this test module would still
        // compile even if an adapter forgot to override it. The real guard is
        // the trait definition itself — this test exists as documentation and
        // to catch if someone re-adds a default.
        struct TestAdapter;

        #[async_trait]
        impl ChatAdapter for TestAdapter {
            fn platform(&self) -> &'static str {
                "test"
            }
            fn message_limit(&self) -> usize {
                2000
            }
            async fn send_message(&self, _: &ChannelRef, _: &str) -> Result<MessageRef> {
                unimplemented!()
            }
            async fn create_thread(
                &self,
                _: &ChannelRef,
                _: &MessageRef,
                _: &str,
            ) -> Result<ChannelRef> {
                unimplemented!()
            }
            async fn add_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
                Ok(())
            }
            async fn remove_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
                Ok(())
            }
            // use_streaming() MUST be declared — removing this line should fail compilation
            fn use_streaming(&self, _other_bot_present: bool) -> bool {
                false
            }
        }

        let adapter = TestAdapter;
        // Verify the method is callable and returns the declared value
        assert!(!adapter.use_streaming(false));
        // renders_native_tables defaults to false: platforms that don't override
        // it keep the table→code/bullets conversion (e.g. Discord, Gateway).
        assert!(!adapter.renders_native_tables("discord"));
    }

    #[test]
    fn origin_event_id_excluded_from_eq() {
        let a = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_aaa".into()),
        };
        let b = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_bbb".into()),
        };
        assert_eq!(a, b, "same channel with different event IDs must be equal");
    }

    #[test]
    fn origin_event_id_excluded_from_hash() {
        use std::collections::HashMap;
        let a = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_aaa".into()),
        };
        let b = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_bbb".into()),
        };
        let mut map = HashMap::new();
        map.insert(a, "first");
        // b should hit the same bucket and overwrite
        map.insert(b, "second");
        assert_eq!(map.len(), 1);
        assert_eq!(map.values().next(), Some(&"second"));
    }

    #[test]
    fn origin_event_id_survives_clone() {
        let ch = ChannelRef {
            platform: "line".into(),
            channel_id: "U123".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt_abc".into()),
        };
        // Simulates create_thread propagation: clone preserves origin_event_id
        let thread_ch = ChannelRef {
            thread_id: Some("topic_1".into()),
            origin_event_id: ch.origin_event_id.clone(),
            ..ch.clone()
        };
        assert_eq!(thread_ch.origin_event_id.as_deref(), Some("evt_abc"));
    }

    fn tool(id: &str, title: &str, state: ToolState) -> ToolEntry {
        ToolEntry {
            id: id.into(),
            title: title.into(),
            state,
        }
    }

    #[test]
    fn compose_display_full_shows_complete_title() {
        let tools = vec![tool(
            "1",
            "curl -s https://example.com",
            ToolState::Completed,
        )];
        let out = compose_display(&tools, "done", false, ToolDisplay::Full);
        assert!(out.contains("`curl -s https://example.com`"));
    }

    #[test]
    fn compose_display_compact_shows_count_summary() {
        let tools = vec![
            tool("1", "curl -s https://example.com", ToolState::Completed),
            tool("2", "grep -r pattern src/", ToolState::Completed),
            tool("3", "cat /etc/hosts", ToolState::Failed),
        ];
        let out = compose_display(&tools, "done", false, ToolDisplay::Compact);
        assert!(out.contains("✅ 2"), "expected completed count: {out}");
        assert!(out.contains("❌ 1"), "expected failed count: {out}");
        assert!(out.contains("tool(s)"), "expected tool(s) label: {out}");
        // Must NOT contain individual tool names
        assert!(!out.contains("curl"), "should not show tool names: {out}");
        assert!(!out.contains("grep"), "should not show tool names: {out}");
    }

    #[test]
    fn compose_display_compact_shows_running_count() {
        let tools = vec![
            tool("1", "curl", ToolState::Completed),
            tool("2", "npm install", ToolState::Running),
        ];
        let out = compose_display(&tools, "", true, ToolDisplay::Compact);
        assert!(out.contains("✅ 1"), "expected completed count: {out}");
        assert!(out.contains("🔧 1"), "expected running count: {out}");
    }

    #[test]
    fn compose_display_none_hides_tools() {
        let tools = vec![tool(
            "1",
            "curl -s https://example.com",
            ToolState::Completed,
        )];
        let out = compose_display(&tools, "response text", false, ToolDisplay::None);
        assert_eq!(out, "response text");
    }

    #[test]
    fn contains_bot_mention_user() {
        assert!(contains_bot_mention("hello <@1234567890> world"));
    }

    #[test]
    fn contains_bot_mention_nickname() {
        assert!(contains_bot_mention("hey <@!9876543210>"));
    }

    #[test]
    fn contains_bot_mention_role() {
        assert!(contains_bot_mention("calling <@&1496247626675257384>"));
    }

    #[test]
    fn contains_bot_mention_no_match() {
        assert!(!contains_bot_mention("hello world"));
        assert!(!contains_bot_mention("email user@example.com"));
        assert!(!contains_bot_mention("<@not_a_number>"));
        assert!(!contains_bot_mention("<#123456>")); // channel mention
    }

    #[test]
    fn contains_bot_mention_embedded() {
        assert!(contains_bot_mention("請問 <@1501788608439386172> 1+1=?"));
    }
}

#[cfg(test)]
mod directive_tests {
    use super::parse_output_directives;
    use super::{classify_empty_turn, SILENT_FAILURE_MSG};
    use crate::acp::TurnResult;

    #[test]
    fn parse_reply_to_directive() {
        let input = "[[reply_to:1502606076451885136]]\nHello world";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502606076451885136".to_string()));
        assert_eq!(content, "Hello world");
    }

    #[test]
    fn parse_no_directives() {
        let input = "Just plain content\nwith multiple lines";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, input);
    }

    #[test]
    fn parse_multiple_directives() {
        let input = "[[reply_to:123456]]\n[[unknown_key:value]]\nContent here";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123456".to_string()));
        assert_eq!(content, "Content here");
    }

    #[test]
    fn parse_invalid_reply_to_rejects_whitespace() {
        let input = "[[reply_to:has spaces]]\nContent";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_slack_ts_format_accepted() {
        let input = "[[reply_to:1234567890.123456]]\nContent";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1234567890.123456".to_string()));
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_empty_reply_to() {
        let input = "[[reply_to:]]\nContent";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_crlf_line_endings() {
        let input = "[[reply_to:999]]\r\nContent with CRLF";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("999".to_string()));
        assert_eq!(content, "Content with CRLF");
    }

    #[test]
    fn parse_directive_only_no_content() {
        let input = "[[reply_to:123]]";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123".to_string()));
        assert_eq!(content, "");
    }

    #[test]
    fn parse_non_directive_line_stops_parsing() {
        let input = "Normal first line\n[[reply_to:123]]\nMore content";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, input);
    }

    #[test]
    fn parse_duplicate_reply_to_last_wins() {
        let input = "[[reply_to:111]]\n[[reply_to:222]]\nContent";
        let (directives, content) = parse_output_directives(input);
        // Last value wins
        assert_eq!(directives.reply_to, Some("222".to_string()));
        assert_eq!(content, "Content");
    }

    #[test]
    fn parse_crlf_multiple_directives() {
        let input = "[[reply_to:456]]\r\n[[unknown:x]]\r\nContent after CRLF";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("456".to_string()));
        assert_eq!(content, "Content after CRLF");
    }

    #[test]
    fn parse_bracket_without_colon_preserved() {
        // [[Note]] has no colon — not a directive, preserved as content
        let input = "[[Summary]]\nThis is body text";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, None);
        assert_eq!(content, input);
    }

    #[test]
    fn parse_reply_to_with_inline_content() {
        // Agent puts content on same line as directive — should still parse
        let input = "[[reply_to:1502724086474870926]]  @BOT I'm on standby";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502724086474870926".to_string()));
        assert_eq!(content, "@BOT I'm on standby");
    }

    #[test]
    fn parse_reply_to_inline_with_more_lines() {
        let input = "[[reply_to:123]]  First line\nSecond line\nThird line";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123".to_string()));
        assert_eq!(content, "First line\nSecond line\nThird line");
    }

    #[test]
    fn parse_reply_to_no_space_before_content() {
        // No space between ]] and content
        let input = "[[reply_to:1502724086474870926]]收到";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502724086474870926".to_string()));
        assert_eq!(content, "收到");
    }

    #[test]
    fn parse_reply_to_inline_with_mention() {
        // Real-world case: directive followed by Discord mention
        let input = "[[reply_to:1502724086474870926]]  <@1490365068863606784> 我 standby";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("1502724086474870926".to_string()));
        assert_eq!(content, "<@1490365068863606784> 我 standby");
    }

    #[test]
    fn parse_reply_to_inline_only_spaces() {
        // Trailing spaces only — no real content, should be empty
        let input = "[[reply_to:123]]   ";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("123".to_string()));
        assert_eq!(content, "");
    }

    #[test]
    fn parse_reply_to_with_brackets_in_content() {
        // Content after ]] contains brackets — should not confuse parser
        let input = "[[reply_to:456]]  看看 [[這個]] 怎麼樣";
        let (directives, content) = parse_output_directives(input);
        assert_eq!(directives.reply_to, Some("456".to_string()));
        assert_eq!(content, "看看 [[這個]] 怎麼樣");
    }

    // --- classify_empty_turn: adapter-level finalization tests ---

    #[test]
    fn empty_turn_silent_failure_produces_diagnostic() {
        let tr = TurnResult {
            stop_reason: Some("end_turn".into()),
            output_tokens: Some(0),
            input_tokens: Some(0),
            total_tokens: Some(0),
        };
        let result = classify_empty_turn(None, &tr);
        assert_eq!(result, SILENT_FAILURE_MSG);
    }

    #[test]
    fn empty_turn_silent_failure_nonzero_input_still_diagnostic() {
        let tr = TurnResult {
            stop_reason: Some("end_turn".into()),
            output_tokens: Some(0),
            input_tokens: Some(150),
            total_tokens: Some(150),
        };
        let result = classify_empty_turn(None, &tr);
        assert_eq!(result, SILENT_FAILURE_MSG);
    }

    #[test]
    fn empty_turn_response_error_takes_precedence() {
        let tr = TurnResult {
            stop_reason: Some("end_turn".into()),
            output_tokens: Some(0),
            input_tokens: Some(0),
            total_tokens: Some(0),
        };
        let result = classify_empty_turn(Some("Agent process died"), &tr);
        assert_eq!(result, "⚠️ Agent process died");
    }

    #[test]
    fn empty_turn_missing_usage_shows_no_response() {
        let tr = TurnResult::default();
        let result = classify_empty_turn(None, &tr);
        assert_eq!(result, "_(no response)_");
    }

    #[test]
    fn empty_turn_nonzero_output_shows_no_response() {
        let tr = TurnResult {
            stop_reason: Some("end_turn".into()),
            output_tokens: Some(50),
            input_tokens: Some(10),
            total_tokens: Some(60),
        };
        let result = classify_empty_turn(None, &tr);
        assert_eq!(result, "_(no response)_");
    }

    #[test]
    fn empty_turn_different_stop_reason_shows_no_response() {
        let tr = TurnResult {
            stop_reason: Some("max_tokens".into()),
            output_tokens: Some(0),
            input_tokens: Some(10),
            total_tokens: Some(10),
        };
        let result = classify_empty_turn(None, &tr);
        assert_eq!(result, "_(no response)_");
    }
}
