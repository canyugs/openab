//! Shared driver for one active ACP prompt turn.
//!
//! The driver owns event accumulation and canonical platform delivery, but it
//! deliberately does not own connection pooling or session identity.  Text
//! dispatch can therefore supply a pooled connection while action execution
//! supplies a short-lived connection with a stricter policy.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::warn;

use crate::acp::connection::AcpConnection;
use crate::acp::protocol::{ConfigOption, JsonRpcMessage};
use crate::acp::{classify_notification, parse_turn_result, AcpEvent, ContentBlock, TurnResult};
use crate::acp_turn::{
    DeliveryOutcome, ExecutionOutcome, LegacyDispatchDisposition, ObservedToolCall,
    ObservedToolCallStatus, TurnCompletion, TurnOutput, TurnStartError, TurnTicket, UnknownReason,
};
use crate::adapter::{ChannelRef, ChatAdapter, MessageRef};
use crate::config::ToolDisplay;
use crate::error_display::format_coded_error;
use crate::format;
use crate::markdown::{self, TableMode};
use crate::reactions::StatusReactionController;

const DELIVERY_FAILURE: &str =
    "streaming finalization had delivery failures; user view is incomplete";
const DELIVERY_SETUP_FAILURE: &str = "platform delivery setup failed after the ACP turn started";

/// Owned platform and rendering inputs for a single ACP turn.
///
/// Keeping this context independent from `AdapterRouter` is what lets action
/// execution reuse the exact text finalization path without entering the
/// normal message batching or pooled-session path.
pub struct TurnDeliveryContext {
    /// Platform adapter receiving progress and the canonical result.
    pub adapter: Arc<dyn ChatAdapter>,
    /// Channel or thread where the result is delivered.
    pub thread_channel: ChannelRef,
    /// Legacy reaction/status controller for turn progress.
    pub reactions: Arc<StatusReactionController>,
    /// Optional native-stream recipient metadata.
    pub recipient: Option<(String, String)>,
    /// Whether another bot is present, which may disable streaming.
    pub other_bot_present: bool,
    /// Markdown table conversion selected by configuration.
    pub table_mode: TableMode,
    /// Tool-call summary rendering selected by configuration.
    pub tool_display: ToolDisplay,
    /// Whether send-once delivery retains inter-tool narration.
    pub narration_display: bool,
    /// Absolute ceiling for one active prompt.
    pub prompt_hard_timeout: Duration,
    /// Polling cadence for process liveness and the hard ceiling.
    pub liveness_check_interval: Duration,
}

/// Stateless ACP prompt driver.
pub struct AcpTurnDriver;

/// Receivers for one detached ACP turn.
///
/// The driver task owns the connection lock and continues to terminal cleanup
/// even if this handle is dropped. Action executors may await [`Self::ticket`]
/// to obtain the exact active turn before requesting cancellation; normal text
/// dispatch only awaits [`Self::wait`].
pub struct RunningTurn {
    ticket_rx: Option<oneshot::Receiver<TurnTicket>>,
    completion_rx: oneshot::Receiver<Result<TurnCompletion>>,
}

impl RunningTurn {
    /// Wait until `session/prompt` has been flushed and its exact ticket exists.
    pub async fn ticket(&mut self) -> Result<TurnTicket> {
        let receiver = self
            .ticket_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("turn ticket was already consumed"))?;
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("turn ended before an active ticket was issued"))
    }

    /// Wait for the driver's single typed terminal result.
    pub async fn wait(self) -> Result<TurnCompletion> {
        self.completion_rx
            .await
            .map_err(|_| anyhow::anyhow!("turn driver ended without a completion"))?
    }
}

/// The connection operations needed to drive exactly one ACP prompt turn.
///
/// Keeping this seam crate-private prevents callers from manufacturing turn
/// tickets while allowing the lifecycle and delivery contract to be tested
/// without spawning an agent subprocess.
#[async_trait]
pub(crate) trait TurnConnection: Send {
    /// Consume the one-shot session-reset marker for this turn.
    fn take_session_reset(&mut self) -> bool;

    /// Flush a prompt and return its notification stream and exact ticket.
    async fn session_prompt(
        &mut self,
        content_blocks: Vec<ContentBlock>,
    ) -> std::result::Result<(mpsc::UnboundedReceiver<JsonRpcMessage>, TurnTicket), TurnStartError>;

    /// Whether the underlying ACP process is still alive.
    fn alive(&self) -> bool;

    /// Whether delivery of targeted cancellation became indeterminate for the
    /// exact active ticket.
    async fn cancel_write_is_indeterminate(&self, ticket: &TurnTicket) -> bool;

    /// Best-effort abandonment of the exact active ticket.
    async fn abandon_request(&mut self, ticket: &TurnTicket);

    /// Finish bookkeeping for the exact active ticket.
    async fn prompt_done(&mut self, ticket: &TurnTicket);

    /// Apply session configuration updates observed during the turn.
    fn update_config_options(&mut self, options: Vec<ConfigOption>);
}

#[async_trait]
impl TurnConnection for AcpConnection {
    fn take_session_reset(&mut self) -> bool {
        std::mem::take(&mut self.session_reset)
    }

    async fn session_prompt(
        &mut self,
        content_blocks: Vec<ContentBlock>,
    ) -> std::result::Result<(mpsc::UnboundedReceiver<JsonRpcMessage>, TurnTicket), TurnStartError>
    {
        AcpConnection::session_prompt_typed(self, content_blocks).await
    }

    fn alive(&self) -> bool {
        AcpConnection::alive(self)
    }

    async fn cancel_write_is_indeterminate(&self, ticket: &TurnTicket) -> bool {
        AcpConnection::cancel_write_is_indeterminate(self, ticket).await
    }

    async fn abandon_request(&mut self, ticket: &TurnTicket) {
        AcpConnection::abandon_request_typed(self, ticket).await;
    }

    async fn prompt_done(&mut self, ticket: &TurnTicket) {
        AcpConnection::prompt_done_typed(self, ticket).await;
    }

    fn update_config_options(&mut self, options: Vec<ConfigOption>) {
        self.config_options = options;
    }
}

impl AcpTurnDriver {
    /// Start a turn in a detached task that owns the connection lock through
    /// final cleanup. Dropping the returned receivers never aborts the task.
    pub fn spawn(
        connection: Arc<Mutex<AcpConnection>>,
        content_blocks: Vec<ContentBlock>,
        context: TurnDeliveryContext,
    ) -> RunningTurn {
        Self::spawn_with_connection(connection, content_blocks, context)
    }

    fn spawn_with_connection<C>(
        connection: Arc<Mutex<C>>,
        content_blocks: Vec<ContentBlock>,
        context: TurnDeliveryContext,
    ) -> RunningTurn
    where
        C: TurnConnection + 'static,
    {
        let (ticket_tx, ticket_rx) = oneshot::channel();
        let (completion_tx, completion_rx) = oneshot::channel();

        tokio::spawn(async move {
            let result = {
                let mut connection = connection.lock().await;
                Self::run_with_connection_notifying(
                    &mut *connection,
                    content_blocks,
                    context,
                    Some(ticket_tx),
                )
                .await
            };
            let _ = completion_tx.send(result);
        });

        RunningTurn {
            ticket_rx: Some(ticket_rx),
            completion_rx,
        }
    }

    #[cfg(test)]
    async fn run_with_connection<C: TurnConnection + ?Sized>(
        conn: &mut C,
        content_blocks: Vec<ContentBlock>,
        context: TurnDeliveryContext,
    ) -> Result<TurnCompletion> {
        Self::run_with_connection_notifying(conn, content_blocks, context, None).await
    }

    async fn run_with_connection_notifying<C: TurnConnection + ?Sized>(
        conn: &mut C,
        content_blocks: Vec<ContentBlock>,
        context: TurnDeliveryContext,
        turn_started: Option<oneshot::Sender<TurnTicket>>,
    ) -> Result<TurnCompletion> {
        let TurnDeliveryContext {
            adapter,
            thread_channel,
            reactions,
            recipient,
            other_bot_present,
            table_mode: configured_table_mode,
            tool_display,
            narration_display,
            prompt_hard_timeout,
            liveness_check_interval,
        } = context;

        let message_limit = adapter.message_limit();
        let streaming = adapter.use_streaming(other_bot_present);
        let keep_full_text = streaming || narration_display;
        let native = adapter.uses_native_streaming(other_bot_present);
        let assistant_status = adapter.uses_assistant_status();
        let table_mode = if adapter.renders_native_tables(&thread_channel.platform) {
            TableMode::Off
        } else {
            configured_table_mode
        };

        let reset = conn.take_session_reset();

        // Only failures proven to precede the stdin write escape the driver.
        // A write/flush error may have delivered partial JSON and therefore
        // resolves as a typed unknown outcome after exact-ticket cleanup.
        let (mut rx, ticket) = match conn.session_prompt(content_blocks).await {
            Ok(started) => started,
            Err(TurnStartError::NotStarted { error }) => return Err(anyhow::anyhow!(error)),
            Err(TurnStartError::WriteIndeterminate { ticket, error }) => {
                conn.prompt_done(&ticket).await;
                return Ok(TurnCompletion {
                    ticket,
                    execution: ExecutionOutcome::OutcomeUnknown {
                        reason: UnknownReason::PromptWriteIndeterminate {
                            error: error.clone(),
                        },
                        observed_tool_calls: Vec::new(),
                    },
                    output: None,
                    delivery: DeliveryOutcome::NotAttempted,
                    legacy_dispatch: LegacyDispatchDisposition::Failed { error },
                });
            }
        };
        if let Some(sender) = turn_started {
            let _ = sender.send(ticket.clone());
        }

        if assistant_status {
            let _ = adapter.set_status(&thread_channel, "Thinking…").await;
        } else {
            reactions.set_thinking().await;
        }

        let mut text_buf = String::new();
        let mut agent_text_buf = String::new();
        let mut tool_lines: Vec<ToolEntry> = Vec::new();
        let mut observed_tools: Vec<ObservedToolCall> = Vec::new();
        let mut answer_start = 0usize;
        let mut agent_answer_start = 0usize;

        if reset {
            text_buf.push_str(SESSION_RESET_NOTICE);
        }

        // Native streaming opens lazily on the first text event.
        let mut native_msg: Option<MessageRef> = None;
        let mut stream_begin_failed = false;
        let mut native_pending = String::new();
        let mut native_last_flush = tokio::time::Instant::now();
        const NATIVE_FLUSH_MS: u128 = 400;

        // A placeholder failure happens after the prompt was flushed.  It must
        // therefore resolve as unknown execution plus failed delivery rather
        // than escaping with `?` and losing the active ticket.
        let mut delivery_setup_error: Option<String> = None;
        let (buf_tx, placeholder_msg, edit_handle) = if streaming && !native {
            let initial = if reset {
                format!("{SESSION_RESET_NOTICE}…")
            } else {
                "…".to_string()
            };
            let msg = if adapter.show_streaming_placeholder() {
                match adapter.send_message(&thread_channel, &initial).await {
                    Ok(msg) => Some(msg),
                    Err(error) => {
                        delivery_setup_error = Some(error.to_string());
                        None
                    }
                }
            } else {
                Some(MessageRef {
                    message_id: "draft".to_string(),
                    channel: thread_channel.clone(),
                })
            };

            if let Some(msg) = msg {
                let (tx, rx) = tokio::sync::watch::channel(initial);
                let edit_adapter = adapter.clone();
                let edit_msg = msg.clone();
                let mut buf_rx = rx;
                let edit_handle = tokio::spawn(async move {
                    let mut last = String::new();
                    let mut consecutive_failures: u32 = 0;
                    const MAX_CONSECUTIVE_FAILURES: u32 = 3;
                    loop {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        if buf_rx.has_changed().unwrap_or(false) {
                            let content = buf_rx.borrow_and_update().clone();
                            if content != last {
                                let display = if content.chars().count() > message_limit - 100 {
                                    format!(
                                        "…{}",
                                        format::truncate_chars_tail(&content, message_limit - 100)
                                    )
                                } else {
                                    content.clone()
                                };
                                match edit_adapter.edit_message(&edit_msg, &display).await {
                                    Ok(()) => {
                                        consecutive_failures = 0;
                                        last = content;
                                    }
                                    Err(error) => {
                                        consecutive_failures += 1;
                                        tracing::debug!(
                                            message_id = %edit_msg.message_id,
                                            platform = %edit_msg.channel.platform,
                                            error = ?error,
                                            consecutive_failures,
                                            "mid-stream cosmetic edit failed"
                                        );
                                        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                                            tracing::warn!(
                                                message_id = %edit_msg.message_id,
                                                platform = %edit_msg.channel.platform,
                                                consecutive_failures,
                                                "mid-stream cosmetic edit aborted; final content will be delivered at turn end"
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        if buf_rx.has_changed().is_err() {
                            break;
                        }
                    }
                });
                (Some(tx), Some(msg), Some(edit_handle))
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        let mut response_error: Option<String> = None;
        let mut terminal = delivery_setup_error.as_ref().map(|_| {
            TerminalObservation::Unknown(UnknownReason::DeliverySetupFailure {
                error: DELIVERY_SETUP_FAILURE.to_string(),
            })
        });
        let mut request_abandoned = delivery_setup_error.is_some();

        if request_abandoned {
            conn.abandon_request(&ticket).await;
        }

        let prompt_start = tokio::time::Instant::now();
        while terminal.is_none() {
            let notification = tokio::select! {
                msg = rx.recv() => match msg {
                    Some(notification) => notification,
                    None => {
                        let reason = if conn.cancel_write_is_indeterminate(&ticket).await {
                            response_error = Some(
                                "ACP cancellation delivery was indeterminate; agent terminated"
                                    .to_string(),
                            );
                            UnknownReason::CancelWriteIndeterminate
                        } else {
                            response_error = Some("Agent process exited unexpectedly".to_string());
                            UnknownReason::AgentExited
                        };
                        terminal = Some(TerminalObservation::Unknown(reason));
                        request_abandoned = true;
                        break;
                    }
                },
                _ = tokio::time::sleep(liveness_check_interval) => {
                    if conn.cancel_write_is_indeterminate(&ticket).await {
                        response_error = Some(
                            "ACP cancellation delivery was indeterminate; agent terminated"
                                .to_string(),
                        );
                        terminal = Some(TerminalObservation::Unknown(
                            UnknownReason::CancelWriteIndeterminate,
                        ));
                        request_abandoned = true;
                        break;
                    }
                    if !conn.alive() {
                        response_error = Some("Agent process died".to_string());
                        terminal = Some(TerminalObservation::Unknown(UnknownReason::AgentDied));
                        request_abandoned = true;
                        break;
                    }
                    if prompt_start.elapsed() > prompt_hard_timeout {
                        response_error = Some(format!(
                            "Agent exceeded hard timeout ({}s)",
                            prompt_hard_timeout.as_secs(),
                        ));
                        terminal = Some(TerminalObservation::Unknown(
                            UnknownReason::HardTimeout {
                                seconds: prompt_hard_timeout.as_secs(),
                            },
                        ));
                        request_abandoned = true;
                        break;
                    }
                    continue;
                }
            };

            if let Some(notification_id) = notification.id {
                if notification_id != ticket.request_id() {
                    continue;
                }

                if let Some(error) = &notification.error {
                    let display_error =
                        format_coded_error(error.code, &error.message, error.data_message());
                    let audit_error = format_coded_error(error.code, "", None);
                    response_error = Some(display_error);
                    terminal = Some(TerminalObservation::Unknown(UnknownReason::AcpError {
                        error: audit_error,
                    }));
                } else if let Some(result) = &notification.result {
                    let turn_result = parse_turn_result(result);
                    terminal = if is_cancelled(&turn_result) {
                        Some(TerminalObservation::Cancelled(turn_result))
                    } else {
                        Some(TerminalObservation::Succeeded(turn_result))
                    };
                } else {
                    terminal = Some(TerminalObservation::Unknown(UnknownReason::MissingResult));
                }
                break;
            }

            if let Some(event) = classify_notification(&notification) {
                match event {
                    AcpEvent::Text(text) => {
                        text_buf.push_str(&text);
                        agent_text_buf.push_str(&text);
                        if native {
                            if native_msg.is_none() && !stream_begin_failed {
                                match adapter
                                    .stream_begin(&thread_channel, recipient.clone())
                                    .await
                                {
                                    Ok(message) => native_msg = Some(message),
                                    Err(error) => {
                                        tracing::error!(
                                            error = ?error,
                                            "stream_begin failed on first text; will not retry this turn"
                                        );
                                        stream_begin_failed = true;
                                    }
                                }
                            }
                            if let Some(message) = &native_msg {
                                native_pending.push_str(&text);
                                if native_last_flush.elapsed().as_millis() >= NATIVE_FLUSH_MS
                                    && !native_pending.is_empty()
                                {
                                    let _ = adapter.stream_append(message, &native_pending).await;
                                    native_pending.clear();
                                    native_last_flush = tokio::time::Instant::now();
                                }
                            }
                        } else if let Some(tx) = &buf_tx {
                            let _ = tx.send(compose_display(
                                &tool_lines,
                                &text_buf,
                                true,
                                tool_display,
                            ));
                        }
                    }
                    AcpEvent::Thinking => {
                        if assistant_status {
                            let _ = adapter.set_status(&thread_channel, "Thinking…").await;
                        } else {
                            reactions.set_thinking().await;
                        }
                    }
                    AcpEvent::ToolStart { id, title } => {
                        observe_tool_call(
                            &mut observed_tools,
                            &id,
                            &title,
                            ObservedToolCallStatus::Started,
                        );
                        if title.is_empty() {
                            continue;
                        }
                        if assistant_status {
                            let _ = adapter
                                .set_status(&thread_channel, &format!("Using {title}…"))
                                .await;
                        } else {
                            reactions.set_tool(&title).await;
                        }
                        let title = sanitize_title(&title);
                        if let Some(slot) = tool_lines.iter_mut().find(|entry| entry.id == id) {
                            slot.title = title;
                            slot.state = ToolState::Running;
                        } else {
                            tool_lines.push(ToolEntry {
                                id,
                                title,
                                state: ToolState::Running,
                            });
                        }
                        if let Some(tx) = &buf_tx {
                            let _ = tx.send(compose_display(
                                &tool_lines,
                                &text_buf,
                                true,
                                tool_display,
                            ));
                        }
                    }
                    AcpEvent::ToolDone { id, title, status } => {
                        let observed_status = if status == "completed" {
                            ObservedToolCallStatus::Completed
                        } else {
                            ObservedToolCallStatus::Failed
                        };
                        observe_tool_call(&mut observed_tools, &id, &title, observed_status);
                        answer_start = text_buf.len();
                        agent_answer_start = agent_text_buf.len();
                        if assistant_status {
                            let _ = adapter.set_status(&thread_channel, "Thinking…").await;
                        } else {
                            reactions.set_thinking().await;
                        }
                        let new_state = if status == "completed" {
                            ToolState::Completed
                        } else {
                            ToolState::Failed
                        };
                        if let Some(slot) = tool_lines.iter_mut().find(|entry| entry.id == id) {
                            if !title.is_empty() {
                                slot.title = sanitize_title(&title);
                            }
                            slot.state = new_state;
                        } else if !title.is_empty() {
                            tool_lines.push(ToolEntry {
                                id,
                                title: sanitize_title(&title),
                                state: new_state,
                            });
                        }
                        if let Some(tx) = &buf_tx {
                            let _ = tx.send(compose_display(
                                &tool_lines,
                                &text_buf,
                                true,
                                tool_display,
                            ));
                        }
                    }
                    AcpEvent::ConfigUpdate { options } => {
                        conn.update_config_options(options);
                    }
                    _ => {}
                }
            }
        }

        if request_abandoned && delivery_setup_error.is_none() {
            conn.abandon_request(&ticket).await;
        }

        // The one post-start cleanup point.  No branch below can return early.
        conn.prompt_done(&ticket).await;
        drop(buf_tx);
        if let Some(handle) = edit_handle {
            handle.abort();
            let _ = handle.await;
        }

        let completion = if let Some(error) = delivery_setup_error {
            TurnCompletion {
                ticket,
                execution: ExecutionOutcome::OutcomeUnknown {
                    reason: UnknownReason::DeliverySetupFailure {
                        error: DELIVERY_SETUP_FAILURE.to_string(),
                    },
                    observed_tool_calls: observed_tools,
                },
                output: None,
                delivery: DeliveryOutcome::Failed {
                    error: DELIVERY_SETUP_FAILURE.to_string(),
                    partially_delivered: false,
                },
                legacy_dispatch: LegacyDispatchDisposition::Failed { error },
            }
        } else {
            let (_, authoritative_answer) =
                split_delivery(&agent_text_buf, agent_answer_start, false);
            let authoritative_output_present = !authoritative_answer.trim().is_empty();
            let terminal =
                terminal.unwrap_or(TerminalObservation::Unknown(UnknownReason::MissingResult));
            let turn_result_for_display = match &terminal {
                TerminalObservation::Succeeded(turn_result)
                | TerminalObservation::Cancelled(turn_result) => turn_result.clone(),
                TerminalObservation::Unknown(_) => TurnResult::default(),
            };
            let execution =
                classify_execution(terminal, authoritative_output_present, observed_tools);

            let (directives, text_body) = split_delivery(&text_buf, answer_start, keep_full_text);
            let text_body = finalize_body(reset, keep_full_text, answer_start, text_body);
            // Speech is always the directive-free final agent answer, never the
            // streaming/narration display buffer or session-reset notice.
            let speech_text = authoritative_answer;

            let final_content = compose_display(&tool_lines, &text_body, false, tool_display);
            let final_content = if final_content.is_empty() {
                if turn_result_for_display.is_silent_failure() {
                    warn!(
                        stop_reason = ?turn_result_for_display.stop_reason,
                        input_tokens = ?turn_result_for_display.input_tokens,
                        output_tokens = ?turn_result_for_display.output_tokens,
                        total_tokens = ?turn_result_for_display.total_tokens,
                        "agent returned empty turn (0 output tokens) — likely provider/model/auth failure"
                    );
                }
                classify_empty_turn(response_error.as_deref(), &turn_result_for_display)
            } else if let Some(error) = response_error {
                format!("⚠️ {error}\n\n{final_content}")
            } else {
                final_content
            };

            let final_content = markdown::convert_tables(&final_content, table_mode);
            let chunks = if adapter.platform() == "discord" {
                let mentions = extract_mentions(&final_content);
                let mention_reserve = mention_footer_len(&mentions);
                let chunks = format::split_message(
                    &final_content,
                    message_limit.saturating_sub(mention_reserve),
                );
                propagate_mentions_to_chunks(chunks, &mentions, message_limit)
            } else {
                format::split_message(&final_content, message_limit)
            };

            let mut delivery_errors = Vec::new();
            // A native stream or edit-based placeholder may already have exposed
            // partial text before authoritative finalization fails.
            let mut content_delivered = native_msg.is_some() || placeholder_msg.is_some();
            if assistant_status {
                let _ = adapter.set_status(&thread_channel, "").await;
            }

            if native {
                if let Some(message) = &native_msg {
                    if !native_pending.is_empty() {
                        if let Err(error) = adapter.stream_append(message, &native_pending).await {
                            tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "native finalize stream_append failed");
                            delivery_errors.push(error.to_string());
                        }
                    }
                    match chunks.first() {
                        Some(first) => {
                            match adapter.stream_finish(message, first).await {
                                Ok(()) => content_delivered = true,
                                Err(error) => {
                                    tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "native stream_finish failed");
                                    delivery_errors.push(error.to_string());
                                }
                            }
                            for chunk in chunks.iter().skip(1) {
                                match adapter.send_message(&thread_channel, chunk).await {
                                    Ok(_) => content_delivered = true,
                                    Err(error) => {
                                        tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "native overflow chunk send failed");
                                        delivery_errors.push(error.to_string());
                                    }
                                }
                            }
                        }
                        None => match adapter.stream_finish(message, &final_content).await {
                            Ok(()) => content_delivered = true,
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "native stream_finish (no chunks) failed");
                                delivery_errors.push(error.to_string());
                            }
                        },
                    }
                } else {
                    for chunk in &chunks {
                        match adapter.send_message(&thread_channel, chunk).await {
                            Ok(_) => content_delivered = true,
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, "native fallback chunk send failed");
                                delivery_errors.push(error.to_string());
                            }
                        }
                    }
                }
            } else if let Some(message) = placeholder_msg {
                if let Some(reply_id) = &directives.reply_to {
                    let mut send_ok = false;
                    let mut first = true;
                    for chunk in &chunks {
                        if first {
                            match adapter
                                .send_message_with_reply(&thread_channel, chunk, reply_id)
                                .await
                            {
                                Ok(_) => {
                                    send_ok = true;
                                    content_delivered = true;
                                }
                                Err(error) => {
                                    tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "reply_to send failed; preserving placeholder");
                                    delivery_errors.push(error.to_string());
                                }
                            }
                        } else {
                            match adapter.send_message(&thread_channel, chunk).await {
                                Ok(_) => content_delivered = true,
                                Err(error) => {
                                    tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "reply_to overflow chunk send failed");
                                    delivery_errors.push(error.to_string());
                                }
                            }
                        }
                        first = false;
                    }
                    if send_ok {
                        if let Err(error) = adapter.delete_message(&message).await {
                            tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "delete placeholder failed; placeholder will remain visible");
                        }
                    }
                } else if adapter.platform() == "discord" && contains_bot_mention(&final_content) {
                    let mut send_ok = false;
                    if let Some(first) = chunks.first() {
                        match adapter.send_message(&thread_channel, first).await {
                            Ok(_) => {
                                send_ok = true;
                                content_delivered = true;
                            }
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "discord bot-mention first chunk send failed");
                                delivery_errors.push(error.to_string());
                            }
                        }
                    }
                    for chunk in chunks.iter().skip(1) {
                        match adapter.send_message(&thread_channel, chunk).await {
                            Ok(_) => content_delivered = true,
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "streaming overflow chunk send failed");
                                delivery_errors.push(error.to_string());
                            }
                        }
                    }
                    if send_ok {
                        let _ = adapter.delete_message(&message).await;
                    }
                } else if message.message_id == "draft" {
                    for chunk in &chunks {
                        match adapter.send_message(&thread_channel, chunk).await {
                            Ok(_) => content_delivered = true,
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "draft placeholder fallback chunk send failed");
                                delivery_errors.push(error.to_string());
                            }
                        }
                    }
                } else if let Some(first) = chunks.first() {
                    match adapter.edit_message(&message, first).await {
                        Ok(()) => content_delivered = true,
                        Err(error) => {
                            tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "final streaming edit failed; deleting placeholder and sending fresh");
                            if let Err(delete_error) = adapter.delete_message(&message).await {
                                tracing::warn!(error = ?delete_error, platform = %thread_channel.platform, message_id = %message.message_id, "delete placeholder failed; user will see overlap");
                            }
                            match adapter.send_message(&thread_channel, first).await {
                                Ok(_) => content_delivered = true,
                                Err(fallback_error) => {
                                    tracing::error!(error = ?fallback_error, platform = %thread_channel.platform, message_id = %message.message_id, "fallback send_message also failed");
                                    delivery_errors.push(fallback_error.to_string());
                                }
                            }
                        }
                    }
                    for chunk in chunks.iter().skip(1) {
                        match adapter.send_message(&thread_channel, chunk).await {
                            Ok(_) => content_delivered = true,
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, message_id = %message.message_id, "streaming overflow chunk send failed");
                                delivery_errors.push(error.to_string());
                            }
                        }
                    }
                }
            } else {
                let mut first = true;
                for chunk in &chunks {
                    if first {
                        if let Some(reply_id) = &directives.reply_to {
                            match adapter
                                .send_message_with_reply(&thread_channel, chunk, reply_id)
                                .await
                            {
                                Ok(_) => content_delivered = true,
                                Err(error) => {
                                    tracing::warn!(error = ?error, platform = %thread_channel.platform, "send-once reply_to first chunk failed");
                                    delivery_errors.push(error.to_string());
                                }
                            }
                        } else {
                            match adapter.send_message(&thread_channel, chunk).await {
                                Ok(_) => content_delivered = true,
                                Err(error) => {
                                    tracing::warn!(error = ?error, platform = %thread_channel.platform, "send-once first chunk failed");
                                    delivery_errors.push(error.to_string());
                                }
                            }
                        }
                    } else {
                        match adapter.send_message(&thread_channel, chunk).await {
                            Ok(_) => content_delivered = true,
                            Err(error) => {
                                tracing::warn!(error = ?error, platform = %thread_channel.platform, "send-once subsequent chunk failed");
                                delivery_errors.push(error.to_string());
                            }
                        }
                    }
                    first = false;
                }
            }

            let (delivery, legacy_dispatch) = if delivery_errors.is_empty() {
                (
                    DeliveryOutcome::Delivered,
                    LegacyDispatchDisposition::Succeeded,
                )
            } else {
                (
                    DeliveryOutcome::Failed {
                        error: DELIVERY_FAILURE.to_string(),
                        partially_delivered: content_delivered,
                    },
                    LegacyDispatchDisposition::Failed {
                        error: DELIVERY_FAILURE.to_string(),
                    },
                )
            };

            TurnCompletion {
                ticket,
                execution,
                output: Some(TurnOutput {
                    display_text: final_content,
                    speech_text,
                }),
                delivery,
                legacy_dispatch,
            }
        };

        Ok(completion)
    }
}

enum TerminalObservation {
    Succeeded(TurnResult),
    Cancelled(TurnResult),
    Unknown(UnknownReason),
}

fn classify_execution(
    terminal: TerminalObservation,
    authoritative_output_present: bool,
    observed_tool_calls: Vec<ObservedToolCall>,
) -> ExecutionOutcome {
    match terminal {
        TerminalObservation::Succeeded(turn_result) if authoritative_output_present => {
            ExecutionOutcome::Succeeded { turn_result }
        }
        TerminalObservation::Succeeded(_) => ExecutionOutcome::OutcomeUnknown {
            reason: UnknownReason::MissingOutput,
            observed_tool_calls,
        },
        TerminalObservation::Cancelled(turn_result) => ExecutionOutcome::Cancelled {
            turn_result,
            observed_tool_calls,
        },
        TerminalObservation::Unknown(reason) => ExecutionOutcome::OutcomeUnknown {
            reason,
            observed_tool_calls,
        },
    }
}

fn is_cancelled(turn_result: &TurnResult) -> bool {
    matches!(
        turn_result.stop_reason.as_deref(),
        Some(reason) if reason.eq_ignore_ascii_case("cancelled")
            || reason.eq_ignore_ascii_case("canceled")
    )
}

const SESSION_RESET_NOTICE: &str = "⚠️ _Session expired, starting fresh..._\n\n";

/// Directives parsed from the leading header of agent output.
#[derive(Default, Debug)]
pub struct OutputDirectives {
    /// Platform message ID that the finalized answer should reply to.
    pub reply_to: Option<String>,
}

/// Parse consecutive `[[key:value]]` directives at the start of output.
///
/// The returned body has the directive header removed. Unknown directives are
/// ignored so adding a directive does not break older OpenAB versions.
pub fn parse_output_directives(content: &str) -> (OutputDirectives, String) {
    let mut directives = OutputDirectives::default();
    let mut content_start = 0;
    let mut trailing_content: Option<&str> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after_open) = trimmed.strip_prefix("[[") {
            if let Some(close_pos) = after_open.find("]]") {
                let inner = &after_open[..close_pos];
                if let Some((key, value)) = inner.split_once(':') {
                    if key.trim() == "reply_to" {
                        let value = value.trim();
                        if !value.is_empty()
                            && value.len() <= 64
                            && value.chars().all(|character| {
                                character.is_ascii_alphanumeric()
                                    || character == '.'
                                    || character == '-'
                                    || character == '_'
                            })
                        {
                            directives.reply_to = Some(value.to_string());
                        }
                    } else {
                        tracing::debug!(key = key.trim(), "unknown output directive ignored");
                    }
                    let remainder = after_open[close_pos + 2..].trim();
                    if !remainder.is_empty() {
                        trailing_content = Some(remainder);
                        advance_line_ending(content, &mut content_start, line.len());
                        break;
                    }
                    advance_line_ending(content, &mut content_start, line.len());
                } else {
                    break;
                }
            } else {
                break;
            }
        } else {
            break;
        }
    }

    let remaining = if let Some(trailing) = trailing_content {
        if content_start < content.len() {
            format!("{trailing}\n{}", &content[content_start..])
        } else {
            trailing.to_string()
        }
    } else if content_start < content.len() {
        content[content_start..].to_string()
    } else {
        String::new()
    };
    (directives, remaining)
}

fn advance_line_ending(content: &str, content_start: &mut usize, line_len: usize) {
    *content_start += line_len;
    if content.as_bytes().get(*content_start) == Some(&b'\r') {
        *content_start += 1;
    }
    if content.as_bytes().get(*content_start) == Some(&b'\n') {
        *content_start += 1;
    }
}

/// Select the whole streamed buffer or only the post-tool final answer block.
pub fn select_delivery_text(full: &str, answer_start: usize, keep_full: bool) -> &str {
    if keep_full {
        full
    } else {
        full.get(answer_start..).unwrap_or_else(|| {
            tracing::warn!(
                answer_start,
                full_len = full.len(),
                "stale answer_start offset; delivering full buffer"
            );
            full
        })
    }
}

/// Resolve leading output directives and the exact body to deliver.
pub fn split_delivery(
    full: &str,
    answer_start: usize,
    keep_full: bool,
) -> (OutputDirectives, String) {
    let (directives, _) = parse_output_directives(full);
    let delivered = select_delivery_text(full, answer_start, keep_full);
    let body = if answer_start == 0 || keep_full {
        parse_output_directives(delivered).1
    } else {
        delivered.to_owned()
    };
    (directives, body)
}

pub(crate) fn finalize_body(
    reset: bool,
    keep_full_text: bool,
    answer_start: usize,
    body: String,
) -> String {
    if reset && !keep_full_text && answer_start > 0 {
        format!("{SESSION_RESET_NOTICE}{body}")
    } else {
        body
    }
}

pub(crate) fn contains_bot_mention(content: &str) -> bool {
    let mut index = 0;
    let bytes = content.as_bytes();
    while index + 2 < bytes.len() {
        if bytes[index] == b'<' && bytes[index + 1] == b'@' {
            let start = if index + 2 < bytes.len()
                && (bytes[index + 2] == b'!' || bytes[index + 2] == b'&')
            {
                index + 3
            } else {
                index + 2
            };
            if start < bytes.len() && bytes[start].is_ascii_digit() {
                if let Some(end) = content[start..].find('>') {
                    if content[start..start + end]
                        .chars()
                        .all(|character| character.is_ascii_digit())
                    {
                        return true;
                    }
                }
            }
            index = start;
        } else {
            index += 1;
        }
    }
    false
}

pub(crate) fn sanitize_title(title: &str) -> String {
    title
        .replace('\r', "")
        .replace('\n', " ; ")
        .replace('`', "'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolState {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) state: ToolState,
}

impl ToolEntry {
    fn render(&self) -> String {
        let icon = match self.state {
            ToolState::Running => "🔧",
            ToolState::Completed => "✅",
            ToolState::Failed => "❌",
        };
        let suffix = if self.state == ToolState::Running {
            "..."
        } else {
            ""
        };
        format!("{icon} `{}`{suffix}", self.title)
    }
}

fn observe_tool_call(
    observed_tools: &mut Vec<ObservedToolCall>,
    id: &str,
    title: &str,
    status: ObservedToolCallStatus,
) {
    if let Some(tool) = observed_tools.iter_mut().find(|tool| tool.id == id) {
        if !title.is_empty() {
            tool.title = sanitize_title(title);
        }
        tool.status = status;
    } else {
        observed_tools.push(ObservedToolCall {
            id: id.to_string(),
            title: sanitize_title(title),
            status,
        });
    }
}

const TOOL_COLLAPSE_THRESHOLD: usize = 3;

pub(crate) const SILENT_FAILURE_MSG: &str = "⚠️ The agent did not produce a response. This usually indicates a backend configuration issue — not an intentional empty reply. Please try again later.";

pub(crate) fn classify_empty_turn(
    response_error: Option<&str>,
    turn_result: &TurnResult,
) -> String {
    if let Some(error) = response_error {
        format!("⚠️ {error}")
    } else if turn_result.is_silent_failure() {
        SILENT_FAILURE_MSG.to_string()
    } else {
        "_(no response)_".to_string()
    }
}

pub(crate) fn compose_display(
    tool_lines: &[ToolEntry],
    text: &str,
    streaming: bool,
    tool_display: ToolDisplay,
) -> String {
    let mut output = String::new();
    if !tool_lines.is_empty() && tool_display != ToolDisplay::None {
        let done = tool_lines
            .iter()
            .filter(|entry| entry.state == ToolState::Completed)
            .count();
        let failed = tool_lines
            .iter()
            .filter(|entry| entry.state == ToolState::Failed)
            .count();
        let running = tool_lines
            .iter()
            .filter(|entry| entry.state == ToolState::Running)
            .count();
        let finished = done + failed;

        match tool_display {
            ToolDisplay::Compact => {
                let mut parts = Vec::new();
                if done > 0 {
                    parts.push(format!("✅ {done}"));
                }
                if failed > 0 {
                    parts.push(format!("❌ {failed}"));
                }
                if running > 0 {
                    parts.push(format!("🔧 {running}"));
                }
                if !parts.is_empty() {
                    output.push_str(&format!("{} tool(s)\n", parts.join(" · ")));
                }
            }
            ToolDisplay::Full => {
                if streaming {
                    let running_entries: Vec<_> = tool_lines
                        .iter()
                        .filter(|entry| entry.state == ToolState::Running)
                        .collect();
                    if finished <= TOOL_COLLAPSE_THRESHOLD {
                        for entry in tool_lines
                            .iter()
                            .filter(|entry| entry.state != ToolState::Running)
                        {
                            output.push_str(&entry.render());
                            output.push('\n');
                        }
                    } else {
                        let mut parts = Vec::new();
                        if done > 0 {
                            parts.push(format!("✅ {done}"));
                        }
                        if failed > 0 {
                            parts.push(format!("❌ {failed}"));
                        }
                        output.push_str(&format!("{} tool(s) completed\n", parts.join(" · ")));
                    }
                    if running_entries.len() <= TOOL_COLLAPSE_THRESHOLD {
                        for entry in &running_entries {
                            output.push_str(&entry.render());
                            output.push('\n');
                        }
                    } else {
                        let hidden = running_entries.len() - TOOL_COLLAPSE_THRESHOLD;
                        output.push_str(&format!("🔧 {hidden} more running\n"));
                        for entry in running_entries.iter().skip(hidden) {
                            output.push_str(&entry.render());
                            output.push('\n');
                        }
                    }
                } else {
                    for entry in tool_lines {
                        output.push_str(&entry.render());
                        output.push('\n');
                    }
                }
            }
            ToolDisplay::None => {}
        }
        if !output.is_empty() {
            output.push('\n');
        }
    }
    output.push_str(text.trim_end());
    output
}

pub(crate) fn extract_mentions(content: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut in_fence = false;

    for line in content.split('\n') {
        if line.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let bytes = line.as_bytes();
        let mut index = 0;
        while index + 2 < bytes.len() {
            if bytes[index] == b'<' && bytes[index + 1] == b'@' {
                let (prefix_end, is_role) = if index + 2 < bytes.len() && bytes[index + 2] == b'&' {
                    (index + 3, true)
                } else if index + 2 < bytes.len() && bytes[index + 2] == b'!' {
                    (index + 3, false)
                } else {
                    (index + 2, false)
                };
                if prefix_end < bytes.len() && bytes[prefix_end].is_ascii_digit() {
                    if let Some(end) = line[prefix_end..].find('>') {
                        if line[prefix_end..prefix_end + end]
                            .chars()
                            .all(|character| character.is_ascii_digit())
                        {
                            let user_id = &line[prefix_end..prefix_end + end];
                            let normalized = if is_role {
                                format!("<@&{user_id}>")
                            } else {
                                format!("<@{user_id}>")
                            };
                            if !mentions.contains(&normalized) {
                                mentions.push(normalized);
                            }
                            index = prefix_end + end + 1;
                            continue;
                        }
                    }
                }
                index = prefix_end;
            } else {
                index += 1;
            }
        }
    }
    mentions
}

pub(crate) fn mention_footer_len(mentions: &[String]) -> usize {
    if mentions.is_empty() {
        0
    } else {
        1 + mentions.iter().map(String::len).sum::<usize>() + mentions.len().saturating_sub(1)
    }
}

pub(crate) fn propagate_mentions_to_chunks(
    chunks: Vec<String>,
    mentions: &[String],
    limit: usize,
) -> Vec<String> {
    if mentions.is_empty() || chunks.len() <= 1 {
        return chunks;
    }
    chunks
        .into_iter()
        .map(|chunk| {
            let missing: Vec<&String> = mentions
                .iter()
                .filter(|mention| !chunk.contains(mention.as_str()))
                .collect();
            if missing.is_empty() {
                chunk
            } else {
                let footer = format!(
                    "\n{}",
                    missing
                        .iter()
                        .map(|mention| mention.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if chunk.chars().count() + footer.chars().count() <= limit {
                    format!("{chunk}{footer}")
                } else {
                    chunk
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use uuid::Uuid;

    use crate::acp::protocol::JsonRpcMessage;
    use crate::config::{ReactionEmojis, ReactionTiming};

    #[derive(Default)]
    struct RecordingAdapterState {
        send_attempts: usize,
        sent: Vec<String>,
    }

    struct RecordingAdapter {
        streaming: bool,
        fail_sends: bool,
        state: StdMutex<RecordingAdapterState>,
    }

    impl RecordingAdapter {
        fn new(streaming: bool, fail_sends: bool) -> Self {
            Self {
                streaming,
                fail_sends,
                state: StdMutex::new(RecordingAdapterState::default()),
            }
        }

        fn sent(&self) -> Vec<String> {
            self.state.lock().unwrap().sent.clone()
        }

        fn send_attempts(&self) -> usize {
            self.state.lock().unwrap().send_attempts
        }
    }

    #[async_trait]
    impl ChatAdapter for RecordingAdapter {
        fn platform(&self) -> &'static str {
            "test"
        }

        fn message_limit(&self) -> usize {
            2_000
        }

        async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef> {
            let mut state = self.state.lock().unwrap();
            state.send_attempts += 1;
            if self.fail_sends {
                return Err(anyhow!("send failed"));
            }
            state.sent.push(content.to_string());
            Ok(MessageRef {
                channel: channel.clone(),
                message_id: format!("message-{}", state.send_attempts),
            })
        }

        async fn create_thread(
            &self,
            channel: &ChannelRef,
            _trigger_msg: &MessageRef,
            _title: &str,
        ) -> Result<ChannelRef> {
            Ok(channel.clone())
        }

        async fn add_reaction(&self, _msg: &MessageRef, _emoji: &str) -> Result<()> {
            Ok(())
        }

        async fn remove_reaction(&self, _msg: &MessageRef, _emoji: &str) -> Result<()> {
            Ok(())
        }

        fn use_streaming(&self, _other_bot_present: bool) -> bool {
            self.streaming
        }
    }

    struct FakeTurnConnection {
        reset: bool,
        reset_takes: usize,
        ticket: TurnTicket,
        notifications: Vec<JsonRpcMessage>,
        keep_notification_stream_open: bool,
        notification_sender: Option<mpsc::UnboundedSender<JsonRpcMessage>>,
        alive: bool,
        cancel_write_indeterminate: bool,
        prompt_starts: usize,
        start_error: Option<TurnStartError>,
        abandoned: Vec<TurnTicket>,
        completed: Vec<TurnTicket>,
        config_updates: Vec<Vec<ConfigOption>>,
    }

    impl FakeTurnConnection {
        fn new(notifications: Vec<JsonRpcMessage>) -> Self {
            Self {
                reset: false,
                reset_takes: 0,
                ticket: TurnTicket::new(
                    Uuid::parse_str("0f29119e-6f1b-4e03-979e-054d0c638c11").unwrap(),
                    42,
                ),
                notifications,
                keep_notification_stream_open: false,
                notification_sender: None,
                alive: true,
                cancel_write_indeterminate: false,
                prompt_starts: 0,
                start_error: None,
                abandoned: Vec::new(),
                completed: Vec::new(),
                config_updates: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl TurnConnection for FakeTurnConnection {
        fn take_session_reset(&mut self) -> bool {
            self.reset_takes += 1;
            std::mem::take(&mut self.reset)
        }

        async fn session_prompt(
            &mut self,
            _content_blocks: Vec<ContentBlock>,
        ) -> std::result::Result<
            (mpsc::UnboundedReceiver<JsonRpcMessage>, TurnTicket),
            TurnStartError,
        > {
            self.prompt_starts += 1;
            if let Some(error) = self.start_error.take() {
                return Err(error);
            }
            let (tx, rx) = mpsc::unbounded_channel();
            for notification in std::mem::take(&mut self.notifications) {
                tx.send(notification).unwrap();
            }
            if self.keep_notification_stream_open {
                self.notification_sender = Some(tx);
            }
            Ok((rx, self.ticket.clone()))
        }

        fn alive(&self) -> bool {
            self.alive
        }

        async fn cancel_write_is_indeterminate(&self, ticket: &TurnTicket) -> bool {
            self.cancel_write_indeterminate && *ticket == self.ticket
        }

        async fn abandon_request(&mut self, ticket: &TurnTicket) {
            self.abandoned.push(ticket.clone());
        }

        async fn prompt_done(&mut self, ticket: &TurnTicket) {
            self.completed.push(ticket.clone());
        }

        fn update_config_options(&mut self, options: Vec<ConfigOption>) {
            self.config_updates.push(options);
        }
    }

    fn channel() -> ChannelRef {
        ChannelRef {
            platform: "test".to_string(),
            channel_id: "channel-1".to_string(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn delivery_context(adapter: Arc<RecordingAdapter>) -> TurnDeliveryContext {
        let thread_channel = channel();
        let adapter_for_reactions: Arc<dyn ChatAdapter> = adapter.clone();
        let reactions = Arc::new(StatusReactionController::new(
            false,
            adapter_for_reactions.clone(),
            MessageRef {
                channel: thread_channel.clone(),
                message_id: "trigger-1".to_string(),
            },
            ReactionEmojis::default(),
            ReactionTiming::default(),
        ));
        TurnDeliveryContext {
            adapter: adapter_for_reactions,
            thread_channel,
            reactions,
            recipient: None,
            other_bot_present: false,
            table_mode: TableMode::Off,
            tool_display: ToolDisplay::None,
            narration_display: false,
            prompt_hard_timeout: Duration::from_secs(30),
            liveness_check_interval: Duration::from_secs(1),
        }
    }

    fn rpc(value: Value) -> JsonRpcMessage {
        serde_json::from_value(value).unwrap()
    }

    fn text_notification(text: &str) -> JsonRpcMessage {
        rpc(json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }
            }
        }))
    }

    fn tool_notification(
        session_update: &str,
        id: &str,
        title: &str,
        status: Option<&str>,
    ) -> JsonRpcMessage {
        let mut update = json!({
            "sessionUpdate": session_update,
            "toolCallId": id,
            "title": title,
        });
        if let Some(status) = status {
            update["status"] = json!(status);
        }
        rpc(json!({
            "method": "session/update",
            "params": {"update": update}
        }))
    }

    fn config_notification() -> JsonRpcMessage {
        rpc(json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": [{
                        "id": "model",
                        "name": "Model",
                        "type": "enum",
                        "currentValue": "test-model",
                        "options": [{"value": "test-model", "name": "Test model"}]
                    }]
                }
            }
        }))
    }

    fn final_result(request_id: u64, stop_reason: &str) -> JsonRpcMessage {
        rpc(json!({
            "id": request_id,
            "result": {
                "stopReason": stop_reason,
                "usage": {"inputTokens": 1, "outputTokens": 2, "totalTokens": 3}
            }
        }))
    }

    fn final_error(request_id: u64) -> JsonRpcMessage {
        rpc(json!({
            "id": request_id,
            "error": {"code": -32000, "message": "agent failed"}
        }))
    }

    async fn run_fake(
        conn: &mut FakeTurnConnection,
        context: TurnDeliveryContext,
    ) -> TurnCompletion {
        AcpTurnDriver::run_with_connection(
            conn,
            vec![ContentBlock::Text {
                text: "do the work".to_string(),
            }],
            context,
        )
        .await
        .unwrap()
    }

    fn assert_cleaned_once(conn: &FakeTurnConnection) {
        assert_eq!(conn.prompt_starts, 1);
        assert_eq!(conn.reset_takes, 1);
        assert_eq!(conn.completed, vec![conn.ticket.clone()]);
    }

    #[tokio::test]
    async fn success_delivers_authoritative_display_and_speech_and_cleans_up_once() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(vec![
            config_notification(),
            text_notification("working notes"),
            tool_notification("tool_call", "tool-1", "Edit file", None),
            tool_notification("tool_call_update", "tool-1", "Edit file", Some("completed")),
            text_notification("authoritative answer"),
            final_result(42, "end_turn"),
        ]);

        let completion = run_fake(&mut conn, delivery_context(adapter.clone())).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::Succeeded { .. }
        ));
        assert_eq!(
            completion.output,
            Some(TurnOutput {
                display_text: "authoritative answer".to_string(),
                speech_text: "authoritative answer".to_string(),
            })
        );
        assert_eq!(
            completion.eligible_speech_text(),
            Some("authoritative answer")
        );
        assert_eq!(completion.delivery, DeliveryOutcome::Delivered);
        assert_eq!(
            completion.legacy_dispatch,
            LegacyDispatchDisposition::Succeeded
        );
        assert_eq!(adapter.sent(), vec!["authoritative answer"]);
        assert!(conn.abandoned.is_empty());
        assert_eq!(conn.config_updates.len(), 1);
        assert_eq!(conn.config_updates[0][0].id, "model");
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn narration_display_does_not_leak_intertool_text_into_speech() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(vec![
            text_notification("working notes\n"),
            tool_notification("tool_call", "tool-1", "Inspect file", None),
            tool_notification(
                "tool_call_update",
                "tool-1",
                "Inspect file",
                Some("completed"),
            ),
            text_notification("authoritative answer"),
            final_result(42, "end_turn"),
        ]);
        let mut context = delivery_context(adapter);
        context.narration_display = true;

        let completion = run_fake(&mut conn, context).await;
        let output = completion.output.unwrap();

        assert!(output.display_text.contains("working notes"));
        assert!(output.display_text.contains("authoritative answer"));
        assert_eq!(output.speech_text, "authoritative answer");
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn detached_turn_exposes_ticket_before_waiting_for_completion() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let conn = Arc::new(Mutex::new(FakeTurnConnection::new(vec![
            text_notification("authoritative answer"),
            final_result(42, "end_turn"),
        ])));
        let mut running = AcpTurnDriver::spawn_with_connection(
            Arc::clone(&conn),
            vec![ContentBlock::Text {
                text: "do the work".to_string(),
            }],
            delivery_context(adapter),
        );

        let ticket = running.ticket().await.unwrap();
        let completion = running.wait().await.unwrap();

        assert_eq!(ticket, completion.ticket);
        let conn = conn.lock().await;
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn dropping_detached_receivers_does_not_abort_turn_cleanup() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let conn = Arc::new(Mutex::new(FakeTurnConnection::new(vec![
            text_notification("authoritative answer"),
            final_result(42, "end_turn"),
        ])));
        let running = AcpTurnDriver::spawn_with_connection(
            Arc::clone(&conn),
            vec![ContentBlock::Text {
                text: "do the work".to_string(),
            }],
            delivery_context(adapter),
        );

        drop(running);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(conn) = conn.try_lock() {
                    if conn.completed.len() == 1 {
                        assert_cleaned_once(&conn);
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached driver must clean up after receivers are dropped");
    }

    #[tokio::test]
    async fn indeterminate_prompt_write_is_typed_unknown_and_cleans_up_once() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(Vec::new());
        conn.start_error = Some(TurnStartError::WriteIndeterminate {
            ticket: conn.ticket.clone(),
            error: "stdin write timeout".to_string(),
        });

        let completion = run_fake(&mut conn, delivery_context(adapter.clone())).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::PromptWriteIndeterminate { .. },
                ..
            }
        ));
        assert_eq!(completion.delivery, DeliveryOutcome::NotAttempted);
        assert_eq!(
            completion.legacy_dispatch,
            LegacyDispatchDisposition::Failed {
                error: "stdin write timeout".to_string()
            }
        );
        assert_eq!(adapter.send_attempts(), 0);
        assert!(conn.abandoned.is_empty());
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn proven_prewrite_failure_remains_an_outer_error() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(Vec::new());
        conn.start_error = Some(TurnStartError::NotStarted {
            error: "no session".to_string(),
        });

        let result = AcpTurnDriver::run_with_connection(
            &mut conn,
            vec![ContentBlock::Text {
                text: "do the work".to_string(),
            }],
            delivery_context(adapter),
        )
        .await;

        assert_eq!(result.unwrap_err().to_string(), "no session");
        assert_eq!(conn.prompt_starts, 1);
        assert_eq!(conn.reset_takes, 1);
        assert!(conn.completed.is_empty());
        assert!(conn.abandoned.is_empty());
    }

    #[tokio::test]
    async fn partial_text_then_acp_error_is_unknown_but_legacy_delivery_succeeds() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn =
            FakeTurnConnection::new(vec![text_notification("partial answer"), final_error(42)]);

        let completion = run_fake(&mut conn, delivery_context(adapter.clone())).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::AcpError { .. },
                ..
            }
        ));
        assert_eq!(completion.delivery, DeliveryOutcome::Delivered);
        assert_eq!(
            completion.legacy_dispatch,
            LegacyDispatchDisposition::Succeeded
        );
        assert!(completion
            .output
            .as_ref()
            .unwrap()
            .display_text
            .contains("partial answer"));
        assert_eq!(adapter.sent().len(), 1);
        assert!(conn.abandoned.is_empty());
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn partial_text_then_eof_is_unknown_and_abandons_and_cleans_up_once() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(vec![text_notification("partial answer")]);

        let completion = run_fake(&mut conn, delivery_context(adapter)).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::AgentExited,
                ..
            }
        ));
        assert_eq!(conn.abandoned, vec![conn.ticket.clone()]);
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn indeterminate_cancel_write_is_distinct_and_cleans_up_once() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(vec![text_notification("partial answer")]);
        conn.cancel_write_indeterminate = true;

        let completion = run_fake(&mut conn, delivery_context(adapter)).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::CancelWriteIndeterminate,
                ..
            }
        ));
        assert_eq!(conn.abandoned, vec![conn.ticket.clone()]);
        assert_cleaned_once(&conn);
    }

    #[tokio::test(start_paused = true)]
    async fn hard_timeout_abandons_the_exact_ticket_once() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(Vec::new());
        conn.keep_notification_stream_open = true;
        let mut context = delivery_context(adapter);
        context.prompt_hard_timeout = Duration::from_secs(1);
        context.liveness_check_interval = Duration::from_millis(250);

        let completion = run_fake(&mut conn, context).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::HardTimeout { seconds: 1 },
                ..
            }
        ));
        assert_eq!(conn.abandoned, vec![conn.ticket.clone()]);
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn matching_cancelled_stop_reason_is_confirmed_cancellation() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(vec![
            text_notification("stopping"),
            final_result(42, "cancelled"),
        ]);

        let completion = run_fake(&mut conn, delivery_context(adapter)).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::Cancelled {
                turn_result: TurnResult {
                    stop_reason: Some(ref reason),
                    ..
                },
                ..
            } if reason == "cancelled"
        ));
        assert!(conn.abandoned.is_empty());
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn final_delivery_failure_preserves_execution_success_and_legacy_failure() {
        let adapter = Arc::new(RecordingAdapter::new(false, true));
        let mut conn = FakeTurnConnection::new(vec![
            text_notification("completed work"),
            final_result(42, "end_turn"),
        ]);

        let completion = run_fake(&mut conn, delivery_context(adapter.clone())).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::Succeeded { .. }
        ));
        assert_eq!(
            completion.delivery,
            DeliveryOutcome::Failed {
                error: DELIVERY_FAILURE.to_string(),
                partially_delivered: false,
            }
        );
        assert_eq!(
            completion.legacy_dispatch,
            LegacyDispatchDisposition::Failed {
                error: DELIVERY_FAILURE.to_string(),
            }
        );
        assert_eq!(adapter.send_attempts(), 1);
        assert!(conn.abandoned.is_empty());
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn streaming_placeholder_failure_is_unknown_and_abandons_and_cleans_up_once() {
        let adapter = Arc::new(RecordingAdapter::new(true, true));
        let mut conn = FakeTurnConnection::new(Vec::new());

        let completion = run_fake(&mut conn, delivery_context(adapter.clone())).await;

        assert_eq!(
            completion.execution,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::DeliverySetupFailure {
                    error: DELIVERY_SETUP_FAILURE.to_string(),
                },
                observed_tool_calls: Vec::new(),
            }
        );
        assert_eq!(completion.output, None);
        assert_eq!(
            completion.delivery,
            DeliveryOutcome::Failed {
                error: DELIVERY_SETUP_FAILURE.to_string(),
                partially_delivered: false,
            }
        );
        assert_eq!(
            completion.legacy_dispatch,
            LegacyDispatchDisposition::Failed {
                error: "send failed".to_string(),
            }
        );
        assert_eq!(adapter.send_attempts(), 1);
        assert_eq!(conn.abandoned, vec![conn.ticket.clone()]);
        assert_cleaned_once(&conn);
    }

    #[tokio::test]
    async fn stale_id_is_ignored_before_the_matching_final_response() {
        let adapter = Arc::new(RecordingAdapter::new(false, false));
        let mut conn = FakeTurnConnection::new(vec![
            final_error(999),
            text_notification("matching answer"),
            final_result(42, "end_turn"),
        ]);

        let completion = run_fake(&mut conn, delivery_context(adapter.clone())).await;

        assert!(matches!(
            completion.execution,
            ExecutionOutcome::Succeeded { .. }
        ));
        assert_eq!(
            completion.output.as_ref().unwrap().display_text,
            "matching answer"
        );
        assert_eq!(adapter.sent(), vec!["matching answer"]);
        assert!(conn.abandoned.is_empty());
        assert_cleaned_once(&conn);
    }

    #[test]
    fn successful_result_without_final_answer_is_unknown() {
        let outcome = classify_execution(
            TerminalObservation::Succeeded(TurnResult {
                stop_reason: Some("end_turn".to_string()),
                ..TurnResult::default()
            }),
            false,
            Vec::new(),
        );
        assert!(matches!(
            outcome,
            ExecutionOutcome::OutcomeUnknown {
                reason: UnknownReason::MissingOutput,
                ..
            }
        ));
    }

    #[test]
    fn confirmed_cancel_preserves_tool_evidence() {
        let observed = vec![ObservedToolCall {
            id: "tool-1".to_string(),
            title: "Edit file".to_string(),
            status: ObservedToolCallStatus::Started,
        }];
        let turn_result = TurnResult {
            stop_reason: Some("cancelled".to_string()),
            ..TurnResult::default()
        };
        let outcome = classify_execution(
            TerminalObservation::Cancelled(turn_result.clone()),
            false,
            observed.clone(),
        );
        assert_eq!(
            outcome,
            ExecutionOutcome::Cancelled {
                turn_result,
                observed_tool_calls: observed
            }
        );
    }

    #[test]
    fn output_directive_is_not_speakable_text() {
        let (_, body) = split_delivery("[[reply_to:42]]\nfinal answer", 0, false);
        assert_eq!(body, "final answer");
    }

    #[test]
    fn tool_evidence_survives_empty_initial_title_and_tracks_terminal_update() {
        let mut observed = Vec::new();
        observe_tool_call(&mut observed, "tool-1", "", ObservedToolCallStatus::Started);
        observe_tool_call(
            &mut observed,
            "tool-1",
            "Edit\nfile",
            ObservedToolCallStatus::Completed,
        );

        assert_eq!(
            observed,
            vec![ObservedToolCall {
                id: "tool-1".to_string(),
                title: "Edit ; file".to_string(),
                status: ObservedToolCallStatus::Completed,
            }]
        );
    }
}
