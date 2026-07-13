//! Discord voice-channel runtime built on Songbird.
//!
//! Songbird callbacks only update in-memory capture state and non-blockingly
//! enqueue completed PCM segments. WAV encoding and STT run in a bounded worker
//! so a slow transcription provider cannot stall Discord's 20 ms receive loop.

use crate::config::{DiscordVoiceConfig, SttConfig, TtsConfig};
use crate::discord::{DiscordVoiceIntentActionExecutor, DiscordVoiceIntentActionOutcome};
use crate::discord_voice::{
    PcmCaptureConfig, PcmSegment, SpeakerPcmBuffer, TranscriptEntry, TranscriptStore,
    VoiceDropCounters, VoiceDropSnapshot,
};
use crate::discord_voice_intent::{
    DiscordVoiceIntentBroker, FinalTranscriptEvent, TranscriptKey, VoiceIntentTextOutcome,
    VoiceIntentTranscriptOutcome,
};
use crate::discord_voice_speech::DiscordVoiceSpeechAudio;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use songbird::model::payload::{ClientDisconnect, Speaking};
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

const VOICE_TICK_FRAMES: usize = 960; // 20 ms at 48 kHz
const MINIMUM_SPEECH_MS: u32 = 100;
const DEFAULT_SILENCE_THRESHOLD: u16 = 500;
const MAX_TRANSCRIPT_ENTRIES: usize = 10_000;
const MAX_TRACKED_SPEAKERS: usize = 25;
const STT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const TTS_POST_ROLL: Duration = Duration::from_millis(300);
const TTS_WATCHDOG_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceConnectionState {
    Connecting,
    Listening,
    Stopping,
    Stopped,
    Expired,
    Failed,
}

#[derive(Debug, Clone)]
pub struct VoiceSessionStatus {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub control_channel_id: u64,
    pub state: VoiceConnectionState,
    pub elapsed: Duration,
    pub transcript_entries: usize,
    pub transcript_bytes: usize,
    pub tracked_speakers: usize,
    pub ignored_speakers: u64,
    pub evicted_transcript_entries: u64,
    pub rejected_transcript_entries: u64,
    pub pending_segments: u64,
    pub stt_failures: u64,
    pub drops: VoiceDropSnapshot,
}

impl VoiceSessionStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            VoiceConnectionState::Connecting | VoiceConnectionState::Listening
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceSessionToken {
    guild_id: u64,
    session_id: u64,
}

impl VoiceSessionToken {
    pub fn guild_id(self) -> u64 {
        self.guild_id
    }

    #[cfg(test)]
    pub(crate) fn for_test(guild_id: u64, session_id: u64) -> Self {
        Self {
            guild_id,
            session_id,
        }
    }
}

struct SegmentJob {
    speaker_id: u64,
    start_frame: u64,
    end_frame: u64,
    segment: PcmSegment,
}

struct CompletedSegment {
    speaker_id: u64,
    start_frame: u64,
    end_frame: u64,
    transcript: Option<String>,
}

struct IntentWorkerContext {
    tx: mpsc::Sender<FinalTranscriptEvent>,
    token: VoiceSessionToken,
    control_channel_id: u64,
}

struct SpeakerCapture {
    /// Session-global frame at which this speaker's local capture timeline began.
    base_frame: u64,
    buffer: SpeakerPcmBuffer,
}

struct VoiceSession {
    id: u64,
    voice_channel_id: u64,
    control_channel_id: u64,
    state: VoiceConnectionState,
    started_at: Instant,
    ended_at: Option<Instant>,
    session_frames: u64,
    ssrc_to_user: HashMap<u32, u64>,
    user_to_ssrc: HashMap<u64, u32>,
    captures: HashMap<u64, SpeakerCapture>,
    transcript: Arc<Mutex<TranscriptStore>>,
    segment_tx: Option<mpsc::Sender<SegmentJob>>,
    pending_segments: Arc<AtomicU64>,
    stt_failures: Arc<AtomicU64>,
    drops: Arc<VoiceDropCounters>,
    ignored_speakers: u64,
    capture_suppressed: bool,
    playback_epoch: u64,
}

/// Owns at most one voice session per Discord guild.
pub struct DiscordVoiceManager {
    config: DiscordVoiceConfig,
    stt_config: SttConfig,
    tts_config: TtsConfig,
    tts_client: reqwest::Client,
    capture_config: PcmCaptureConfig,
    allowed_voice_channels: HashSet<u64>,
    sessions: Mutex<HashMap<u64, VoiceSession>>,
    songbird: OnceLock<Arc<songbird::Songbird>>,
    intent_broker: OnceLock<Arc<DiscordVoiceIntentBroker>>,
    intent_action_executor: OnceLock<Arc<DiscordVoiceIntentActionExecutor>>,
    next_session_id: AtomicU64,
}

impl DiscordVoiceManager {
    pub fn new(config: DiscordVoiceConfig, stt_config: SttConfig) -> Result<Arc<Self>> {
        Self::new_with_tts(config, stt_config, TtsConfig::default())
    }

    pub fn new_with_tts(
        config: DiscordVoiceConfig,
        stt_config: SttConfig,
        tts_config: TtsConfig,
    ) -> Result<Arc<Self>> {
        if config.max_pending_segments == 0 {
            return Err(anyhow!(
                "discord.voice.max_pending_segments must be greater than zero"
            ));
        }
        if config.max_transcript_bytes == 0 {
            return Err(anyhow!(
                "discord.voice.max_transcript_bytes must be greater than zero"
            ));
        }
        if config.max_session_minutes == 0 {
            return Err(anyhow!(
                "discord.voice.max_session_minutes must be greater than zero"
            ));
        }

        let silence_ms =
            u32::try_from(config.silence_ms).context("discord.voice.silence_ms is too large")?;
        let max_segment_ms = config
            .max_segment_seconds
            .checked_mul(1_000)
            .and_then(|value| u32::try_from(value).ok())
            .context("discord.voice.max_segment_seconds is too large")?;
        let capture_config = PcmCaptureConfig::from_millis(
            DEFAULT_SILENCE_THRESHOLD,
            MINIMUM_SPEECH_MS,
            silence_ms,
            max_segment_ms,
        )
        .context("invalid discord.voice capture configuration")?;

        let mut allowed_voice_channels = HashSet::new();
        for raw in &config.allowed_channels {
            let id = raw.parse::<u64>().with_context(|| {
                format!("invalid Discord voice channel ID in discord.voice.allowed_channels: {raw}")
            })?;
            if id == 0 {
                return Err(anyhow!("Discord voice channel IDs must be non-zero: {raw}"));
            }
            allowed_voice_channels.insert(id);
        }

        Ok(Arc::new(Self {
            config,
            stt_config,
            tts_config,
            tts_client: reqwest::Client::new(),
            capture_config,
            allowed_voice_channels,
            sessions: Mutex::new(HashMap::new()),
            songbird: OnceLock::new(),
            intent_broker: OnceLock::new(),
            intent_action_executor: OnceLock::new(),
            next_session_id: AtomicU64::new(1),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn stt_ready(&self) -> bool {
        self.stt_config.enabled && !self.stt_config.api_key.is_empty()
    }

    pub fn tts_ready(&self) -> bool {
        self.tts_config.enabled && !self.tts_config.api_key.trim().is_empty()
    }

    pub fn voice_channel_allowed(&self, channel_id: u64) -> bool {
        self.allowed_voice_channels.is_empty() || self.allowed_voice_channels.contains(&channel_id)
    }

    pub fn attach_songbird(&self, songbird: Arc<songbird::Songbird>) {
        let _ = self.songbird.set(songbird);
    }

    /// Attaches the optional intent broker before the first voice session.
    /// Existing deployments that do not attach one retain transcript-only
    /// behavior.
    pub fn attach_intent_broker(&self, broker: Arc<DiscordVoiceIntentBroker>) {
        let _ = self.intent_broker.set(broker);
    }

    /// Attaches the reusable local ACP handoff used by spoken confirmations.
    pub fn attach_intent_action_executor(&self, executor: Arc<DiscordVoiceIntentActionExecutor>) {
        let _ = self.intent_action_executor.set(executor);
    }

    fn abandon_intent_session(&self, token: VoiceSessionToken) {
        if let Some(broker) = self.intent_broker.get() {
            broker.abandon_session(token);
        }
    }

    /// Creates capture state before Songbird joins so early receive events are
    /// not lost. Call [`Self::discard_session`] if the join fails.
    pub fn begin_session(
        self: &Arc<Self>,
        guild_id: u64,
        voice_channel_id: u64,
        control_channel_id: u64,
    ) -> Result<VoiceSessionToken> {
        if !self.enabled() {
            return Err(anyhow!("Discord voice support is disabled"));
        }
        if !self.stt_ready() {
            return Err(anyhow!(
                "Discord voice requires [stt] enabled = true and a configured API key"
            ));
        }
        if !self.voice_channel_allowed(voice_channel_id) {
            return Err(anyhow!(
                "voice channel is not in discord.voice.allowed_channels"
            ));
        }

        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let token = VoiceSessionToken {
            guild_id,
            session_id,
        };
        let transcript = Arc::new(Mutex::new(
            TranscriptStore::new(MAX_TRANSCRIPT_ENTRIES, self.config.max_transcript_bytes)
                .context("invalid Discord voice transcript limits")?,
        ));
        let pending_segments = Arc::new(AtomicU64::new(0));
        let stt_failures = Arc::new(AtomicU64::new(0));
        let drops = Arc::new(VoiceDropCounters::default());
        let (segment_tx, segment_rx) = mpsc::channel(self.config.max_pending_segments);
        let session = VoiceSession {
            id: session_id,
            voice_channel_id,
            control_channel_id,
            state: VoiceConnectionState::Connecting,
            started_at: Instant::now(),
            ended_at: None,
            session_frames: 0,
            ssrc_to_user: HashMap::new(),
            user_to_ssrc: HashMap::new(),
            captures: HashMap::new(),
            transcript: transcript.clone(),
            segment_tx: Some(segment_tx),
            pending_segments: pending_segments.clone(),
            stt_failures: stt_failures.clone(),
            drops: drops.clone(),
            ignored_speakers: 0,
            capture_suppressed: false,
            playback_epoch: 0,
        };

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = sessions.get(&guild_id) {
            if matches!(
                existing.state,
                VoiceConnectionState::Connecting
                    | VoiceConnectionState::Listening
                    | VoiceConnectionState::Stopping
            ) {
                return Err(anyhow!(
                    "a Discord voice session is already active in this guild"
                ));
            }
            if existing.pending_segments.load(Ordering::Relaxed) > 0 {
                return Err(anyhow!(
                    "the previous Discord voice session still has pending STT work"
                ));
            }
        }
        let replaced_token = sessions
            .insert(guild_id, session)
            .map(|replaced| VoiceSessionToken {
                guild_id,
                session_id: replaced.id,
            });
        drop(sessions);

        if let Some(replaced_token) = replaced_token {
            self.abandon_intent_session(replaced_token);
        }

        let intent = self.intent_broker.get().cloned().map(|broker| {
            let (tx, rx) = mpsc::channel(self.config.max_pending_segments);
            drop(spawn_intent_worker(rx, broker, Arc::downgrade(self)));
            IntentWorkerContext {
                tx,
                token,
                control_channel_id,
            }
        });

        spawn_stt_worker(
            segment_rx,
            transcript,
            pending_segments,
            stt_failures,
            drops,
            self.stt_config.clone(),
            intent,
        );

        let manager = Arc::downgrade(self);
        let timeout = Duration::from_secs(self.config.max_session_minutes.saturating_mul(60));
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(manager) = manager.upgrade() {
                manager.expire_session(token).await;
            }
        });
        Ok(token)
    }

    pub fn mark_listening(&self, token: VoiceSessionToken) -> bool {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&token.guild_id)
        {
            if session.id == token.session_id && session.state == VoiceConnectionState::Connecting {
                session.state = VoiceConnectionState::Listening;
                return true;
            }
        }
        false
    }

    pub fn note_reconnected(&self, token: VoiceSessionToken, channel_id: u64) -> bool {
        let channel_allowed = self.voice_channel_allowed(channel_id);
        let (reconnected, abandon_intent) = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.get_mut(&token.guild_id) else {
                return false;
            };
            if session.id != token.session_id {
                return false;
            }
            if channel_id != session.voice_channel_id || !channel_allowed {
                flush_all_captures(session);
                session.state = VoiceConnectionState::Failed;
                session.ended_at.get_or_insert_with(Instant::now);
                session.segment_tx.take();
                (false, true)
            } else {
                if session.state == VoiceConnectionState::Listening {
                    flush_all_captures(session);
                    session.captures.clear();
                    session.ssrc_to_user.clear();
                    session.user_to_ssrc.clear();
                }
                (
                    matches!(
                        session.state,
                        VoiceConnectionState::Connecting | VoiceConnectionState::Listening
                    ),
                    false,
                )
            }
        };
        if abandon_intent {
            self.abandon_intent_session(token);
        }
        reconnected
    }

    pub fn mark_driver_failed(&self, token: VoiceSessionToken) {
        let abandon_intent = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.get_mut(&token.guild_id) else {
                return;
            };
            if session.id == token.session_id
                && matches!(
                    session.state,
                    VoiceConnectionState::Connecting | VoiceConnectionState::Listening
                )
            {
                flush_all_captures(session);
                session.state = VoiceConnectionState::Failed;
                session.ended_at.get_or_insert_with(Instant::now);
                session.segment_tx.take();
                true
            } else {
                false
            }
        };
        if abandon_intent {
            self.abandon_intent_session(token);
        }
    }

    pub fn discard_session(&self, token: VoiceSessionToken) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let removed = if sessions
            .get(&token.guild_id)
            .is_some_and(|session| session.id == token.session_id)
        {
            sessions.remove(&token.guild_id);
            true
        } else {
            false
        };
        drop(sessions);
        if removed {
            self.abandon_intent_session(token);
        }
    }

    pub fn start_stopping(&self, guild_id: u64) -> Result<(VoiceSessionToken, VoiceSessionStatus)> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(&guild_id)
            .ok_or_else(|| anyhow!("no Discord voice session exists in this guild"))?;
        let token = VoiceSessionToken {
            guild_id,
            session_id: session.id,
        };
        let status = if session.state == VoiceConnectionState::Stopping {
            status_snapshot(guild_id, session)
        } else {
            if !matches!(
                session.state,
                VoiceConnectionState::Connecting
                    | VoiceConnectionState::Listening
                    | VoiceConnectionState::Failed
            ) {
                return Err(anyhow!("the Discord voice session is not active"));
            }
            flush_all_captures(session);
            session.state = VoiceConnectionState::Stopping;
            session.ended_at.get_or_insert_with(Instant::now);
            session.segment_tx.take();
            status_snapshot(guild_id, session)
        };
        drop(sessions);
        self.abandon_intent_session(token);
        Ok((token, status))
    }

    pub fn start_stopping_token(&self, token: VoiceSessionToken) -> Result<VoiceSessionStatus> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(&token.guild_id)
            .ok_or_else(|| anyhow!("the Discord voice session no longer exists"))?;
        if session.id != token.session_id {
            return Err(anyhow!("the Discord voice session was replaced"));
        }
        let status = if session.state == VoiceConnectionState::Stopping {
            status_snapshot(token.guild_id, session)
        } else {
            if !matches!(
                session.state,
                VoiceConnectionState::Connecting
                    | VoiceConnectionState::Listening
                    | VoiceConnectionState::Failed
            ) {
                return Err(anyhow!("the Discord voice session is not active"));
            }
            flush_all_captures(session);
            session.state = VoiceConnectionState::Stopping;
            session.ended_at.get_or_insert_with(Instant::now);
            session.segment_tx.take();
            status_snapshot(token.guild_id, session)
        };
        drop(sessions);
        self.abandon_intent_session(token);
        Ok(status)
    }

    pub fn finish_stop(&self, token: VoiceSessionToken) -> Option<VoiceSessionStatus> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get_mut(&token.guild_id)?;
        if session.id != token.session_id || session.state != VoiceConnectionState::Stopping {
            return None;
        }
        session.state = VoiceConnectionState::Stopped;
        Some(status_snapshot(token.guild_id, session))
    }

    fn finish_failed(&self, token: VoiceSessionToken) -> Option<VoiceSessionStatus> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get_mut(&token.guild_id)?;
        if session.id != token.session_id || session.state != VoiceConnectionState::Stopping {
            return None;
        }
        session.state = VoiceConnectionState::Failed;
        Some(status_snapshot(token.guild_id, session))
    }

    pub fn status(&self, guild_id: u64) -> Option<VoiceSessionStatus> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&guild_id)
            .map(|session| status_snapshot(guild_id, session))
    }

    pub fn session_token(&self, guild_id: u64) -> Option<VoiceSessionToken> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&guild_id)
            .map(|session| VoiceSessionToken {
                guild_id,
                session_id: session.id,
            })
    }

    fn status_for(&self, token: VoiceSessionToken) -> Option<VoiceSessionStatus> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&token.guild_id)
            .filter(|session| session.id == token.session_id)
            .map(|session| status_snapshot(token.guild_id, session))
    }

    pub fn render_transcript(&self, guild_id: u64) -> Result<String> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get(&guild_id)
            .ok_or_else(|| anyhow!("no Discord voice session exists in this guild"))?;
        let transcript = session.transcript.lock().unwrap_or_else(|e| e.into_inner());
        if transcript.is_empty() {
            return Err(anyhow!(
                "the Discord voice session has no completed transcript entries yet"
            ));
        }

        let mut entries: Vec<_> = transcript.entries().cloned().collect();
        entries.sort_by_key(|entry| (entry.start_frame, entry.end_frame, entry.speaker_id));
        let stats = transcript.stats();
        let mut output = if stats.evicted_entries > 0 {
            format!(
                "[OpenAB retained a bounded transcript window; {} earlier entries were evicted.]\n",
                stats.evicted_entries
            )
        } else {
            String::new()
        };
        for entry in &entries {
            let timestamp = format_timestamp(entry.start_millis());
            output.push_str(&format!(
                "[{timestamp}] {}: {}\n",
                entry.speaker_name, entry.text
            ));
        }
        Ok(output)
    }

    pub async fn wait_for_drain(&self, token: VoiceSessionToken, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let pending = self
                .status_for(token)
                .map(|status| status.pending_segments)
                .unwrap_or(0);
            if pending == 0 {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn shutdown_all(&self) {
        let guild_ids: Vec<u64> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        for guild_id in guild_ids {
            let token = self.start_stopping(guild_id).ok().map(|(token, _)| token);
            if let Some(songbird) = self.songbird.get() {
                let _ = songbird
                    .remove(serenity::model::id::GuildId::new(guild_id))
                    .await;
            }
            if let Some(token) = token {
                self.finish_stop(token);
            }
        }
    }

    async fn speak(self: &Arc<Self>, token: VoiceSessionToken, text: &str) -> Result<bool> {
        if !self.tts_config.enabled || self.tts_config.api_key.trim().is_empty() {
            return Ok(false);
        }
        if self
            .status_for(token)
            .is_none_or(|status| !status.is_active())
        {
            return Ok(false);
        }

        let wav = crate::tts::synthesize_wav(&self.tts_client, &self.tts_config, text).await?;
        let audio = DiscordVoiceSpeechAudio::from_wav(&wav)?;
        let duration = audio.duration();
        let Some(epoch) = self.begin_playback(token) else {
            return Ok(false);
        };
        let Some(songbird) = self.songbird.get() else {
            self.finish_playback(token, epoch);
            return Err(anyhow!(
                "Songbird is not attached to the Discord voice manager"
            ));
        };
        let guild_id = serenity::model::id::GuildId::new(token.guild_id);
        let Some(call) = songbird.get(guild_id) else {
            self.finish_playback(token, epoch);
            return Ok(false);
        };

        let track = {
            let mut call = call.lock().await;
            call.play_only_input(audio.into_songbird_input())
        };
        let release = VoicePlaybackFinished {
            manager: Arc::downgrade(self),
            token,
            epoch,
        };
        if let Err(err) = track.add_event(
            Event::Track(songbird::events::TrackEvent::End),
            release.clone(),
        ) {
            self.finish_playback(token, epoch);
            return Err(anyhow!("failed to watch Discord TTS playback end: {err}"));
        }
        if let Err(err) =
            track.add_event(Event::Track(songbird::events::TrackEvent::Error), release)
        {
            self.finish_playback(token, epoch);
            return Err(anyhow!(
                "failed to watch Discord TTS playback errors: {err}"
            ));
        }

        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(duration + TTS_WATCHDOG_GRACE + TTS_POST_ROLL).await;
            if let Some(manager) = manager.upgrade() {
                manager.finish_playback(token, epoch);
            }
        });
        info!(
            guild_id = token.guild_id,
            playback_epoch = epoch,
            duration_ms = duration.as_millis(),
            "started Discord voice TTS playback"
        );
        Ok(true)
    }

    pub(crate) async fn speak_brief(
        self: &Arc<Self>,
        token: VoiceSessionToken,
        brief: &str,
    ) -> Result<bool> {
        self.speak(token, brief).await
    }

    fn begin_playback(&self, token: VoiceSessionToken) -> Option<u64> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get_mut(&token.guild_id)?;
        if session.id != token.session_id || session.state != VoiceConnectionState::Listening {
            return None;
        }
        session.playback_epoch = session.playback_epoch.saturating_add(1);
        session.capture_suppressed = true;
        // Discard partial capture instead of enqueuing it: it may contain the
        // start of the bot's own playback through an operator's speakers.
        session.captures.clear();
        Some(session.playback_epoch)
    }

    fn finish_playback(&self, token: VoiceSessionToken, epoch: u64) -> bool {
        let finished = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.get_mut(&token.guild_id) else {
                return false;
            };
            if session.id != token.session_id || session.playback_epoch != epoch {
                return false;
            }
            session.capture_suppressed = false;
            session.captures.clear();
            true
        };
        if finished {
            if let Some(broker) = self.intent_broker.get() {
                broker.refresh_confirmation_timeout(token);
            }
        }
        finished
    }

    async fn expire_session(&self, token: VoiceSessionToken) {
        let should_remove = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.get_mut(&token.guild_id) else {
                return;
            };
            if session.id != token.session_id {
                return;
            }
            if !matches!(
                session.state,
                VoiceConnectionState::Connecting | VoiceConnectionState::Listening
            ) {
                return;
            }
            flush_all_captures(session);
            session.state = VoiceConnectionState::Stopping;
            session.ended_at.get_or_insert_with(Instant::now);
            session.segment_tx.take();
            true
        };
        if should_remove {
            self.abandon_intent_session(token);
            warn!(
                guild_id = token.guild_id,
                "Discord voice session reached its maximum duration"
            );
            if let Some(songbird) = self.songbird.get() {
                if let Err(err) = songbird
                    .remove(serenity::model::id::GuildId::new(token.guild_id))
                    .await
                {
                    error!(
                        guild_id = token.guild_id,
                        error = %err,
                        "failed to leave expired Discord voice session; `/voice stop` can retry"
                    );
                    return;
                }
            }
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = sessions.get_mut(&token.guild_id) {
                if session.id == token.session_id && session.state == VoiceConnectionState::Stopping
                {
                    session.state = VoiceConnectionState::Expired;
                }
            }
        }
    }

    fn note_speaking(&self, token: VoiceSessionToken, ssrc: u32, user_id: u64) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = sessions.get_mut(&token.guild_id) else {
            return;
        };
        if session.id != token.session_id
            || !matches!(
                session.state,
                VoiceConnectionState::Connecting | VoiceConnectionState::Listening
            )
        {
            return;
        }
        if !session.user_to_ssrc.contains_key(&user_id)
            && session.user_to_ssrc.len() >= MAX_TRACKED_SPEAKERS
        {
            session.ignored_speakers = session.ignored_speakers.saturating_add(1);
            warn!(
                guild_id = token.guild_id,
                user_id,
                max_speakers = MAX_TRACKED_SPEAKERS,
                "ignoring Discord voice speaker because the capture limit was reached"
            );
            return;
        }

        if let Some(previous_user) = session.ssrc_to_user.insert(ssrc, user_id) {
            if previous_user != user_id && session.user_to_ssrc.get(&previous_user) == Some(&ssrc) {
                session.user_to_ssrc.remove(&previous_user);
            }
        }
        if let Some(previous_ssrc) = session.user_to_ssrc.insert(user_id, ssrc) {
            if previous_ssrc != ssrc && session.ssrc_to_user.get(&previous_ssrc) == Some(&user_id) {
                session.ssrc_to_user.remove(&previous_ssrc);
            }
        }
        session
            .captures
            .entry(user_id)
            .or_insert_with(|| SpeakerCapture {
                base_frame: session.session_frames,
                buffer: SpeakerPcmBuffer::new(self.capture_config),
            });
    }

    fn note_disconnect(&self, token: VoiceSessionToken, user_id: u64) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = sessions.get_mut(&token.guild_id) else {
            return;
        };
        if session.id != token.session_id {
            return;
        }
        if let Some(mut capture) = session.captures.remove(&user_id) {
            if let Some(segment) = capture.buffer.flush() {
                enqueue_segment(session, user_id, capture.base_frame, segment);
            }
        }
        if let Some(ssrc) = session.user_to_ssrc.remove(&user_id) {
            session.ssrc_to_user.remove(&ssrc);
        }
    }

    fn handle_tick(
        &self,
        token: VoiceSessionToken,
        tick: &songbird::events::context_data::VoiceTick,
    ) {
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.get_mut(&token.guild_id) else {
                return;
            };
            if session.id != token.session_id || session.state != VoiceConnectionState::Listening {
                return;
            }
            session.session_frames = session
                .session_frames
                .saturating_add(u64::try_from(VOICE_TICK_FRAMES).unwrap_or(u64::MAX));
            if session.capture_suppressed {
                session.captures.clear();
                return;
            }

            for (ssrc, voice) in &tick.speaking {
                let Some(user_id) = session.ssrc_to_user.get(ssrc).copied() else {
                    continue;
                };
                let Some(samples) = voice.decoded_voice.as_deref() else {
                    continue;
                };
                let tick_start = session
                    .session_frames
                    .saturating_sub(u64::try_from(VOICE_TICK_FRAMES).unwrap_or(u64::MAX));
                let (base_frame, result) = {
                    let capture =
                        session
                            .captures
                            .entry(user_id)
                            .or_insert_with(|| SpeakerCapture {
                                base_frame: tick_start,
                                buffer: SpeakerPcmBuffer::new(self.capture_config),
                            });
                    (
                        capture.base_frame,
                        capture.buffer.push_interleaved_stereo(samples),
                    )
                };
                match result {
                    Ok(segments) => {
                        for segment in segments {
                            enqueue_segment(session, user_id, base_frame, segment);
                        }
                    }
                    Err(err) => {
                        warn!(guild_id = token.guild_id, user_id, error = %err, "invalid Discord voice PCM")
                    }
                }
            }

            for ssrc in &tick.silent {
                let Some(user_id) = session.ssrc_to_user.get(ssrc).copied() else {
                    continue;
                };
                let tick_start = session
                    .session_frames
                    .saturating_sub(u64::try_from(VOICE_TICK_FRAMES).unwrap_or(u64::MAX));
                let (base_frame, segment) = {
                    let capture =
                        session
                            .captures
                            .entry(user_id)
                            .or_insert_with(|| SpeakerCapture {
                                base_frame: tick_start,
                                buffer: SpeakerPcmBuffer::new(self.capture_config),
                            });
                    (
                        capture.base_frame,
                        capture.buffer.push_silence_frames(VOICE_TICK_FRAMES),
                    )
                };
                if let Some(segment) = segment {
                    enqueue_segment(session, user_id, base_frame, segment);
                }
            }
        }
    }
}

fn enqueue_segment(
    session: &mut VoiceSession,
    speaker_id: u64,
    base_frame: u64,
    segment: PcmSegment,
) {
    let Some(tx) = session.segment_tx.as_ref() else {
        session.drops.record_segment();
        return;
    };
    let (start_frame, end_frame) = session_segment_range(base_frame, &segment);
    let job = SegmentJob {
        speaker_id,
        start_frame,
        end_frame,
        segment,
    };
    session.pending_segments.fetch_add(1, Ordering::Relaxed);
    if tx.try_send(job).is_err() {
        session.pending_segments.fetch_sub(1, Ordering::Relaxed);
        session.drops.record_segment();
    }
}

fn session_segment_range(base_frame: u64, segment: &PcmSegment) -> (u64, u64) {
    (
        base_frame.saturating_add(segment.start_frame),
        base_frame.saturating_add(segment.end_frame),
    )
}

fn flush_all_captures(session: &mut VoiceSession) {
    let segments: Vec<_> = session
        .captures
        .iter_mut()
        .filter_map(|(user_id, capture)| {
            capture
                .buffer
                .flush()
                .map(|segment| (*user_id, capture.base_frame, segment))
        })
        .collect();
    for (user_id, base_frame, segment) in segments {
        enqueue_segment(session, user_id, base_frame, segment);
    }
}

fn spawn_stt_worker(
    mut rx: mpsc::Receiver<SegmentJob>,
    transcript: Arc<Mutex<TranscriptStore>>,
    pending_segments: Arc<AtomicU64>,
    stt_failures: Arc<AtomicU64>,
    drops: Arc<VoiceDropCounters>,
    stt_config: SttConfig,
    intent: Option<IntentWorkerContext>,
) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        while let Some(job) = rx.recv().await {
            let SegmentJob {
                speaker_id,
                start_frame,
                end_frame,
                segment,
            } = job;
            let wav = segment.to_wav_bytes();
            drop(segment);
            let result = match wav {
                Ok(wav) => match tokio::time::timeout(
                    STT_REQUEST_TIMEOUT,
                    crate::stt::transcribe(
                        &client,
                        &stt_config,
                        wav,
                        "discord-voice-segment.wav".into(),
                        "audio/wav",
                    ),
                )
                .await
                {
                    Ok(transcript) => transcript,
                    Err(_) => {
                        warn!(
                            speaker_id,
                            timeout_seconds = STT_REQUEST_TIMEOUT.as_secs(),
                            "Discord voice STT request timed out"
                        );
                        None
                    }
                },
                Err(err) => {
                    error!(speaker_id, error = %err, "failed to encode Discord voice WAV");
                    None
                }
            };

            complete_segment_job(
                &transcript,
                &pending_segments,
                &stt_failures,
                &drops,
                intent.as_ref(),
                CompletedSegment {
                    speaker_id,
                    start_frame,
                    end_frame,
                    transcript: result,
                },
            );
        }
        debug!("Discord voice STT worker stopped");
    });
}

fn complete_segment_job(
    transcript: &Mutex<TranscriptStore>,
    pending_segments: &AtomicU64,
    stt_failures: &AtomicU64,
    drops: &VoiceDropCounters,
    intent: Option<&IntentWorkerContext>,
    completed: CompletedSegment,
) {
    if let Some(text) = completed.transcript {
        let entry = TranscriptEntry {
            speaker_id: completed.speaker_id,
            speaker_name: format!("<@{}>", completed.speaker_id),
            start_frame: completed.start_frame,
            end_frame: completed.end_frame,
            text,
        };
        let intent_text = intent.map(|_| entry.text.clone());
        let outcome = {
            let mut transcript = transcript.lock().unwrap_or_else(|e| e.into_inner());
            transcript.push(entry)
        };
        if !outcome.accepted {
            drops.record_transcript();
        } else if let (Some(intent), Some(text)) = (intent, intent_text) {
            let event = FinalTranscriptEvent {
                token: intent.token,
                control_channel_id: intent.control_channel_id,
                key: TranscriptKey {
                    speaker_id: completed.speaker_id,
                    start_frame: completed.start_frame,
                    end_frame: completed.end_frame,
                },
                text,
            };
            if let Err(err) = intent.tx.try_send(event) {
                warn!(
                    guild_id = intent.token.guild_id,
                    speaker_id = completed.speaker_id,
                    error = %err,
                    "dropping Discord voice intent event because its bounded queue is unavailable"
                );
            }
        }
    } else {
        stt_failures.fetch_add(1, Ordering::Relaxed);
    }
    pending_segments.fetch_sub(1, Ordering::Relaxed);
}

fn spawn_intent_worker(
    mut rx: mpsc::Receiver<FinalTranscriptEvent>,
    broker: Arc<DiscordVoiceIntentBroker>,
    manager: std::sync::Weak<DiscordVoiceManager>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let guild_id = event.token.guild_id;
            let speaker_id = event.key.speaker_id;
            let token = event.token;
            match broker.handle_final_transcript(event).await {
                Ok(outcome) => {
                    let Some(manager) = manager.upgrade() else {
                        break;
                    };
                    handle_voice_intent_outcome(&manager, &broker, token, outcome).await;
                }
                Err(err) => {
                    warn!(
                        guild_id,
                        speaker_id,
                        error = %err,
                        "Discord voice intent broker failed to handle a final transcript event"
                    );
                }
            }
        }
        debug!("Discord voice intent worker stopped");
    })
}

async fn handle_voice_intent_outcome(
    manager: &Arc<DiscordVoiceManager>,
    broker: &Arc<DiscordVoiceIntentBroker>,
    token: VoiceSessionToken,
    outcome: VoiceIntentTranscriptOutcome,
) {
    let feedback = match outcome {
        VoiceIntentTranscriptOutcome::Proposed { speech_prompt } => Some(speech_prompt),
        VoiceIntentTranscriptOutcome::Confirmation(confirmation) => {
            let mut feedback = confirmation.speech_feedback;
            if let VoiceIntentTextOutcome::ExecuteLocal(request) = confirmation.outcome {
                feedback = match manager.intent_action_executor.get() {
                    Some(executor) => match executor.execute(&request).await {
                        DiscordVoiceIntentActionOutcome::Queued { .. } => feedback,
                        DiscordVoiceIntentActionOutcome::Failed {
                            confirmation_reopened,
                            ..
                        } => Some(if confirmation_reopened {
                            "沒有成功開始。你可以再說一次對，或更正指令。".to_string()
                        } else {
                            "沒有成功開始。請重新說出指令。".to_string()
                        }),
                    },
                    None => {
                        let reopened = broker.reopen_local_execution(&request);
                        warn!(
                            intent_id = %request.intent_id,
                            revision = request.revision,
                            reopened,
                            "spoken local voice intent has no action executor"
                        );
                        Some(if reopened {
                            "目前無法開始。你可以再說一次對，或更正指令。".to_string()
                        } else {
                            "目前無法開始。請重新說出指令。".to_string()
                        })
                    }
                };
            }
            feedback
        }
        VoiceIntentTranscriptOutcome::Ignored
        | VoiceIntentTranscriptOutcome::AwaitingConfirmation => None,
    };

    if let Some(feedback) = feedback {
        match manager.speak(token, &feedback).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(
                    guild_id = token.guild_id,
                    "Discord voice TTS feedback was skipped; text fallback remains available"
                );
            }
            Err(err) => {
                warn!(
                    guild_id = token.guild_id,
                    error = %err,
                    "failed to synthesize or play Discord voice intent feedback"
                );
            }
        }
    }
}

fn status_snapshot(guild_id: u64, session: &VoiceSession) -> VoiceSessionStatus {
    let transcript = session.transcript.lock().unwrap_or_else(|e| e.into_inner());
    VoiceSessionStatus {
        guild_id,
        voice_channel_id: session.voice_channel_id,
        control_channel_id: session.control_channel_id,
        state: session.state,
        elapsed: session
            .ended_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(session.started_at),
        transcript_entries: transcript.len(),
        transcript_bytes: transcript.text_bytes(),
        tracked_speakers: session.ssrc_to_user.len(),
        ignored_speakers: session.ignored_speakers,
        evicted_transcript_entries: transcript.stats().evicted_entries,
        rejected_transcript_entries: transcript.stats().rejected_entries,
        pending_segments: session.pending_segments.load(Ordering::Relaxed),
        stt_failures: session.stt_failures.load(Ordering::Relaxed),
        drops: session.drops.snapshot(),
    }
}

fn format_timestamp(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Releases half-duplex capture suppression for one exact playback generation.
#[derive(Clone)]
struct VoicePlaybackFinished {
    manager: std::sync::Weak<DiscordVoiceManager>,
    token: VoiceSessionToken,
    epoch: u64,
}

#[async_trait]
impl VoiceEventHandler for VoicePlaybackFinished {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        let manager = self.manager.clone();
        let token = self.token;
        let epoch = self.epoch;
        tokio::spawn(async move {
            tokio::time::sleep(TTS_POST_ROLL).await;
            if let Some(manager) = manager.upgrade() {
                manager.finish_playback(token, epoch);
            }
        });
        None
    }
}

/// Songbird global event handler for one exact voice session. The session token
/// prevents delayed events from a previous call from mutating its replacement.
#[derive(Clone)]
pub struct DiscordVoiceReceiver {
    token: VoiceSessionToken,
    manager: Arc<DiscordVoiceManager>,
}

impl DiscordVoiceReceiver {
    pub fn new(token: VoiceSessionToken, manager: Arc<DiscordVoiceManager>) -> Self {
        Self { token, manager }
    }

    pub fn install_on(&self, call: &mut songbird::Call) {
        call.remove_all_global_events();
        call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), self.clone());
        call.add_global_event(CoreEvent::VoiceTick.into(), self.clone());
        call.add_global_event(CoreEvent::ClientDisconnect.into(), self.clone());
        call.add_global_event(CoreEvent::DriverReconnect.into(), self.clone());
        call.add_global_event(CoreEvent::DriverDisconnect.into(), self.clone());
    }
}

#[async_trait]
impl VoiceEventHandler for DiscordVoiceReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(Speaking {
                ssrc,
                user_id: Some(user_id),
                ..
            }) => {
                self.manager.note_speaking(self.token, *ssrc, user_id.0);
            }
            EventContext::VoiceTick(tick) => {
                self.manager.handle_tick(self.token, tick);
            }
            EventContext::ClientDisconnect(ClientDisconnect { user_id, .. }) => {
                self.manager.note_disconnect(self.token, user_id.0);
            }
            EventContext::DriverReconnect(connection) => {
                let channel_id = connection.channel_id.0.get();
                if self.manager.note_reconnected(self.token, channel_id) {
                    info!(
                        guild_id = self.token.guild_id,
                        channel_id, "Discord voice driver reconnected"
                    );
                } else {
                    warn!(
                        guild_id = self.token.guild_id,
                        channel_id,
                        "Discord voice reconnected to an unexpected or disallowed channel"
                    );
                    if self.manager.start_stopping_token(self.token).is_ok() {
                        if let Some(songbird) = self.manager.songbird.get() {
                            match songbird
                                .remove(serenity::model::id::GuildId::new(self.token.guild_id))
                                .await
                            {
                                Ok(()) => {
                                    self.manager.finish_failed(self.token);
                                }
                                Err(err) => {
                                    error!(
                                        guild_id = self.token.guild_id,
                                        error = %err,
                                        "failed to leave unexpected Discord voice channel; `/voice stop` can retry"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            EventContext::DriverDisconnect(disconnect) => {
                warn!(
                    guild_id = self.token.guild_id,
                    kind = ?disconnect.kind,
                    reason = ?disconnect.reason,
                    "Discord voice driver disconnected after exhausting recovery"
                );
                self.manager.mark_driver_failed(self.token);
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DiscordVoiceIntentConfig, DiscordVoiceIntentTargetConfig};
    use crate::discord_voice_intent::VoiceIntentMessenger;
    use std::collections::BTreeMap;
    use tokio::sync::Notify;

    struct NoopIntentMessenger;

    #[async_trait]
    impl VoiceIntentMessenger for NoopIntentMessenger {
        async fn send_message(
            &self,
            _channel_id: u64,
            _content: &str,
            _nonce: Option<&str>,
        ) -> Result<String> {
            Ok("noop-message".to_string())
        }

        async fn delete_message(&self, _channel_id: u64, _message_id: &str) -> Result<()> {
            Ok(())
        }
    }

    struct BlockingIntentMessenger {
        entered: Notify,
        release: Notify,
    }

    #[async_trait]
    impl VoiceIntentMessenger for BlockingIntentMessenger {
        async fn send_message(
            &self,
            _channel_id: u64,
            _content: &str,
            _nonce: Option<&str>,
        ) -> Result<String> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok("blocking-message".to_string())
        }

        async fn delete_message(&self, _channel_id: u64, _message_id: &str) -> Result<()> {
            Ok(())
        }
    }

    fn voice_manager_with_intent() -> (Arc<DiscordVoiceManager>, Arc<DiscordVoiceIntentBroker>) {
        let voice = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(voice, stt).unwrap();
        let intent = DiscordVoiceIntentConfig {
            enabled: true,
            confirmation_timeout_seconds: 30,
            default_to_local: false,
            targets: BTreeMap::from([(
                "B0".to_string(),
                DiscordVoiceIntentTargetConfig {
                    discord_user_id: "9000".to_string(),
                    aliases: Vec::new(),
                },
            )]),
        };
        let broker = DiscordVoiceIntentBroker::new(intent, Arc::new(NoopIntentMessenger)).unwrap();
        manager.attach_intent_broker(broker.clone());
        (manager, broker)
    }

    #[test]
    fn voice_manager_defaults_are_backward_compatible() {
        let manager =
            DiscordVoiceManager::new(DiscordVoiceConfig::default(), SttConfig::default()).unwrap();
        assert!(!manager.enabled());
        assert!(!manager.stt_ready());
        assert!(!manager.tts_ready());
        assert!(manager.voice_channel_allowed(123));
    }

    #[test]
    fn tts_readiness_requires_opt_in_and_a_key() {
        let tts = TtsConfig {
            enabled: true,
            api_key: "test".into(),
            ..TtsConfig::default()
        };
        let manager = DiscordVoiceManager::new_with_tts(
            DiscordVoiceConfig::default(),
            SttConfig::default(),
            tts,
        )
        .unwrap();
        assert!(manager.tts_ready());
    }

    #[tokio::test]
    async fn playback_suppression_is_session_and_epoch_bound() {
        let voice = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(voice, stt).unwrap();
        let token = manager.begin_session(1, 2, 3).unwrap();
        assert!(manager.mark_listening(token));
        manager.note_speaking(token, 10, 100);

        let first = manager.begin_playback(token).unwrap();
        let second = manager.begin_playback(token).unwrap();
        assert!(!manager.finish_playback(token, first));
        {
            let sessions = manager.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions.get(&1).unwrap();
            assert!(session.capture_suppressed);
            assert!(session.captures.is_empty());
        }
        assert!(manager.finish_playback(token, second));
        assert!(
            !manager
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&1)
                .unwrap()
                .capture_suppressed
        );

        let replacement = VoiceSessionToken::for_test(1, token.session_id.saturating_add(1));
        assert!(!manager.finish_playback(replacement, second));
    }

    #[test]
    fn allowed_voice_channels_are_enforced() {
        let config = DiscordVoiceConfig {
            enabled: true,
            allowed_channels: vec!["123".into()],
            ..DiscordVoiceConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, SttConfig::default()).unwrap();
        assert!(manager.voice_channel_allowed(123));
        assert!(!manager.voice_channel_allowed(456));
    }

    #[test]
    fn invalid_voice_channel_id_is_rejected() {
        let config = DiscordVoiceConfig {
            allowed_channels: vec!["not-an-id".into()],
            ..DiscordVoiceConfig::default()
        };
        assert!(DiscordVoiceManager::new(config, SttConfig::default()).is_err());
    }

    #[test]
    fn timestamp_format_is_stable() {
        assert_eq!(format_timestamp(0), "00:00:00.000");
        assert_eq!(format_timestamp(3_661_999), "01:01:01.999");
    }

    #[test]
    fn speaker_local_segment_offsets_are_preserved_on_the_session_timeline() {
        let segment = PcmSegment {
            start_frame: 960,
            end_frame: 1_920,
            interleaved_samples: vec![1; 1_920],
            boundary: crate::discord_voice::SegmentBoundary::Silence,
        };
        assert_eq!(session_segment_range(48_000, &segment), (48_960, 49_920));
    }

    #[tokio::test]
    async fn stale_timeout_cannot_expire_a_replacement_session() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let old = manager.begin_session(1, 2, 3).unwrap();
        let (stopping, _) = manager.start_stopping(1).unwrap();
        assert_eq!(old, stopping);
        manager.finish_stop(old).unwrap();
        let replacement = manager.begin_session(1, 2, 3).unwrap();

        manager.expire_session(old).await;
        assert!(manager.status(1).unwrap().is_active());
        assert_eq!(manager.session_token(1), Some(replacement));
    }

    #[tokio::test]
    async fn terminal_voice_lifecycle_abandons_the_exact_intent_session() {
        let (manager, broker) = voice_manager_with_intent();
        let stopped = manager.begin_session(1, 2, 3).unwrap();
        assert!(broker.bind_session(stopped, 3, 4, "Can"));
        manager.start_stopping_token(stopped).unwrap();
        assert!(!broker.abandon_session(stopped));

        let (manager, broker) = voice_manager_with_intent();
        let failed = manager.begin_session(1, 2, 3).unwrap();
        assert!(broker.bind_session(failed, 3, 4, "Can"));
        manager.mark_driver_failed(failed);
        assert!(!broker.abandon_session(failed));

        let (manager, broker) = voice_manager_with_intent();
        let reconnected_wrong = manager.begin_session(1, 2, 3).unwrap();
        assert!(broker.bind_session(reconnected_wrong, 3, 4, "Can"));
        assert!(!manager.note_reconnected(reconnected_wrong, 9));
        assert!(!broker.abandon_session(reconnected_wrong));

        let (manager, broker) = voice_manager_with_intent();
        let discarded = manager.begin_session(1, 2, 3).unwrap();
        assert!(broker.bind_session(discarded, 3, 4, "Can"));
        manager.discard_session(discarded);
        assert!(!broker.abandon_session(discarded));

        let (manager, broker) = voice_manager_with_intent();
        let expired = manager.begin_session(1, 2, 3).unwrap();
        assert!(broker.bind_session(expired, 3, 4, "Can"));
        manager.expire_session(expired).await;
        assert!(!broker.abandon_session(expired));
    }

    #[tokio::test]
    async fn slow_intent_delivery_does_not_block_the_transcript_event_producer() {
        let messenger = Arc::new(BlockingIntentMessenger {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let intent = DiscordVoiceIntentConfig {
            enabled: true,
            confirmation_timeout_seconds: 30,
            default_to_local: false,
            targets: BTreeMap::from([(
                "B0".to_string(),
                DiscordVoiceIntentTargetConfig {
                    discord_user_id: "9000".to_string(),
                    aliases: Vec::new(),
                },
            )]),
        };
        let broker = DiscordVoiceIntentBroker::new(intent, messenger.clone()).unwrap();
        let token = VoiceSessionToken::for_test(1, 1);
        assert!(broker.bind_session(token, 3, 4, "Can"));

        let (tx, rx) = mpsc::channel(1);
        let manager =
            DiscordVoiceManager::new(DiscordVoiceConfig::default(), SttConfig::default()).unwrap();
        let worker = spawn_intent_worker(rx, broker, Arc::downgrade(&manager));
        tx.try_send(FinalTranscriptEvent {
            token,
            control_channel_id: 3,
            key: TranscriptKey {
                speaker_id: 4,
                start_frame: 0,
                end_frame: 48_000,
            },
            text: "請 B0 review PR #123".to_string(),
        })
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), messenger.entered.notified())
            .await
            .expect("intent worker should reach the slow Discord messenger");

        // The worker has removed the first event from the queue and is blocked
        // on Discord. The producer can still enqueue the next transcript
        // without awaiting that network request.
        tx.try_send(FinalTranscriptEvent {
            token,
            control_channel_id: 3,
            key: TranscriptKey {
                speaker_id: 4,
                start_frame: 48_000,
                end_frame: 96_000,
            },
            text: "請 B0 run CI".to_string(),
        })
        .expect("slow Discord delivery must not block the transcript producer");

        drop(tx);
        messenger.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("intent worker should stop after its sender closes")
            .expect("intent worker should not panic");
    }

    #[tokio::test]
    async fn full_intent_queue_drops_only_the_broker_event_and_releases_stt_pending() {
        let transcript = Mutex::new(TranscriptStore::new(10, 1_000).unwrap());
        let pending_segments = AtomicU64::new(2);
        let stt_failures = AtomicU64::new(0);
        let drops = VoiceDropCounters::default();
        let (tx, mut rx) = mpsc::channel(1);
        let intent = IntentWorkerContext {
            tx,
            token: VoiceSessionToken::for_test(1, 1),
            control_channel_id: 3,
        };

        for (start_frame, text) in [(0, "請 B0 review"), (48_000, "請 B0 run CI")] {
            complete_segment_job(
                &transcript,
                &pending_segments,
                &stt_failures,
                &drops,
                Some(&intent),
                CompletedSegment {
                    speaker_id: 4,
                    start_frame,
                    end_frame: start_frame + 48_000,
                    transcript: Some(text.to_string()),
                },
            );
        }

        assert_eq!(pending_segments.load(Ordering::Relaxed), 0);
        assert_eq!(stt_failures.load(Ordering::Relaxed), 0);
        assert_eq!(transcript.lock().unwrap().len(), 2);
        assert_eq!(rx.try_recv().unwrap().key.start_frame, 0);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn stale_voice_token_cannot_abandon_replacement_intent_session() {
        let (manager, broker) = voice_manager_with_intent();
        let old = manager.begin_session(1, 2, 3).unwrap();
        manager.start_stopping_token(old).unwrap();
        manager.finish_stop(old).unwrap();

        // Rebinding the terminal generation isolates the replacement hook from
        // the ordinary stop hook exercised above.
        assert!(broker.bind_session(old, 3, 4, "Can"));
        let replacement = manager.begin_session(1, 2, 3).unwrap();
        assert!(!broker.abandon_session(old));
        assert!(broker.bind_session(replacement, 3, 4, "Can"));
        manager.discard_session(old);

        assert!(broker.abandon_session(replacement));
    }

    #[tokio::test]
    async fn terminal_driver_disconnect_is_visible_as_failed() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let token = manager.begin_session(1, 2, 3).unwrap();
        assert!(manager.mark_listening(token));
        manager.mark_driver_failed(token);
        assert_eq!(
            manager.status(1).unwrap().state,
            VoiceConnectionState::Failed
        );
    }

    #[tokio::test]
    async fn reconnect_must_stay_in_the_pinned_allowed_channel() {
        let config = DiscordVoiceConfig {
            enabled: true,
            allowed_channels: vec!["2".into(), "9".into()],
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let token = manager.begin_session(1, 2, 3).unwrap();
        assert!(manager.mark_listening(token));
        manager.note_speaking(token, 10, 100);
        assert!(manager.note_reconnected(token, 2));
        assert_eq!(manager.status(1).unwrap().tracked_speakers, 0);
        assert!(!manager.note_reconnected(token, 9));
        assert_eq!(
            manager.status(1).unwrap().state,
            VoiceConnectionState::Failed
        );
    }

    #[tokio::test]
    async fn every_mapped_participant_gets_a_separate_capture() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let token = manager.begin_session(1, 2, 3).unwrap();

        manager.note_speaking(token, 10, 100);
        manager.note_speaking(token, 20, 200);

        let sessions = manager.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get(&1).unwrap();
        assert_eq!(session.ssrc_to_user.get(&10), Some(&100));
        assert_eq!(session.ssrc_to_user.get(&20), Some(&200));
        assert!(session.captures.contains_key(&100));
        assert!(session.captures.contains_key(&200));
    }

    #[tokio::test]
    async fn speaker_capture_count_is_bounded() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let token = manager.begin_session(1, 2, 3).unwrap();
        for offset in 0..=MAX_TRACKED_SPEAKERS {
            manager.note_speaking(token, offset as u32 + 1, offset as u64 + 100);
        }
        let status = manager.status(1).unwrap();
        assert_eq!(status.tracked_speakers, MAX_TRACKED_SPEAKERS);
        assert_eq!(status.ignored_speakers, 1);
    }

    #[tokio::test]
    async fn stopping_blocks_replacement_and_stale_token_cannot_touch_next_session() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let old = manager.begin_session(1, 2, 3).unwrap();
        assert!(manager.mark_listening(old));
        manager.start_stopping_token(old).unwrap();
        assert!(manager.begin_session(1, 2, 3).is_err());
        manager.finish_stop(old).unwrap();

        let pending = {
            let sessions = manager.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.get(&1).unwrap().pending_segments.clone()
        };
        pending.fetch_add(1, Ordering::Relaxed);
        assert!(manager.begin_session(1, 2, 3).is_err());
        pending.fetch_sub(1, Ordering::Relaxed);

        let replacement = manager.begin_session(1, 2, 3).unwrap();
        assert!(!manager.mark_listening(old));
        manager.discard_session(old);
        assert_eq!(manager.session_token(1), Some(replacement));
    }

    #[tokio::test]
    async fn elapsed_time_freezes_when_capture_starts_stopping() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        let token = manager.begin_session(1, 2, 3).unwrap();
        assert!(manager.mark_listening(token));
        tokio::time::sleep(Duration::from_millis(2)).await;
        manager.start_stopping_token(token).unwrap();
        let stopped_elapsed = manager.status(1).unwrap().elapsed;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(manager.status(1).unwrap().elapsed, stopped_elapsed);
    }

    #[tokio::test]
    async fn transcript_rendering_sorts_out_of_order_stt_results() {
        let config = DiscordVoiceConfig {
            enabled: true,
            ..DiscordVoiceConfig::default()
        };
        let stt = SttConfig {
            enabled: true,
            api_key: "test".into(),
            ..SttConfig::default()
        };
        let manager = DiscordVoiceManager::new(config, stt).unwrap();
        manager.begin_session(1, 2, 3).unwrap();
        let transcript = {
            let sessions = manager.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.get(&1).unwrap().transcript.clone()
        };
        let mut transcript = transcript.lock().unwrap_or_else(|e| e.into_inner());
        transcript.push(TranscriptEntry {
            speaker_id: 2,
            speaker_name: "<@2>".into(),
            start_frame: 48_000,
            end_frame: 96_000,
            text: "second".into(),
        });
        transcript.push(TranscriptEntry {
            speaker_id: 1,
            speaker_name: "<@1>".into(),
            start_frame: 0,
            end_frame: 24_000,
            text: "first".into(),
        });
        drop(transcript);

        let rendered = manager.render_transcript(1).unwrap();
        assert!(rendered.find("first").unwrap() < rendered.find("second").unwrap());
        assert!(rendered.contains("[00:00:00.000]"));
        assert!(rendered.contains("[00:00:01.000]"));
    }
}
