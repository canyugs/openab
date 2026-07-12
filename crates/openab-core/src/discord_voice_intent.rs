//! Session-bound voice intent delegation for Discord.
//!
//! This module deliberately does not execute ACP actions. It turns a finalized
//! transcript from the operator bound to a voice session into a proposed
//! Discord-bot delegation or a request for the voice agent itself, waits for one
//! text confirmation, and then emits the selected action. Discord I/O is kept
//! behind [`VoiceIntentMessenger`] so delegate dispatch can make its
//! exactly-once transition before any network request is awaited.

use crate::config::DiscordVoiceIntentConfig;
use crate::discord_voice_runtime::VoiceSessionToken;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

const DISCORD_MESSAGE_LIMIT: usize = 2_000;
const MAX_PROCESSED_TRANSCRIPT_KEYS: usize = 1_024;
const MAX_CONFIRMATION_RETRY_ATTEMPTS: usize = 5;
const MAX_STALE_CONFIRMATION_RECOVERY_ATTEMPTS: usize = 2;
const CONFIRMATION_RETRY_BASE: Duration = Duration::from_secs(1);
const CONFIRMATION_RETRY_MAX: Duration = Duration::from_secs(8);

/// Stable identity of one finalized STT segment within a voice session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TranscriptKey {
    pub speaker_id: u64,
    pub start_frame: u64,
    pub end_frame: u64,
}

/// The final transcript information needed by the intent broker.
#[derive(Debug, Clone)]
pub struct FinalTranscriptEvent {
    pub token: VoiceSessionToken,
    pub control_channel_id: u64,
    pub key: TranscriptKey,
    pub text: String,
}

/// Narrow Discord output boundary used by the broker.
///
/// `nonce` may be present for any stateful broker message. Production
/// implementations must pass a provided nonce to Discord with enforcement
/// enabled so retrying an ambiguously completed request recovers the same
/// message instead of creating a duplicate.
#[async_trait]
pub trait VoiceIntentMessenger: Send + Sync {
    async fn send_message(
        &self,
        channel_id: u64,
        content: &str,
        nonce: Option<&str>,
    ) -> Result<String>;

    /// Best-effort cleanup for a prompt that became stale while its HTTP post
    /// was in flight. Valid audit-trail messages are intentionally retained.
    async fn delete_message(&self, channel_id: u64, message_id: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceIntentTranscriptOutcome {
    /// The event was disabled, stale, from another speaker/channel, or was not
    /// an unambiguous command for a supported destination.
    Ignored,
    /// This exact session already has an intent waiting for text confirmation.
    AwaitingConfirmation,
    /// A new proposal was stored and its idempotent confirmation post started.
    Proposed,
}

/// Confirmed work that should execute through the voice agent's own ACP path.
///
/// The broker keeps the intent in `Dispatching` until the caller completes the
/// handoff. A definitely-not-enqueued failure may reopen confirmation; any
/// repeated handoff keeps the same `intent_id` and `revision` for deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIntentExecution {
    pub intent_id: Uuid,
    pub revision: u64,
    pub session: VoiceSessionToken,
    pub control_channel_id: u64,
    pub operator_user_id: u64,
    pub operator_display_name: String,
    pub task: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceIntentTextOutcome {
    /// This text is unrelated to a pending confirmation and should continue
    /// through the ordinary Discord message pipeline.
    NotApplicable,
    /// A second confirmation arrived while the first dispatch was in flight.
    AlreadyProcessing,
    /// The target message may have been accepted and is being recovered with
    /// its enforced nonce after an ambiguous transport result.
    Dispatching,
    Dispatched,
    /// Execute this confirmed task through the voice agent's own ACP runtime.
    ExecuteLocal(LocalIntentExecution),
    Cancelled,
    Corrected,
    /// The message used correction syntax but did not contain a usable task.
    CorrectionRejected,
}

impl VoiceIntentTextOutcome {
    /// Whether the Discord handler must stop normal message processing.
    pub fn consumed(&self) -> bool {
        !matches!(self, Self::NotApplicable)
    }
}

#[derive(Debug, Clone)]
struct IntentTarget {
    canonical: String,
    discord_user_id: u64,
    aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IntentProposal {
    destination: IntentDestination,
    task: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntentDestination {
    Local,
    Delegate { target_index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetMatch {
    None,
    Delegate {
        target_index: usize,
        start: usize,
        end: usize,
    },
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingPhase {
    PostingConfirmation,
    WaitingConfirmation,
    Dispatching,
}

#[derive(Debug, Clone)]
struct PendingIntent {
    id: Uuid,
    revision: u64,
    waiting_epoch: u64,
    proposal: IntentProposal,
    phase: PendingPhase,
    confirmation_message_id: Option<String>,
}

#[derive(Debug)]
struct BoundSession {
    token: VoiceSessionToken,
    control_channel_id: u64,
    operator_user_id: u64,
    operator_display_name: String,
    next_revision: u64,
    next_waiting_epoch: u64,
    pending: Option<PendingIntent>,
    processed_keys: HashSet<TranscriptKey>,
    processed_key_order: VecDeque<TranscriptKey>,
}

#[derive(Debug, Default)]
struct BrokerState {
    sessions: HashMap<u64, BoundSession>,
}

/// Per-process broker for the opt-in Discord voice intent flow.
pub struct DiscordVoiceIntentBroker {
    enabled: bool,
    default_to_local: bool,
    confirmation_timeout: Duration,
    targets: Vec<IntentTarget>,
    messenger: Arc<dyn VoiceIntentMessenger>,
    state: Mutex<BrokerState>,
}

impl DiscordVoiceIntentBroker {
    pub fn new(
        config: DiscordVoiceIntentConfig,
        messenger: Arc<dyn VoiceIntentMessenger>,
    ) -> Result<Arc<Self>> {
        // The parent voice configuration is validated by the caller. Passing
        // `true` here still gives this standalone subsystem strict validation.
        config.validate(true)?;
        let default_to_local = config.default_to_local;

        let mut targets = Vec::with_capacity(config.targets.len());
        for (canonical, configured) in config.targets {
            let discord_user_id = configured
                .discord_user_id
                .trim()
                .parse::<u64>()
                .with_context(|| {
                    format!("invalid Discord user ID for discord.voice.intent.targets.{canonical}")
                })?;
            let mut aliases = Vec::with_capacity(configured.aliases.len() + 1);
            aliases.push(normalize_for_matching(&canonical));
            aliases.extend(
                configured
                    .aliases
                    .iter()
                    .map(|alias| normalize_for_matching(alias)),
            );
            aliases.sort();
            aliases.dedup();
            targets.push(IntentTarget {
                canonical,
                discord_user_id,
                aliases,
            });
        }

        Ok(Arc::new(Self {
            enabled: config.enabled,
            default_to_local,
            confirmation_timeout: Duration::from_secs(config.confirmation_timeout_seconds.max(1)),
            targets,
            messenger,
            state: Mutex::new(BrokerState::default()),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Mark a local ACP handoff as accepted by the in-process execution queue.
    pub fn complete_local_execution(&self, request: &LocalIntentExecution) -> bool {
        self.remove_pending_if_current(
            request.session,
            request.intent_id,
            request.revision,
            PendingPhase::Dispatching,
        )
    }

    /// Re-arm text confirmation after a local handoff definitely was not
    /// enqueued. Ambiguous submit results must not call this method.
    pub fn reopen_local_execution(self: &Arc<Self>, request: &LocalIntentExecution) -> bool {
        let waiting_epoch = {
            let mut state = self.lock_state();
            let Some(bound) = state.sessions.get_mut(&request.session.guild_id()) else {
                return false;
            };
            if bound.token != request.session
                || !bound.pending.as_ref().is_some_and(|pending| {
                    pending.id == request.intent_id
                        && pending.revision == request.revision
                        && pending.phase == PendingPhase::Dispatching
                        && pending.proposal.destination == IntentDestination::Local
                })
            {
                return false;
            }
            let waiting_epoch = bound.next_waiting_epoch;
            bound.next_waiting_epoch = bound.next_waiting_epoch.saturating_add(1);
            let pending = bound.pending.as_mut().expect("pending was checked above");
            pending.phase = PendingPhase::WaitingConfirmation;
            pending.waiting_epoch = waiting_epoch;
            waiting_epoch
        };
        self.schedule_timeout(
            request.session,
            request.intent_id,
            request.revision,
            waiting_epoch,
        );
        true
    }

    /// Bind the operator and Discord control channel to one exact voice session.
    ///
    /// A newer session for the same guild atomically replaces the old binding,
    /// including any pending proposal. Late transcripts, confirmations, and
    /// timeout tasks from the replaced session can no longer match.
    pub fn bind_session(
        &self,
        token: VoiceSessionToken,
        control_channel_id: u64,
        operator_user_id: u64,
        operator_display_name: impl Into<String>,
    ) -> bool {
        if !self.enabled {
            return false;
        }

        let display_name = sanitize_display_name(operator_display_name.into(), operator_user_id);
        let stale_prompt = {
            let replaced = self.lock_state().sessions.insert(
                token.guild_id(),
                BoundSession {
                    token,
                    control_channel_id,
                    operator_user_id,
                    operator_display_name: display_name,
                    next_revision: 1,
                    next_waiting_epoch: 1,
                    pending: None,
                    processed_keys: HashSet::new(),
                    processed_key_order: VecDeque::new(),
                },
            );
            replaced.and_then(visible_prompt_to_delete)
        };
        if let Some((channel_id, message_id)) = stale_prompt {
            self.delete_message_in_background(channel_id, message_id);
        }
        true
    }

    /// Remove a binding only when its opaque session token still matches.
    pub fn abandon_session(&self, token: VoiceSessionToken) -> bool {
        if !self.enabled {
            return false;
        }

        let removed = {
            let mut state = self.lock_state();
            if state
                .sessions
                .get(&token.guild_id())
                .is_some_and(|bound| bound.token == token)
            {
                state.sessions.remove(&token.guild_id())
            } else {
                None
            }
        };
        if let Some(bound) = removed {
            if let Some((channel_id, message_id)) = visible_prompt_to_delete(bound) {
                self.delete_message_in_background(channel_id, message_id);
            }
            true
        } else {
            false
        }
    }

    /// Propose an intent from one accepted, finalized STT segment.
    pub async fn handle_final_transcript(
        self: &Arc<Self>,
        event: FinalTranscriptEvent,
    ) -> Result<VoiceIntentTranscriptOutcome> {
        if !self.enabled {
            return Ok(VoiceIntentTranscriptOutcome::Ignored);
        }

        let Some(raw_proposal) = self.resolve_proposal(&event.text) else {
            return Ok(VoiceIntentTranscriptOutcome::Ignored);
        };

        let confirmation = {
            let mut state = self.lock_state();
            let Some(bound) = state.sessions.get_mut(&event.token.guild_id()) else {
                return Ok(VoiceIntentTranscriptOutcome::Ignored);
            };
            if bound.token != event.token
                || bound.control_channel_id != event.control_channel_id
                || bound.operator_user_id != event.key.speaker_id
            {
                return Ok(VoiceIntentTranscriptOutcome::Ignored);
            }
            let Some(proposal) = self.canonicalize_proposal(bound, raw_proposal) else {
                return Ok(VoiceIntentTranscriptOutcome::Ignored);
            };
            // Record every valid, unambiguous operator proposal before looking
            // at the pending slot. A delayed replay therefore cannot become a
            // fresh command after the current intent completes.
            if !record_transcript_key(bound, event.key) {
                return Ok(VoiceIntentTranscriptOutcome::Ignored);
            }
            if bound.pending.is_some() {
                return Ok(VoiceIntentTranscriptOutcome::AwaitingConfirmation);
            }

            let intent = PendingIntent {
                id: Uuid::new_v4(),
                revision: bound.next_revision,
                waiting_epoch: 0,
                proposal,
                phase: PendingPhase::PostingConfirmation,
                confirmation_message_id: None,
            };
            bound.next_revision = bound.next_revision.saturating_add(1);
            let confirmation = PendingMessage::from_bound(bound, intent.clone());
            bound.pending = Some(intent.clone());
            confirmation
        };

        self.post_confirmation_or_schedule_retry(confirmation).await;
        Ok(VoiceIntentTranscriptOutcome::Proposed)
    }

    /// Consume an exact text confirmation, cancellation, or correction.
    ///
    /// Errors here occur only after a message was recognized as part of the
    /// confirmation flow. Callers should therefore consume the Discord message
    /// even when this method returns `Err`, rather than forwarding a bare "yes"
    /// into ACP.
    pub async fn handle_text_message(
        self: &Arc<Self>,
        guild_id: u64,
        channel_id: u64,
        user_id: u64,
        content: &str,
    ) -> Result<VoiceIntentTextOutcome> {
        if !self.enabled {
            return Ok(VoiceIntentTextOutcome::NotApplicable);
        }
        let confirmation = classify_confirmation(content);
        if matches!(confirmation, Confirmation::Unrelated) {
            return Ok(VoiceIntentTextOutcome::NotApplicable);
        }

        let effect = {
            let mut state = self.lock_state();
            let Some(bound) = state.sessions.get_mut(&guild_id) else {
                return Ok(VoiceIntentTextOutcome::NotApplicable);
            };
            if bound.control_channel_id != channel_id || bound.operator_user_id != user_id {
                return Ok(VoiceIntentTextOutcome::NotApplicable);
            }
            let Some(current) = bound.pending.clone() else {
                return Ok(VoiceIntentTextOutcome::NotApplicable);
            };
            if current.phase != PendingPhase::WaitingConfirmation {
                return Ok(VoiceIntentTextOutcome::AlreadyProcessing);
            }

            match confirmation {
                Confirmation::Affirmative => match current.proposal.destination {
                    IntentDestination::Delegate { .. } => {
                        let updated = {
                            let pending =
                                bound.pending.as_mut().expect("pending was checked above");
                            pending.phase = PendingPhase::Dispatching;
                            pending.clone()
                        };
                        TextEffect::Dispatch(PendingMessage::from_bound(bound, updated))
                    }
                    IntentDestination::Local => {
                        let updated = {
                            let pending =
                                bound.pending.as_mut().expect("pending was checked above");
                            pending.phase = PendingPhase::Dispatching;
                            pending.clone()
                        };
                        let message = PendingMessage::from_bound(bound, updated);
                        TextEffect::ExecuteLocal(LocalIntentExecution::from_pending(&message))
                    }
                },
                Confirmation::Negative => {
                    bound.pending = None;
                    TextEffect::Cancel(PendingMessage::from_bound(bound, current))
                }
                Confirmation::Correction(correction) => {
                    if let Some(proposal) = self
                        .resolve_correction(&current.proposal, correction)
                        .and_then(|proposal| self.canonicalize_proposal(bound, proposal))
                    {
                        let revised_intent = PendingIntent {
                            id: Uuid::new_v4(),
                            revision: bound.next_revision,
                            waiting_epoch: 0,
                            proposal,
                            phase: PendingPhase::PostingConfirmation,
                            confirmation_message_id: None,
                        };
                        bound.next_revision = bound.next_revision.saturating_add(1);
                        let revised = PendingMessage::from_bound(bound, revised_intent.clone());
                        bound.pending = Some(revised_intent);
                        TextEffect::Correct(revised)
                    } else {
                        TextEffect::RejectCorrection(PendingMessage::from_bound(bound, current))
                    }
                }
                Confirmation::Unrelated => unreachable!("handled before locking broker state"),
            }
        };

        self.apply_text_effect(effect).await
    }

    async fn apply_text_effect(
        self: &Arc<Self>,
        effect: TextEffect,
    ) -> Result<VoiceIntentTextOutcome> {
        match effect {
            TextEffect::Dispatch(message) => {
                let IntentDestination::Delegate { target_index } =
                    message.intent.proposal.destination
                else {
                    unreachable!("only delegate intents enter Discord dispatch")
                };
                let target = &self.targets[target_index];
                let content = dispatch_message(
                    target.discord_user_id,
                    message.operator_user_id,
                    &message.operator_display_name,
                    &message.intent.proposal.task,
                );
                let nonce = dispatch_nonce(message.intent.id);
                if let Err(error) = self
                    .messenger
                    .send_message(message.control_channel_id, &content, Some(&nonce))
                    .await
                {
                    warn!(
                        error = %error,
                        intent_id = %message.intent.id,
                        "Discord voice dispatch was ambiguous; scheduling idempotent recovery"
                    );
                    self.schedule_dispatch_retry(message, content, nonce);
                    return Ok(VoiceIntentTextOutcome::Dispatching);
                }
                self.remove_pending_if_current(
                    message.token,
                    message.intent.id,
                    message.intent.revision,
                    PendingPhase::Dispatching,
                );
                Ok(VoiceIntentTextOutcome::Dispatched)
            }
            TextEffect::ExecuteLocal(request) => Ok(VoiceIntentTextOutcome::ExecuteLocal(request)),
            TextEffect::Cancel(message) => {
                let content = bounded_discord_message(format!(
                    "<@{}> 已取消這次語音任務。",
                    message.operator_user_id
                ));
                self.messenger
                    .send_message(message.control_channel_id, &content, None)
                    .await
                    .context("failed to acknowledge cancelled Discord voice intent")?;
                Ok(VoiceIntentTextOutcome::Cancelled)
            }
            TextEffect::Correct(revised) => {
                self.post_confirmation_or_schedule_retry(revised).await;
                Ok(VoiceIntentTextOutcome::Corrected)
            }
            TextEffect::RejectCorrection(message) => {
                let content = bounded_discord_message(format!(
                    "<@{}> 我沒有理解這個修正；請用「更正：請 Sam review PR #123」委派，或用「更正：幫我 review PR #123」改由我處理。",
                    message.operator_user_id
                ));
                self.messenger
                    .send_message(message.control_channel_id, &content, None)
                    .await
                    .context("failed to send Discord voice correction guidance")?;
                Ok(VoiceIntentTextOutcome::CorrectionRejected)
            }
        }
    }

    fn resolve_proposal(&self, text: &str) -> Option<IntentProposal> {
        let normalized = NormalizedText::new(text);
        match self.find_target_match(&normalized.value) {
            TargetMatch::Delegate {
                target_index,
                start,
                end,
            } => {
                if !command_prefix_allowed(&normalized.value[..start]) {
                    return if self.default_to_local {
                        resolve_local_task(text).map(|task| IntentProposal {
                            destination: IntentDestination::Local,
                            task,
                        })
                    } else {
                        None
                    };
                }
                let original_end = normalized.original_index(end)?;
                let task = clean_task(&text[original_end..])?;
                Some(IntentProposal {
                    destination: IntentDestination::Delegate { target_index },
                    task,
                })
            }
            TargetMatch::Ambiguous => None,
            TargetMatch::None if self.default_to_local => {
                resolve_local_task(text).map(|task| IntentProposal {
                    destination: IntentDestination::Local,
                    task,
                })
            }
            TargetMatch::None => None,
        }
    }

    fn find_target_match(&self, normalized: &str) -> TargetMatch {
        let mut distinct_target: Option<usize> = None;
        let mut selected_match: Option<(usize, usize, usize)> = None;

        for (target_index, target) in self.targets.iter().enumerate() {
            let mut target_match: Option<(usize, usize)> = None;
            for alias in &target.aliases {
                for (start, end) in alias_matches(normalized, alias) {
                    if target_match.is_none_or(|(best_start, best_end)| {
                        start < best_start
                            || (start == best_start && end - start > best_end - best_start)
                    }) {
                        target_match = Some((start, end));
                    }
                }
            }

            if let Some((start, end)) = target_match {
                if distinct_target.is_some_and(|existing| existing != target_index) {
                    return TargetMatch::Ambiguous;
                }
                distinct_target = Some(target_index);
                if selected_match.is_none_or(|(_, best_start, best_end)| {
                    start < best_start
                        || (start == best_start && end - start > best_end - best_start)
                }) {
                    selected_match = Some((target_index, start, end));
                }
            }
        }

        if let Some((target_index, start, end)) = selected_match {
            TargetMatch::Delegate {
                target_index,
                start,
                end,
            }
        } else {
            TargetMatch::None
        }
    }

    fn resolve_correction(
        &self,
        current: &IntentProposal,
        correction: &str,
    ) -> Option<IntentProposal> {
        if let Some(proposal) = self.resolve_proposal(correction) {
            return Some(match proposal.destination {
                IntentDestination::Local if !has_explicit_local_prefix(correction) => {
                    IntentProposal {
                        destination: current.destination,
                        task: proposal.task,
                    }
                }
                _ => proposal,
            });
        }
        if self.text_mentions_any_target(correction) {
            // A target was named but the command was ambiguous or malformed;
            // do not accidentally treat its name as part of the old target's task.
            return None;
        }
        clean_task(correction).map(|task| IntentProposal {
            destination: current.destination,
            task,
        })
    }

    fn text_mentions_any_target(&self, text: &str) -> bool {
        let normalized = normalize_for_matching(text);
        self.targets.iter().any(|target| {
            target
                .aliases
                .iter()
                .any(|alias| alias_matches(&normalized, alias).next().is_some())
        })
    }

    fn canonicalize_proposal(
        &self,
        bound: &BoundSession,
        mut proposal: IntentProposal,
    ) -> Option<IntentProposal> {
        let confirmation_overhead = self
            .confirmation_message_for(bound.operator_user_id, &proposal, "")?
            .chars()
            .count();
        let dispatch_overhead = match proposal.destination {
            IntentDestination::Delegate { target_index } => {
                let target = self.targets.get(target_index)?;
                dispatch_message(
                    target.discord_user_id,
                    bound.operator_user_id,
                    &bound.operator_display_name,
                    "",
                )
                .chars()
                .count()
            }
            IntentDestination::Local => 0,
        };
        let maximum_task_chars =
            DISCORD_MESSAGE_LIMIT.saturating_sub(confirmation_overhead.max(dispatch_overhead));
        // A pathological configured name must not turn the task into only an
        // ellipsis or make the two rendered messages truncate differently.
        if maximum_task_chars < 16 {
            return None;
        }
        proposal.task = canonicalize_task(&proposal.task, maximum_task_chars);
        Some(proposal)
    }

    fn confirmation_message_for(
        &self,
        operator_user_id: u64,
        proposal: &IntentProposal,
        task: &str,
    ) -> Option<String> {
        match proposal.destination {
            IntentDestination::Delegate { target_index } => {
                let target = self.targets.get(target_index)?;
                Some(delegate_confirmation_message(
                    operator_user_id,
                    &target.canonical,
                    task,
                ))
            }
            IntentDestination::Local => Some(local_confirmation_message(operator_user_id, task)),
        }
    }

    async fn post_confirmation_or_schedule_retry(self: &Arc<Self>, message: PendingMessage) {
        let Some(content) = self.confirmation_message_for(
            message.operator_user_id,
            &message.intent.proposal,
            &message.intent.proposal.task,
        ) else {
            warn!(
                intent_id = %message.intent.id,
                "Discord voice intent target disappeared before confirmation"
            );
            self.remove_pending_if_current(
                message.token,
                message.intent.id,
                message.intent.revision,
                PendingPhase::PostingConfirmation,
            );
            return;
        };
        let nonce = confirmation_nonce(message.intent.id);
        match self
            .messenger
            .send_message(message.control_channel_id, &content, Some(&nonce))
            .await
        {
            Ok(message_id) => {
                self.finish_confirmation_post(message, message_id).await;
            }
            Err(error) => {
                // Discord may have accepted the nonce even though the response
                // was lost. Never roll state backward based on an
                // undifferentiated transport error; one retry owner recovers the
                // same message ID with the enforced nonce.
                warn!(
                    error = %error,
                    intent_id = %message.intent.id,
                    "Discord voice confirmation post was ambiguous; scheduling idempotent retry"
                );
                self.schedule_confirmation_retry(message, content, nonce);
            }
        }
    }

    fn schedule_confirmation_retry(
        self: &Arc<Self>,
        message: PendingMessage,
        content: String,
        nonce: String,
    ) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut stale_recovery_attempts = 0;
            for attempt in 0..MAX_CONFIRMATION_RETRY_ATTEMPTS {
                tokio::time::sleep(confirmation_retry_delay(attempt)).await;
                let Some(broker) = weak.upgrade() else {
                    return;
                };
                if !broker.is_current_phase(&message, PendingPhase::PostingConfirmation) {
                    if stale_recovery_attempts >= MAX_STALE_CONFIRMATION_RECOVERY_ATTEMPTS {
                        return;
                    }
                    stale_recovery_attempts += 1;
                }
                match broker
                    .messenger
                    .send_message(message.control_channel_id, &content, Some(&nonce))
                    .await
                {
                    Ok(message_id) => {
                        broker
                            .finish_confirmation_post(message.clone(), message_id)
                            .await;
                        return;
                    }
                    Err(error) => {
                        // Even if the ticket became stale, an earlier ambiguous
                        // attempt may have created a prompt. Keep recovering the
                        // nonce at a fixed bounded cadence so it can be deleted.
                        warn!(
                            error = %error,
                            intent_id = %message.intent.id,
                            attempt = attempt + 1,
                            "Discord voice confirmation retry remains ambiguous"
                        );
                    }
                }
            }
            let Some(broker) = weak.upgrade() else {
                return;
            };
            if broker.remove_pending_if_current(
                message.token,
                message.intent.id,
                message.intent.revision,
                PendingPhase::PostingConfirmation,
            ) {
                warn!(
                    intent_id = %message.intent.id,
                    attempts = MAX_CONFIRMATION_RETRY_ATTEMPTS,
                    "Discord voice confirmation retries exhausted; cleared pending intent"
                );
            }
        });
    }

    fn schedule_dispatch_retry(
        self: &Arc<Self>,
        message: PendingMessage,
        content: String,
        nonce: String,
    ) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut stale_recovery_attempts = 0;
            for attempt in 0..MAX_CONFIRMATION_RETRY_ATTEMPTS {
                tokio::time::sleep(confirmation_retry_delay(attempt)).await;
                let Some(broker) = weak.upgrade() else {
                    return;
                };
                if !broker.is_current_phase(&message, PendingPhase::Dispatching) {
                    if stale_recovery_attempts >= MAX_STALE_CONFIRMATION_RECOVERY_ATTEMPTS {
                        return;
                    }
                    stale_recovery_attempts += 1;
                }
                match broker
                    .messenger
                    .send_message(message.control_channel_id, &content, Some(&nonce))
                    .await
                {
                    Ok(_) => {
                        broker.remove_pending_if_current(
                            message.token,
                            message.intent.id,
                            message.intent.revision,
                            PendingPhase::Dispatching,
                        );
                        return;
                    }
                    Err(error) => {
                        warn!(
                            error = %error,
                            intent_id = %message.intent.id,
                            attempt = attempt + 1,
                            "Discord voice dispatch recovery remains ambiguous"
                        );
                    }
                }
            }
            let Some(broker) = weak.upgrade() else {
                return;
            };
            if let Some(waiting_epoch) = broker.restore_waiting_if_current(&message) {
                broker.schedule_timeout(
                    message.token,
                    message.intent.id,
                    message.intent.revision,
                    waiting_epoch,
                );
                warn!(
                    intent_id = %message.intent.id,
                    attempts = MAX_CONFIRMATION_RETRY_ATTEMPTS,
                    "Discord voice dispatch recovery exhausted; reopened confirmation"
                );
            }
        });
    }

    fn is_current_phase(&self, message: &PendingMessage, phase: PendingPhase) -> bool {
        self.lock_state()
            .sessions
            .get(&message.token.guild_id())
            .is_some_and(|bound| {
                bound.token == message.token
                    && bound.pending.as_ref().is_some_and(|pending| {
                        pending.id == message.intent.id
                            && pending.revision == message.intent.revision
                            && pending.phase == phase
                    })
            })
    }

    async fn finish_confirmation_post(
        self: &Arc<Self>,
        message: PendingMessage,
        message_id: String,
    ) {
        if let Some(waiting_epoch) =
            self.enter_waiting_after_confirmation_post(&message, message_id.clone())
        {
            self.schedule_timeout(
                message.token,
                message.intent.id,
                message.intent.revision,
                waiting_epoch,
            );
        } else {
            // The HTTP request completed after stop/rebind/correction. The
            // prompt is not part of a valid audit trail and must not remain as
            // an actionable-looking orphan.
            self.delete_message_best_effort(message.control_channel_id, &message_id)
                .await;
        }
    }

    fn enter_waiting_after_confirmation_post(
        &self,
        message: &PendingMessage,
        message_id: String,
    ) -> Option<u64> {
        let mut state = self.lock_state();
        let bound = state.sessions.get_mut(&message.token.guild_id())?;
        if bound.token != message.token
            || !bound.pending.as_ref().is_some_and(|pending| {
                pending.id == message.intent.id
                    && pending.revision == message.intent.revision
                    && pending.phase == PendingPhase::PostingConfirmation
            })
        {
            return None;
        }
        let waiting_epoch = bound.next_waiting_epoch;
        bound.next_waiting_epoch = bound.next_waiting_epoch.saturating_add(1);
        let pending = bound.pending.as_mut().expect("pending was checked above");
        pending.phase = PendingPhase::WaitingConfirmation;
        pending.waiting_epoch = waiting_epoch;
        pending.confirmation_message_id = Some(message_id);
        Some(waiting_epoch)
    }

    fn schedule_timeout(
        self: &Arc<Self>,
        token: VoiceSessionToken,
        intent_id: Uuid,
        revision: u64,
        waiting_epoch: u64,
    ) {
        let timeout = self.confirmation_timeout;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let Some(broker) = weak.upgrade() else {
                return;
            };
            broker
                .expire_pending(token, intent_id, revision, waiting_epoch)
                .await;
        });
    }

    async fn expire_pending(
        &self,
        token: VoiceSessionToken,
        intent_id: Uuid,
        revision: u64,
        waiting_epoch: u64,
    ) {
        let expired = {
            let mut state = self.lock_state();
            let Some(bound) = state.sessions.get_mut(&token.guild_id()) else {
                return;
            };
            if bound.token != token {
                return;
            }
            let Some(pending) = bound.pending.as_ref() else {
                return;
            };
            if pending.id != intent_id
                || pending.revision != revision
                || pending.waiting_epoch != waiting_epoch
                || pending.phase != PendingPhase::WaitingConfirmation
            {
                return;
            }
            let pending = bound.pending.take().expect("pending was checked above");
            PendingMessage::from_bound(bound, pending)
        };

        let content = bounded_discord_message(format!(
            "<@{}> 語音任務確認已逾時，未送出或執行任何指令。",
            expired.operator_user_id
        ));
        if let Err(error) = self
            .messenger
            .send_message(expired.control_channel_id, &content, None)
            .await
        {
            warn!(error = %error, "failed to report expired Discord voice intent");
        }
    }

    fn remove_pending_if_current(
        &self,
        token: VoiceSessionToken,
        intent_id: Uuid,
        revision: u64,
        phase: PendingPhase,
    ) -> bool {
        let mut state = self.lock_state();
        let Some(bound) = state.sessions.get_mut(&token.guild_id()) else {
            return false;
        };
        if bound.token != token
            || !bound.pending.as_ref().is_some_and(|pending| {
                pending.id == intent_id && pending.revision == revision && pending.phase == phase
            })
        {
            return false;
        }
        bound.pending = None;
        true
    }

    fn restore_waiting_if_current(&self, message: &PendingMessage) -> Option<u64> {
        let mut state = self.lock_state();
        let bound = state.sessions.get_mut(&message.token.guild_id())?;
        if bound.token != message.token {
            return None;
        }
        if !bound.pending.as_ref().is_some_and(|pending| {
            pending.id == message.intent.id
                && pending.revision == message.intent.revision
                && pending.phase == PendingPhase::Dispatching
        }) {
            return None;
        }
        let waiting_epoch = bound.next_waiting_epoch;
        bound.next_waiting_epoch = bound.next_waiting_epoch.saturating_add(1);
        let pending = bound.pending.as_mut().expect("pending was checked above");
        pending.phase = PendingPhase::WaitingConfirmation;
        pending.waiting_epoch = waiting_epoch;
        Some(waiting_epoch)
    }

    async fn delete_message_best_effort(&self, channel_id: u64, message_id: &str) {
        if let Err(error) = self.messenger.delete_message(channel_id, message_id).await {
            warn!(
                error = %error,
                channel_id,
                message_id,
                "failed to delete stale Discord voice confirmation"
            );
        }
    }

    fn delete_message_in_background(&self, channel_id: u64, message_id: String) {
        let messenger = self.messenger.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            warn!(
                channel_id,
                message_id,
                "could not delete stale Discord voice confirmation outside a Tokio runtime"
            );
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = messenger.delete_message(channel_id, &message_id).await {
                warn!(
                    error = %error,
                    channel_id,
                    message_id,
                    "failed to delete stale Discord voice confirmation"
                );
            }
        });
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BrokerState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

fn record_transcript_key(bound: &mut BoundSession, key: TranscriptKey) -> bool {
    if !bound.processed_keys.insert(key) {
        return false;
    }
    bound.processed_key_order.push_back(key);
    while bound.processed_key_order.len() > MAX_PROCESSED_TRANSCRIPT_KEYS {
        if let Some(evicted) = bound.processed_key_order.pop_front() {
            bound.processed_keys.remove(&evicted);
        }
    }
    true
}

fn visible_prompt_to_delete(bound: BoundSession) -> Option<(u64, String)> {
    let pending = bound.pending?;
    if pending.phase == PendingPhase::Dispatching {
        // The operator already confirmed this prompt; retain it as part of the
        // audit trail even if lifecycle shutdown races the final dispatch.
        return None;
    }
    let message_id = pending.confirmation_message_id?;
    Some((bound.control_channel_id, message_id))
}

fn canonicalize_task(task: &str, maximum_chars: usize) -> String {
    if task.chars().count() <= maximum_chars {
        return task.to_string();
    }
    task.chars()
        .take(maximum_chars.saturating_sub(1))
        .chain(std::iter::once('\u{2026}'))
        .collect()
}

#[derive(Debug, Clone)]
struct PendingMessage {
    token: VoiceSessionToken,
    control_channel_id: u64,
    operator_user_id: u64,
    operator_display_name: String,
    intent: PendingIntent,
}

impl PendingMessage {
    fn from_bound(bound: &BoundSession, intent: PendingIntent) -> Self {
        Self {
            token: bound.token,
            control_channel_id: bound.control_channel_id,
            operator_user_id: bound.operator_user_id,
            operator_display_name: bound.operator_display_name.clone(),
            intent,
        }
    }
}

impl LocalIntentExecution {
    fn from_pending(message: &PendingMessage) -> Self {
        Self {
            intent_id: message.intent.id,
            revision: message.intent.revision,
            session: message.token,
            control_channel_id: message.control_channel_id,
            operator_user_id: message.operator_user_id,
            operator_display_name: message.operator_display_name.clone(),
            task: message.intent.proposal.task.clone(),
        }
    }
}

#[derive(Debug)]
enum TextEffect {
    Dispatch(PendingMessage),
    ExecuteLocal(LocalIntentExecution),
    Cancel(PendingMessage),
    Correct(PendingMessage),
    RejectCorrection(PendingMessage),
}

#[derive(Debug, PartialEq, Eq)]
enum Confirmation<'a> {
    Affirmative,
    Negative,
    Correction(&'a str),
    Unrelated,
}

fn classify_confirmation(content: &str) -> Confirmation<'_> {
    let trimmed = content.trim();
    let exact = trimmed
        .trim_matches(is_confirmation_punctuation)
        .trim()
        .to_ascii_lowercase();
    if matches!(
        exact.as_str(),
        "對" | "对"
            | "是"
            | "是的"
            | "好"
            | "好的"
            | "沒錯"
            | "没错"
            | "yes"
            | "y"
            | "correct"
            | "confirm"
    ) {
        return Confirmation::Affirmative;
    }
    if matches!(
        exact.as_str(),
        "不是" | "不對" | "不对" | "不要" | "取消" | "no" | "nope" | "cancel"
    ) {
        return Confirmation::Negative;
    }

    const CORRECTION_PREFIXES: &[&str] = &[
        "更正",
        "修正",
        "改成",
        "其實",
        "其实",
        "不是",
        "correction",
        "actually",
    ];
    let lowered = trimmed.to_ascii_lowercase();
    for prefix in CORRECTION_PREFIXES {
        if !lowered.starts_with(prefix) {
            continue;
        }
        let remainder = &trimmed[prefix.len()..];
        if remainder.is_empty() {
            continue;
        }
        if let Some(first) = remainder.chars().next() {
            if first.is_whitespace() || is_correction_separator(first) {
                return Confirmation::Correction(
                    remainder
                        .trim_start_matches(|character: char| {
                            character.is_whitespace() || is_correction_separator(character)
                        })
                        .trim(),
                );
            }
        }
    }
    Confirmation::Unrelated
}

fn normalize_for_matching(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Unicode lowercase text plus the original byte boundary for each normalized
/// character boundary. This keeps matching consistent with config validation
/// without slicing the original transcript at a byte offset changed by Unicode
/// case expansion.
struct NormalizedText {
    value: String,
    boundaries: Vec<(usize, usize)>,
}

impl NormalizedText {
    fn new(original: &str) -> Self {
        let mut value = String::with_capacity(original.len());
        let mut boundaries = Vec::with_capacity(original.chars().count() + 1);
        for (original_start, character) in original.char_indices() {
            boundaries.push((value.len(), original_start));
            value.extend(character.to_lowercase());
        }
        boundaries.push((value.len(), original.len()));
        boundaries.dedup_by_key(|boundary| boundary.0);
        Self { value, boundaries }
    }

    fn original_index(&self, normalized_index: usize) -> Option<usize> {
        self.boundaries
            .binary_search_by_key(&normalized_index, |boundary| boundary.0)
            .ok()
            .map(|index| self.boundaries[index].1)
    }
}

fn alias_matches<'a>(
    haystack: &'a str,
    alias: &'a str,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    haystack.match_indices(alias).filter_map(move |(start, _)| {
        let end = start + alias.len();
        if alias_requires_word_boundary(alias)
            && (!word_boundary_before(haystack, start) || !word_boundary_after(haystack, end))
        {
            None
        } else {
            Some((start, end))
        }
    })
}

fn alias_requires_word_boundary(alias: &str) -> bool {
    alias
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn word_boundary_before(text: &str, byte_index: usize) -> bool {
    text[..byte_index]
        .chars()
        .next_back()
        .is_none_or(|character| !is_ascii_word(character))
}

fn word_boundary_after(text: &str, byte_index: usize) -> bool {
    text[byte_index..]
        .chars()
        .next()
        .is_none_or(|character| !is_ascii_word(character))
}

fn is_ascii_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn command_prefix_allowed(prefix: &str) -> bool {
    let prefix = prefix.trim_matches(is_command_separator).trim();
    if prefix.is_empty() {
        return true;
    }

    const PREFIXES: &[&str] = &[
        "請",
        "请",
        "叫",
        "讓",
        "让",
        "麻煩",
        "麻烦",
        "幫我叫",
        "帮我叫",
        "幫我請",
        "帮我请",
        "請幫我叫",
        "请帮我叫",
        "可以請",
        "可以请",
        "你幫我叫",
        "你帮我叫",
        "please",
        "ask",
        "tell",
        "please ask",
        "please tell",
        "have",
        "get",
    ];
    PREFIXES.iter().any(|candidate| {
        prefix == *candidate
            || prefix
                .strip_suffix(candidate)
                .is_some_and(|head| head.trim().is_empty())
    })
}

const LOCAL_REQUEST_PREFIXES: &[&str] = &[
    "麻煩你幫我",
    "麻烦你帮我",
    "可以請你幫我",
    "可以请你帮我",
    "請你幫我",
    "请你帮我",
    "麻煩幫我",
    "麻烦帮我",
    "可以幫我",
    "可以帮我",
    "請幫我",
    "请帮我",
    "你幫我",
    "你帮我",
    "幫我",
    "帮我",
    "could you",
    "can you",
    "please",
    "請",
    "请",
];

fn resolve_local_task(text: &str) -> Option<String> {
    let trimmed = text
        .trim_matches(|character: char| {
            character.is_whitespace() || is_command_separator(character)
        })
        .trim();
    if let Some(remainder) = strip_local_request_prefix(trimmed) {
        return clean_task(remainder);
    }
    starts_with_local_action(trimmed)
        .then(|| clean_task(trimmed))
        .flatten()
}

fn has_explicit_local_prefix(text: &str) -> bool {
    let trimmed = text
        .trim_matches(|character: char| {
            character.is_whitespace() || is_command_separator(character)
        })
        .trim();
    strip_local_request_prefix(trimmed).is_some()
}

fn strip_local_request_prefix(text: &str) -> Option<&str> {
    let lowered = text.to_ascii_lowercase();
    LOCAL_REQUEST_PREFIXES.iter().find_map(|prefix| {
        if !lowered.starts_with(prefix) {
            return None;
        }
        let remainder = &text[prefix.len()..];
        if matches!(*prefix, "請" | "请")
            && !starts_with_local_action(
                remainder
                    .trim_start_matches(|character: char| {
                        character.is_whitespace() || is_command_separator(character)
                    })
                    .trim(),
            )
        {
            return None;
        }
        if prefix
            .chars()
            .all(|character| character.is_ascii_alphabetic() || character.is_whitespace())
            && remainder.chars().next().is_some_and(is_ascii_word)
        {
            None
        } else {
            Some(remainder)
        }
    })
}

fn starts_with_local_action(text: &str) -> bool {
    const ACTIONS: &[&str] = &[
        "檢查",
        "检查",
        "執行",
        "执行",
        "修復",
        "修复",
        "整理",
        "總結",
        "总结",
        "調查",
        "调查",
        "查看",
        "review",
        "check",
        "inspect",
        "run",
        "fix",
        "summarize",
        "analyse",
        "analyze",
    ];
    let lowered = text.to_ascii_lowercase();
    let lowered = lowered
        .strip_prefix('先')
        .map(str::trim_start)
        .unwrap_or(&lowered);
    ACTIONS.iter().any(|action| {
        let Some(remainder) = lowered.strip_prefix(action) else {
            return false;
        };
        !action
            .chars()
            .all(|character| character.is_ascii_alphabetic())
            || remainder
                .chars()
                .next()
                .is_none_or(|next| !is_ascii_word(next))
    })
}

fn clean_task(text: &str) -> Option<String> {
    let mut task = text
        .trim_matches(|character: char| {
            character.is_whitespace() || is_command_separator(character)
        })
        .trim();
    const LEADING_FILLERS: &[&str] = &[
        "請幫我",
        "请帮我",
        "幫我",
        "帮我",
        "請",
        "请",
        "去",
        "to",
        "please",
    ];
    loop {
        let mut changed = false;
        for filler in LEADING_FILLERS {
            if let Some(remainder) = strip_task_connector(task, filler) {
                task = remainder
                    .trim_matches(|character: char| {
                        character.is_whitespace() || is_command_separator(character)
                    })
                    .trim();
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    if task.is_empty() {
        None
    } else {
        Some(task.to_string())
    }
}

fn strip_task_connector<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let lowered = text.to_ascii_lowercase();
    if !lowered.starts_with(prefix) {
        return None;
    }
    let remainder = &text[prefix.len()..];
    let Some(next) = remainder.chars().next() else {
        return Some(remainder);
    };
    if prefix
        .chars()
        .all(|character| character.is_ascii_alphabetic())
    {
        return (!is_ascii_word(next)).then_some(remainder);
    }
    if matches!(prefix, "幫我" | "帮我" | "請幫我" | "请帮我")
        || next.is_whitespace()
        || is_command_separator(next)
        || next.is_ascii_alphanumeric()
    {
        Some(remainder)
    } else {
        None
    }
}

fn delegate_confirmation_message(operator_user_id: u64, target: &str, task: &str) -> String {
    bounded_discord_message(format!(
        "<@{operator_user_id}> 我理解為：要請 {target} {task}，對嗎？請回覆「對」、「不是」，或用「更正：...」修正。"
    ))
}

fn local_confirmation_message(operator_user_id: u64, task: &str) -> String {
    bounded_discord_message(format!(
        "<@{operator_user_id}> 我理解為：由我直接處理「{task}」，對嗎？請回覆「對」、「不是」，或用「更正：...」修正。"
    ))
}

fn dispatch_message(
    target_user_id: u64,
    operator_user_id: u64,
    operator_display_name: &str,
    task: &str,
) -> String {
    bounded_discord_message(format!(
        "<@{target_user_id}> {operator_display_name} (<@{operator_user_id}>) asked via voice:\n{task}"
    ))
}

fn dispatch_nonce(intent_id: Uuid) -> String {
    // Discord accepts at most 25 characters for a string nonce. The prefix
    // versions the derivation; the remaining 20 hexadecimal characters are a
    // stable projection of the immutable intent ID and are reused on retries.
    let simple = intent_id.simple().to_string();
    format!("oabv1{}", &simple[..20])
}

fn confirmation_nonce(intent_id: Uuid) -> String {
    let simple = intent_id.simple().to_string();
    format!("oabp1{}", &simple[..20])
}

fn confirmation_retry_delay(attempt: usize) -> Duration {
    let multiplier = 1_u32 << u32::try_from(attempt.min(3)).unwrap_or(3);
    CONFIRMATION_RETRY_BASE
        .saturating_mul(multiplier)
        .min(CONFIRMATION_RETRY_MAX)
}

fn bounded_discord_message(message: String) -> String {
    if message.chars().count() <= DISCORD_MESSAGE_LIMIT {
        return message;
    }
    message
        .chars()
        .take(DISCORD_MESSAGE_LIMIT.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn sanitize_display_name(name: String, operator_user_id: u64) -> String {
    let name = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('@', "@\u{200B}");
    if name.is_empty() {
        operator_user_id.to_string()
    } else {
        name
    }
}

fn is_ascii_command_punctuation(character: char) -> bool {
    matches!(character, ',' | ':' | ';' | '-' | '>' | '.' | '!' | '?')
}

fn is_command_separator(character: char) -> bool {
    is_ascii_command_punctuation(character)
        || matches!(character, '，' | '：' | '；' | '。' | '！' | '？' | '、')
}

fn is_confirmation_punctuation(character: char) -> bool {
    is_command_separator(character) || matches!(character, '"' | '\'' | '「' | '」')
}

fn is_correction_separator(character: char) -> bool {
    matches!(character, ':' | ',' | ';' | '-' | '：' | '，' | '；')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DiscordVoiceIntentTargetConfig;
    use anyhow::anyhow;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::sync::Notify;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SentMessage {
        message_id: String,
        channel_id: u64,
        content: String,
        nonce: Option<String>,
    }

    #[derive(Default)]
    struct FakeDeliveryState {
        messages: Vec<SentMessage>,
        nonce_ids: HashMap<String, String>,
        deleted_ids: HashSet<String>,
    }

    #[derive(Default)]
    struct FakeMessenger {
        delivery: Mutex<FakeDeliveryState>,
        next_message_id: AtomicU64,
        send_calls: AtomicU64,
        fail_next: AtomicBool,
        accept_then_fail_next: AtomicBool,
        fail_always: AtomicBool,
        dispatch_entered: Notify,
        release_dispatch: Notify,
        block_dispatch: AtomicBool,
        confirmation_entered: Notify,
        release_confirmation: Notify,
        block_confirmation: AtomicBool,
    }

    impl FakeMessenger {
        fn messages(&self) -> Vec<SentMessage> {
            self.delivery
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .messages
                .clone()
        }

        fn visible_messages(&self) -> Vec<SentMessage> {
            let delivery = self
                .delivery
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            delivery
                .messages
                .iter()
                .filter(|message| !delivery.deleted_ids.contains(&message.message_id))
                .cloned()
                .collect()
        }

        fn fail_next(&self) {
            self.fail_next.store(true, Ordering::SeqCst);
        }

        fn accept_then_fail_next(&self) {
            self.accept_then_fail_next.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl VoiceIntentMessenger for FakeMessenger {
        async fn send_message(
            &self,
            channel_id: u64,
            content: &str,
            nonce: Option<&str>,
        ) -> Result<String> {
            self.send_calls.fetch_add(1, Ordering::SeqCst);
            let is_confirmation = nonce.is_some_and(|value| value.starts_with("oabp1"));
            let is_dispatch = nonce.is_some_and(|value| value.starts_with("oabv1"));
            if is_confirmation && self.block_confirmation.load(Ordering::SeqCst) {
                self.confirmation_entered.notify_one();
                self.release_confirmation.notified().await;
            }
            if is_dispatch && self.block_dispatch.load(Ordering::SeqCst) {
                self.dispatch_entered.notify_one();
                self.release_dispatch.notified().await;
            }
            if self.fail_always.load(Ordering::SeqCst)
                || self.fail_next.swap(false, Ordering::SeqCst)
            {
                return Err(anyhow!("synthetic send failure"));
            }

            let mut delivery = self
                .delivery
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(existing) = nonce.and_then(|value| delivery.nonce_ids.get(value)) {
                return Ok(existing.clone());
            }
            let message_id = format!(
                "message-{}",
                self.next_message_id.fetch_add(1, Ordering::SeqCst) + 1
            );
            delivery.messages.push(SentMessage {
                message_id: message_id.clone(),
                channel_id,
                content: content.to_string(),
                nonce: nonce.map(str::to_string),
            });
            if let Some(nonce) = nonce {
                delivery
                    .nonce_ids
                    .insert(nonce.to_string(), message_id.clone());
            }
            drop(delivery);
            if self.accept_then_fail_next.swap(false, Ordering::SeqCst) {
                return Err(anyhow!("synthetic accepted-but-response-lost failure"));
            }
            Ok(message_id)
        }

        async fn delete_message(&self, _channel_id: u64, message_id: &str) -> Result<()> {
            self.delivery
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .deleted_ids
                .insert(message_id.to_string());
            Ok(())
        }
    }

    fn intent_config(timeout_seconds: u64) -> DiscordVoiceIntentConfig {
        DiscordVoiceIntentConfig {
            enabled: true,
            confirmation_timeout_seconds: timeout_seconds,
            default_to_local: false,
            targets: BTreeMap::from([
                (
                    "B0".to_string(),
                    DiscordVoiceIntentTargetConfig {
                        discord_user_id: "9000".to_string(),
                        aliases: vec!["bot0".to_string(), "零號".to_string(), "Sam".to_string()],
                    },
                ),
                (
                    "B1".to_string(),
                    DiscordVoiceIntentTargetConfig {
                        discord_user_id: "9001".to_string(),
                        aliases: vec!["bot1".to_string(), "一號".to_string()],
                    },
                ),
            ]),
        }
    }

    fn token(guild_id: u64, session_id: u64) -> VoiceSessionToken {
        VoiceSessionToken::for_test(guild_id, session_id)
    }

    fn broker(timeout_seconds: u64) -> (Arc<DiscordVoiceIntentBroker>, Arc<FakeMessenger>) {
        let messenger = Arc::new(FakeMessenger::default());
        let broker =
            DiscordVoiceIntentBroker::new(intent_config(timeout_seconds), messenger.clone())
                .expect("valid test broker");
        (broker, messenger)
    }

    fn local_broker(timeout_seconds: u64) -> (Arc<DiscordVoiceIntentBroker>, Arc<FakeMessenger>) {
        let messenger = Arc::new(FakeMessenger::default());
        let mut config = intent_config(timeout_seconds);
        config.default_to_local = true;
        let broker = DiscordVoiceIntentBroker::new(config, messenger.clone())
            .expect("valid local-first test broker");
        (broker, messenger)
    }

    async fn propose(broker: &Arc<DiscordVoiceIntentBroker>, token: VoiceSessionToken) {
        assert_eq!(
            broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "幫我叫 B0 group review PR #123".to_string(),
                })
                .await
                .unwrap(),
            VoiceIntentTranscriptOutcome::Proposed
        );
    }

    async fn propose_local(broker: &Arc<DiscordVoiceIntentBroker>, token: VoiceSessionToken) {
        assert_eq!(
            broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "請幫我 review PR #123".to_string(),
                })
                .await
                .unwrap(),
            VoiceIntentTranscriptOutcome::Proposed
        );
    }

    #[test]
    fn parser_requires_one_target_and_a_command_shaped_prefix() {
        let (broker, _) = broker(30);
        for command in [
            "幫我叫 B0 group review PR #123",
            "請幫我叫 B0 group review PR #123",
            "可以請 B0 group review PR #123",
            "你幫我叫 B0 group review PR #123",
            "叫 Sam group review PR #123",
        ] {
            assert_eq!(
                broker.resolve_proposal(command),
                Some(IntentProposal {
                    destination: IntentDestination::Delegate { target_index: 0 },
                    task: "group review PR #123".to_string(),
                }),
                "command should resolve: {command}"
            );
        }
        assert!(broker
            .resolve_proposal("請 B0 跟 B1 一起 review PR #123")
            .is_none());
        assert!(broker
            .resolve_proposal("我昨天跟 B0 討論 PR #123")
            .is_none());
        assert!(broker.resolve_proposal("請 B0").is_none());
        assert!(broker.resolve_proposal("請 B01 review").is_none());
        assert!(broker.resolve_proposal("請幫我 review PR #123").is_none());
    }

    #[test]
    fn local_mode_only_falls_back_for_command_shaped_unaddressed_tasks() {
        let (broker, _) = local_broker(30);
        assert_eq!(
            broker.resolve_proposal("請幫我 review PR #123"),
            Some(IntentProposal {
                destination: IntentDestination::Local,
                task: "review PR #123".to_string(),
            })
        );
        assert_eq!(
            broker.resolve_proposal("run CI"),
            Some(IntentProposal {
                destination: IntentDestination::Local,
                task: "run CI".to_string(),
            })
        );
        for command in [
            "先查看 OpenAPI 171368整理目前施作方向",
            "請先查看 Open App取得issue 1368整理目前行作方向",
        ] {
            assert_eq!(
                broker
                    .resolve_proposal(command)
                    .map(|proposal| proposal.destination),
                Some(IntentDestination::Local),
                "real STT command should resolve locally: {command}"
            );
        }
        assert_eq!(
            broker.resolve_proposal("請幫我叫 Sam group review PR #123"),
            Some(IntentProposal {
                destination: IntentDestination::Delegate { target_index: 0 },
                task: "group review PR #123".to_string(),
            })
        );
        assert!(broker
            .resolve_proposal("請 B0 跟 B1 一起 review PR #123")
            .is_none());
        assert!(broker.resolve_proposal("我昨天 review PR #123").is_none());
    }

    #[test]
    fn target_name_inside_a_local_task_does_not_force_delegation() {
        let (broker, _) = local_broker(30);
        assert_eq!(
            broker.resolve_proposal("先查看 Sam 的 PR 狀態"),
            Some(IntentProposal {
                destination: IntentDestination::Local,
                task: "先查看 Sam 的 PR 狀態".to_string(),
            })
        );
    }

    #[test]
    fn task_cleanup_does_not_corrupt_chinese_compound_verbs_or_nouns() {
        let (broker, _) = broker(30);
        for (command, expected_task) in [
            ("叫 B0 去除 stale label", "去除 stale label"),
            ("叫 B0 檢查執行緒 deadlock", "檢查執行緒 deadlock"),
            ("叫 B0 執行緒 dump", "執行緒 dump"),
            ("叫 B0 請求 review", "請求 review"),
        ] {
            assert_eq!(
                broker
                    .resolve_proposal(command)
                    .map(|proposal| proposal.task),
                Some(expected_task.to_string()),
                "task should remain semantically intact: {command}"
            );
        }
    }

    #[test]
    fn confirmation_classification_is_exact_but_allows_punctuation() {
        assert_eq!(classify_confirmation(" 對！ "), Confirmation::Affirmative);
        assert_eq!(classify_confirmation("不是"), Confirmation::Negative);
        assert_eq!(
            classify_confirmation("不是，review only"),
            Confirmation::Correction("review only")
        );
        assert_eq!(
            classify_confirmation("更正：請 B1 run CI"),
            Confirmation::Correction("請 B1 run CI")
        );
        assert_eq!(
            classify_confirmation("對了，還有一件事"),
            Confirmation::Unrelated
        );
    }

    #[tokio::test]
    async fn only_exact_bound_session_channel_and_operator_can_propose() {
        let (broker, messenger) = broker(30);
        let current = token(100, 2);
        broker.bind_session(current, 200, 300, "Can");

        for event in [
            FinalTranscriptEvent {
                token: token(100, 1),
                control_channel_id: 200,
                key: TranscriptKey {
                    speaker_id: 300,
                    start_frame: 100,
                    end_frame: 200,
                },
                text: "B0 review PR #123".to_string(),
            },
            FinalTranscriptEvent {
                token: current,
                control_channel_id: 201,
                key: TranscriptKey {
                    speaker_id: 300,
                    start_frame: 100,
                    end_frame: 200,
                },
                text: "B0 review PR #123".to_string(),
            },
            FinalTranscriptEvent {
                token: current,
                control_channel_id: 200,
                key: TranscriptKey {
                    speaker_id: 301,
                    start_frame: 100,
                    end_frame: 200,
                },
                text: "B0 review PR #123".to_string(),
            },
        ] {
            assert_eq!(
                broker.handle_final_transcript(event).await.unwrap(),
                VoiceIntentTranscriptOutcome::Ignored
            );
        }
        assert!(messenger.messages().is_empty());

        propose(&broker, current).await;
        let messages = messenger.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].channel_id, 200);
        assert!(messages[0].content.contains("<@300>"));
        assert!(messages[0].content.contains("B0 group review PR #123"));
        assert!(messages[0]
            .nonce
            .as_deref()
            .is_some_and(|nonce| nonce.starts_with("oabp1") && nonce.len() == 25));
    }

    #[tokio::test]
    async fn unaddressed_commands_remain_ignored_when_local_mode_is_disabled() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");

        assert_eq!(
            broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: current,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "請幫我 review PR #123".to_string(),
                })
                .await
                .unwrap(),
            VoiceIntentTranscriptOutcome::Ignored
        );
        assert!(messenger.messages().is_empty());
    }

    #[tokio::test]
    async fn affirmative_emits_one_local_execution_without_a_delegate_message() {
        let (broker, messenger) = local_broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose_local(&broker, current).await;

        let prompt = messenger.messages().pop().unwrap();
        assert!(prompt
            .content
            .contains("由我直接處理「review PR #123」"));
        let outcome = broker
            .handle_text_message(100, 200, 300, "對")
            .await
            .unwrap();
        let VoiceIntentTextOutcome::ExecuteLocal(request) = outcome else {
            panic!("expected local execution handoff")
        };
        assert_ne!(request.intent_id, Uuid::nil());
        assert_eq!(request.revision, 1);
        assert_eq!(request.session, current);
        assert_eq!(request.control_channel_id, 200);
        assert_eq!(request.operator_user_id, 300);
        assert_eq!(request.operator_display_name, "Can");
        assert_eq!(request.task, "review PR #123");

        assert_eq!(messenger.messages().len(), 1);
        assert!(messenger.messages().iter().all(|message| !message
            .nonce
            .as_deref()
            .is_some_and(|nonce| nonce.starts_with("oabv1"))));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::AlreadyProcessing
        );
        assert!(broker.reopen_local_execution(&request));
        assert!(!broker.reopen_local_execution(&request));
        let VoiceIntentTextOutcome::ExecuteLocal(retried) = broker
            .handle_text_message(100, 200, 300, "對")
            .await
            .unwrap()
        else {
            panic!("expected reopened local execution handoff")
        };
        assert_eq!(retried, request);
        assert!(broker.complete_local_execution(&retried));
        assert!(!broker.complete_local_execution(&retried));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
    }

    #[tokio::test]
    async fn local_mode_preserves_explicit_target_delegation() {
        let (broker, messenger) = local_broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        assert_eq!(
            messenger
                .messages()
                .iter()
                .filter(|message| message.content.starts_with("<@9000>"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn local_intent_can_be_cancelled_without_execution() {
        let (broker, messenger) = local_broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose_local(&broker, current).await;

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "不是")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Cancelled
        );
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
        assert!(messenger.messages().iter().all(|message| !message
            .nonce
            .as_deref()
            .is_some_and(|nonce| nonce.starts_with("oabv1"))));
    }

    #[tokio::test]
    async fn correction_switches_modes_only_when_the_new_destination_is_explicit() {
        let (broker, messenger) = local_broker(30);
        let first = token(100, 1);
        broker.bind_session(first, 200, 300, "Can");
        propose_local(&broker, first).await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "更正：請 B1 run CI")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Corrected
        );
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        assert!(messenger
            .messages()
            .last()
            .unwrap()
            .content
            .starts_with("<@9001>"));

        let second = token(100, 2);
        broker.bind_session(second, 200, 300, "Can");
        propose(&broker, second).await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "更正：review only")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Corrected
        );
        assert!(messenger
            .messages()
            .last()
            .unwrap()
            .content
            .contains("B0 review only"));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "更正：你幫我 run CI")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Corrected
        );
        assert!(messenger
            .messages()
            .last()
            .unwrap()
            .content
            .contains("由我直接處理「run CI」"));
        let VoiceIntentTextOutcome::ExecuteLocal(request) = broker
            .handle_text_message(100, 200, 300, "對")
            .await
            .unwrap()
        else {
            panic!("expected correction to switch to local execution")
        };
        assert_eq!(request.task, "run CI");
    }

    #[tokio::test]
    async fn affirmative_dispatches_one_auditable_real_mention() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        let messages = messenger.messages();
        assert_eq!(messages.len(), 2);
        assert!(messages[1]
            .content
            .starts_with("<@9000> Can (<@300>) asked via voice:\n"));
        assert!(messages[1].content.ends_with("group review PR #123"));
        assert_eq!(messages[1].nonce.as_ref().unwrap().len(), 25);

        // There is no longer a pending intent, so a later conversational "yes"
        // is not swallowed and cannot dispatch again.
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
        assert_eq!(messenger.messages().len(), 2);
    }

    #[tokio::test]
    async fn oversized_task_is_canonicalized_once_for_confirmation_and_dispatch() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        let raw_task = "x".repeat(3_000);
        assert_eq!(
            broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: current,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: format!("請 B0 {raw_task}"),
                })
                .await
                .unwrap(),
            VoiceIntentTranscriptOutcome::Proposed
        );
        let confirmation = messenger.messages()[0].clone();
        assert!(confirmation.content.chars().count() <= DISCORD_MESSAGE_LIMIT);
        let confirmation_task = confirmation
            .content
            .split_once("B0 ")
            .unwrap()
            .1
            .strip_suffix("，對嗎？請回覆「對」、「不是」，或用「更正：...」修正。")
            .unwrap()
            .to_string();
        assert!(confirmation_task.ends_with('…'));

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        let dispatch = messenger.messages()[1].clone();
        assert!(dispatch.content.chars().count() <= DISCORD_MESSAGE_LIMIT);
        let dispatch_task = dispatch.content.split_once('\n').unwrap().1;
        assert_eq!(confirmation_task, dispatch_task);
    }

    #[tokio::test]
    async fn replayed_final_transcript_cannot_create_a_second_proposal() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );

        assert_eq!(
            broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: current,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "幫我叫 B0 group review PR #123".to_string(),
                })
                .await
                .unwrap(),
            VoiceIntentTranscriptOutcome::Ignored
        );
        assert_eq!(messenger.messages().len(), 2);
    }

    #[tokio::test]
    async fn confirmation_cannot_dispatch_before_proposal_message_is_posted() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        messenger.block_confirmation.store(true, Ordering::SeqCst);

        let proposing_broker = broker.clone();
        let proposing = tokio::spawn(async move {
            proposing_broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: current,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "請 B0 review PR #123".to_string(),
                })
                .await
        });
        messenger.confirmation_entered.notified().await;

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::AlreadyProcessing
        );
        assert!(messenger.messages().is_empty());

        messenger.block_confirmation.store(false, Ordering::SeqCst);
        messenger.release_confirmation.notify_one();
        assert_eq!(
            proposing.await.unwrap().unwrap(),
            VoiceIntentTranscriptOutcome::Proposed
        );
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
    }

    #[tokio::test]
    async fn concurrent_affirmatives_make_one_atomic_dispatch() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        messenger.block_dispatch.store(true, Ordering::SeqCst);

        let first_broker = broker.clone();
        let first =
            tokio::spawn(
                async move { first_broker.handle_text_message(100, 200, 300, "yes").await },
            );
        messenger.dispatch_entered.notified().await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "yes")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::AlreadyProcessing
        );
        messenger.release_dispatch.notify_waiters();
        assert_eq!(
            first.await.unwrap().unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        assert_eq!(
            messenger
                .messages()
                .iter()
                .filter(|message| {
                    message
                        .nonce
                        .as_deref()
                        .is_some_and(|nonce| nonce.starts_with("oabv1"))
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn wrong_confirmation_context_is_not_consumed() {
        let (broker, _) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;

        for (guild, channel, user) in [(101, 200, 300), (100, 201, 300), (100, 200, 301)] {
            assert_eq!(
                broker
                    .handle_text_message(guild, channel, user, "對")
                    .await
                    .unwrap(),
                VoiceIntentTextOutcome::NotApplicable
            );
        }
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "unrelated")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
    }

    #[tokio::test]
    async fn cancellation_removes_the_pending_intent() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "不是")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Cancelled
        );
        assert!(messenger.messages()[1].content.contains("已取消"));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
    }

    #[tokio::test]
    async fn correction_can_keep_or_change_the_target_and_invalidates_old_revision() {
        let (broker, messenger) = broker(1);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "更正：review only，不要 merge")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Corrected
        );
        assert!(messenger.messages()[1]
            .content
            .contains("B0 review only，不要 merge"));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "更正：請 B1 run CI")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Corrected
        );
        assert!(messenger.messages()[2].content.contains("B1 run CI"));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        assert!(messenger
            .messages()
            .last()
            .unwrap()
            .content
            .starts_with("<@9001>"));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_timeout_cannot_remove_revised_or_replacement_session_intent() {
        let (broker, messenger) = broker(2);
        let old = token(100, 1);
        broker.bind_session(old, 200, 300, "Can");
        propose(&broker, old).await;
        broker
            .handle_text_message(100, 200, 300, "更正：review only")
            .await
            .unwrap();

        tokio::time::advance(Duration::from_secs(1)).await;
        let replacement = token(100, 2);
        broker.bind_session(replacement, 200, 300, "Can");
        propose(&broker, replacement).await;

        // Both timers belonging to the previous token/revision become due.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        assert!(messenger
            .messages()
            .iter()
            .all(|message| !message.content.contains("已逾時")));
    }

    #[tokio::test(start_paused = true)]
    async fn current_timeout_expires_without_dispatching() {
        let (broker, messenger) = broker(2);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        assert!(messenger
            .messages()
            .iter()
            .any(|message| message.content.contains("已逾時")));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_dispatch_automatically_recovers_with_the_same_nonce() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        messenger.fail_next();
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatching
        );
        tokio::task::yield_now().await;
        tokio::time::advance(confirmation_retry_delay(0)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
        let dispatch = messenger
            .messages()
            .into_iter()
            .find(|message| {
                message
                    .nonce
                    .as_deref()
                    .is_some_and(|nonce| nonce.starts_with("oabv1"))
            })
            .unwrap();
        assert_eq!(dispatch.nonce.unwrap().len(), 25);
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_but_lost_dispatch_response_recovers_one_target_message_without_timeout() {
        let (broker, messenger) = broker(2);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        messenger.accept_then_fail_next();

        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatching
        );
        assert_eq!(
            messenger
                .visible_messages()
                .iter()
                .filter(|message| message.content.starts_with("<@9000>"))
                .count(),
            1
        );

        tokio::task::yield_now().await;
        tokio::time::advance(confirmation_retry_delay(0)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            messenger
                .visible_messages()
                .iter()
                .filter(|message| message.content.starts_with("<@9000>"))
                .count(),
            1
        );
        assert!(messenger
            .messages()
            .iter()
            .all(|message| !message.content.contains("已逾時")));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_dispatch_gets_new_waiting_epoch_and_old_timer_cannot_expire_it() {
        let (broker, messenger) = broker(60);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        messenger.fail_always.store(true, Ordering::SeqCst);
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatching
        );
        tokio::task::yield_now().await;

        for attempt in 0..MAX_CONFIRMATION_RETRY_ATTEMPTS {
            tokio::time::advance(confirmation_retry_delay(attempt)).await;
            tokio::task::yield_now().await;
        }
        messenger.fail_always.store(false, Ordering::SeqCst);

        // Recovery exhausted at t=24 and reopened Waiting with a new epoch. The
        // original epoch reaches its t=60 deadline now and must not expire it.
        tokio::time::advance(Duration::from_secs(36)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        assert!(messenger
            .messages()
            .iter()
            .all(|message| !message.content.contains("已逾時")));
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_corrected_prompt_recovers_same_message_and_never_restores_old_task() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        propose(&broker, current).await;

        // Simulate Discord accepting the revised prompt but losing the HTTP
        // response. The prompt is visible, while the broker remains in Posting.
        messenger.accept_then_fail_next();
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "更正：review only，不要 merge")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Corrected
        );
        assert_eq!(messenger.visible_messages().len(), 2);
        assert!(messenger.visible_messages()[1]
            .content
            .contains("review only，不要 merge"));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::AlreadyProcessing
        );

        // Retry uses the same enforced nonce, recovers the existing message ID,
        // and opens confirmation for the revised intent without duplicating it.
        tokio::task::yield_now().await;
        tokio::time::advance(confirmation_retry_delay(0)).await;
        tokio::task::yield_now().await;
        assert_eq!(messenger.visible_messages().len(), 2);
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::Dispatched
        );
        let dispatch = messenger.messages().last().unwrap().clone();
        assert!(dispatch.content.ends_with("review only，不要 merge"));
        assert!(!dispatch.content.ends_with("group review PR #123"));
    }

    #[tokio::test]
    async fn stopped_or_replaced_session_cannot_dispatch() {
        let (broker, messenger) = broker(30);
        let old = token(100, 1);
        broker.bind_session(old, 200, 300, "Can");
        propose(&broker, old).await;
        assert!(broker.abandon_session(old));
        assert_eq!(
            broker
                .handle_text_message(100, 200, 300, "對")
                .await
                .unwrap(),
            VoiceIntentTextOutcome::NotApplicable
        );
        tokio::task::yield_now().await;
        assert!(messenger.visible_messages().is_empty());

        broker.bind_session(token(100, 2), 200, 300, "Can");
        assert_eq!(messenger.messages().len(), 1);
        assert!(!broker.abandon_session(old));
    }

    #[tokio::test]
    async fn blocked_prompt_completed_after_abandon_is_deleted_as_stale() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        messenger.block_confirmation.store(true, Ordering::SeqCst);

        let proposing_broker = broker.clone();
        let proposing = tokio::spawn(async move {
            proposing_broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: current,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "請 B0 review PR #123".to_string(),
                })
                .await
        });
        messenger.confirmation_entered.notified().await;
        assert!(broker.abandon_session(current));
        messenger.block_confirmation.store(false, Ordering::SeqCst);
        messenger.release_confirmation.notify_one();
        assert_eq!(
            proposing.await.unwrap().unwrap(),
            VoiceIntentTranscriptOutcome::Proposed
        );
        assert_eq!(messenger.messages().len(), 1);
        assert!(messenger.visible_messages().is_empty());
    }

    #[tokio::test]
    async fn blocked_prompt_completed_after_rebind_is_deleted_as_stale() {
        let (broker, messenger) = broker(30);
        let old = token(100, 1);
        broker.bind_session(old, 200, 300, "Can");
        messenger.block_confirmation.store(true, Ordering::SeqCst);

        let proposing_broker = broker.clone();
        let proposing = tokio::spawn(async move {
            proposing_broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: old,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 100,
                        end_frame: 200,
                    },
                    text: "請 B0 review PR #123".to_string(),
                })
                .await
        });
        messenger.confirmation_entered.notified().await;
        broker.bind_session(token(100, 2), 200, 300, "Can");
        messenger.block_confirmation.store(false, Ordering::SeqCst);
        messenger.release_confirmation.notify_one();
        proposing.await.unwrap().unwrap();
        assert!(messenger.visible_messages().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_prompt_retry_after_abandon_recovers_then_deletes_and_stops() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        messenger.accept_then_fail_next();
        propose(&broker, current).await;
        assert_eq!(messenger.visible_messages().len(), 1);
        assert!(broker.abandon_session(current));

        tokio::task::yield_now().await;
        tokio::time::advance(confirmation_retry_delay(0)).await;
        tokio::task::yield_now().await;
        assert!(messenger.visible_messages().is_empty());
        assert_eq!(messenger.send_calls.load(Ordering::SeqCst), 2);

        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(messenger.send_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn permanently_failed_prompt_retries_are_bounded_and_clear_pending_ticket() {
        let (broker, messenger) = broker(30);
        let current = token(100, 1);
        broker.bind_session(current, 200, 300, "Can");
        messenger.fail_always.store(true, Ordering::SeqCst);
        propose(&broker, current).await;
        tokio::task::yield_now().await;

        for attempt in 0..MAX_CONFIRMATION_RETRY_ATTEMPTS {
            tokio::time::advance(confirmation_retry_delay(attempt)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            messenger.send_calls.load(Ordering::SeqCst),
            1 + u64::try_from(MAX_CONFIRMATION_RETRY_ATTEMPTS).unwrap()
        );

        messenger.fail_always.store(false, Ordering::SeqCst);
        assert_eq!(
            broker
                .handle_final_transcript(FinalTranscriptEvent {
                    token: current,
                    control_channel_id: 200,
                    key: TranscriptKey {
                        speaker_id: 300,
                        start_frame: 300,
                        end_frame: 400,
                    },
                    text: "請 B0 run CI".to_string(),
                })
                .await
                .unwrap(),
            VoiceIntentTranscriptOutcome::Proposed
        );
        assert_eq!(messenger.visible_messages().len(), 1);
        assert!(messenger.visible_messages()[0].content.contains("run CI"));
    }

    #[test]
    fn nonce_is_stable_and_never_exceeds_discord_limit() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        assert_eq!(dispatch_nonce(id), "oabv100112233445566778899");
        assert_eq!(dispatch_nonce(id).len(), 25);
        assert_eq!(confirmation_nonce(id), "oabp100112233445566778899");
        assert_eq!(confirmation_nonce(id).len(), 25);
        assert_ne!(confirmation_nonce(id), dispatch_nonce(id));
    }

    #[test]
    fn disabled_broker_is_inert_without_targets() {
        let messenger = Arc::new(FakeMessenger::default());
        let broker =
            DiscordVoiceIntentBroker::new(DiscordVoiceIntentConfig::default(), messenger).unwrap();
        assert!(!broker.enabled());
        assert!(!broker.bind_session(token(100, 1), 200, 300, "Can"));
    }
}
