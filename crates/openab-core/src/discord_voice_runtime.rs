//! Discord voice-channel runtime built on Songbird.
//!
//! Songbird callbacks only update in-memory capture state and non-blockingly
//! enqueue completed PCM segments. WAV encoding and STT run in a bounded worker
//! so a slow transcription provider cannot stall Discord's 20 ms receive loop.

use crate::config::{DiscordVoiceConfig, SttConfig};
use crate::discord_voice::{
    PcmCaptureConfig, PcmSegment, SpeakerPcmBuffer, TranscriptEntry, TranscriptStore,
    VoiceDropCounters, VoiceDropSnapshot,
};
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
}

struct SegmentJob {
    speaker_id: u64,
    start_frame: u64,
    end_frame: u64,
    segment: PcmSegment,
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
}

/// Owns at most one voice session per Discord guild.
pub struct DiscordVoiceManager {
    config: DiscordVoiceConfig,
    stt_config: SttConfig,
    capture_config: PcmCaptureConfig,
    allowed_voice_channels: HashSet<u64>,
    sessions: Mutex<HashMap<u64, VoiceSession>>,
    songbird: OnceLock<Arc<songbird::Songbird>>,
    next_session_id: AtomicU64,
}

impl DiscordVoiceManager {
    pub fn new(config: DiscordVoiceConfig, stt_config: SttConfig) -> Result<Arc<Self>> {
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
            capture_config,
            allowed_voice_channels,
            sessions: Mutex::new(HashMap::new()),
            songbird: OnceLock::new(),
            next_session_id: AtomicU64::new(1),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn stt_ready(&self) -> bool {
        self.stt_config.enabled && !self.stt_config.api_key.is_empty()
    }

    pub fn voice_channel_allowed(&self, channel_id: u64) -> bool {
        self.allowed_voice_channels.is_empty() || self.allowed_voice_channels.contains(&channel_id)
    }

    pub fn attach_songbird(&self, songbird: Arc<songbird::Songbird>) {
        let _ = self.songbird.set(songbird);
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
        sessions.insert(guild_id, session);
        drop(sessions);

        spawn_stt_worker(
            segment_rx,
            transcript,
            pending_segments,
            stt_failures,
            drops,
            self.stt_config.clone(),
        );

        let token = VoiceSessionToken {
            guild_id,
            session_id,
        };
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
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&token.guild_id)
        {
            if session.id != token.session_id {
                return false;
            }
            if channel_id != session.voice_channel_id || !channel_allowed {
                flush_all_captures(session);
                session.state = VoiceConnectionState::Failed;
                session.ended_at.get_or_insert_with(Instant::now);
                session.segment_tx.take();
                return false;
            }
            if session.state == VoiceConnectionState::Listening {
                flush_all_captures(session);
                session.captures.clear();
                session.ssrc_to_user.clear();
                session.user_to_ssrc.clear();
            }
            return matches!(
                session.state,
                VoiceConnectionState::Connecting | VoiceConnectionState::Listening
            );
        }
        false
    }

    pub fn mark_driver_failed(&self, token: VoiceSessionToken) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&token.guild_id)
        {
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
            }
        }
    }

    pub fn discard_session(&self, token: VoiceSessionToken) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions
            .get(&token.guild_id)
            .is_some_and(|session| session.id == token.session_id)
        {
            sessions.remove(&token.guild_id);
        }
    }

    pub fn start_stopping(&self, guild_id: u64) -> Result<(VoiceSessionToken, VoiceSessionStatus)> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(&guild_id)
            .ok_or_else(|| anyhow!("no Discord voice session exists in this guild"))?;
        if session.state == VoiceConnectionState::Stopping {
            let token = VoiceSessionToken {
                guild_id,
                session_id: session.id,
            };
            return Ok((token, status_snapshot(guild_id, session)));
        }
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
        let token = VoiceSessionToken {
            guild_id,
            session_id: session.id,
        };
        Ok((token, status_snapshot(guild_id, session)))
    }

    pub fn start_stopping_token(&self, token: VoiceSessionToken) -> Result<VoiceSessionStatus> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(&token.guild_id)
            .ok_or_else(|| anyhow!("the Discord voice session no longer exists"))?;
        if session.id != token.session_id {
            return Err(anyhow!("the Discord voice session was replaced"));
        }
        if session.state == VoiceConnectionState::Stopping {
            return Ok(status_snapshot(token.guild_id, session));
        }
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
        Ok(status_snapshot(token.guild_id, session))
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

            if let Some(text) = result {
                let entry = TranscriptEntry {
                    speaker_id,
                    speaker_name: format!("<@{speaker_id}>"),
                    start_frame,
                    end_frame,
                    text,
                };
                let outcome = transcript
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(entry);
                if !outcome.accepted {
                    drops.record_transcript();
                }
            } else {
                stt_failures.fetch_add(1, Ordering::Relaxed);
            }
            pending_segments.fetch_sub(1, Ordering::Relaxed);
        }
        debug!("Discord voice STT worker stopped");
    });
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

    #[test]
    fn voice_manager_defaults_are_backward_compatible() {
        let manager =
            DiscordVoiceManager::new(DiscordVoiceConfig::default(), SttConfig::default()).unwrap();
        assert!(!manager.enabled());
        assert!(!manager.stt_ready());
        assert!(manager.voice_channel_allowed(123));
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
