//! Pure capture primitives for Discord voice receive.
//!
//! Discord voice is decoded as interleaved, signed 16-bit, 48 kHz stereo PCM.
//! This module deliberately does not depend on Serenity, Songbird, Tokio, or an
//! STT provider. Transport code can therefore keep packet callbacks small: push
//! one speaker's decoded PCM into [`SpeakerPcmBuffer`], enqueue completed
//! [`PcmSegment`] values into a bounded worker queue, and account for queue
//! overflow with [`VoiceDropCounters`].

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Discord's decoded voice sample rate.
pub const DISCORD_VOICE_SAMPLE_RATE_HZ: u32 = 48_000;

/// Discord's decoded voice channel count.
pub const DISCORD_VOICE_CHANNELS: u16 = 2;

const BYTES_PER_I16_SAMPLE: u16 = 2;

/// Configuration errors for [`PcmCaptureConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmCaptureConfigError {
    ZeroMinimumSpeech,
    ZeroTrailingSilence,
    ZeroMaximumSegment,
    MinimumSpeechExceedsMaximumSegment,
    FrameCountOverflow,
}

impl fmt::Display for PcmCaptureConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMinimumSpeech => f.write_str("minimum speech duration must be non-zero"),
            Self::ZeroTrailingSilence => f.write_str("trailing silence duration must be non-zero"),
            Self::ZeroMaximumSegment => f.write_str("maximum segment duration must be non-zero"),
            Self::MinimumSpeechExceedsMaximumSegment => {
                f.write_str("minimum speech duration cannot exceed maximum segment duration")
            }
            Self::FrameCountOverflow => {
                f.write_str("configured duration is too large for this platform")
            }
        }
    }
}

impl Error for PcmCaptureConfigError {}

/// Sample-count based voice segmentation configuration.
///
/// A *frame* contains one sample for each of the two channels. Duration is
/// derived from frame counts, never from wall-clock time, so executor stalls do
/// not change silence detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmCaptureConfig {
    silence_threshold: u16,
    minimum_speech_frames: usize,
    trailing_silence_frames: usize,
    maximum_segment_frames: usize,
}

impl PcmCaptureConfig {
    /// Creates a configuration from exact 48 kHz frame counts.
    ///
    /// A frame is considered speech when the absolute value of either channel
    /// is strictly greater than `silence_threshold`.
    pub fn from_frame_counts(
        silence_threshold: u16,
        minimum_speech_frames: usize,
        trailing_silence_frames: usize,
        maximum_segment_frames: usize,
    ) -> Result<Self, PcmCaptureConfigError> {
        if minimum_speech_frames == 0 {
            return Err(PcmCaptureConfigError::ZeroMinimumSpeech);
        }
        if trailing_silence_frames == 0 {
            return Err(PcmCaptureConfigError::ZeroTrailingSilence);
        }
        if maximum_segment_frames == 0 {
            return Err(PcmCaptureConfigError::ZeroMaximumSegment);
        }
        if minimum_speech_frames > maximum_segment_frames {
            return Err(PcmCaptureConfigError::MinimumSpeechExceedsMaximumSegment);
        }
        if maximum_segment_frames
            .checked_mul(usize::from(DISCORD_VOICE_CHANNELS))
            .is_none()
        {
            return Err(PcmCaptureConfigError::FrameCountOverflow);
        }

        Ok(Self {
            silence_threshold,
            minimum_speech_frames,
            trailing_silence_frames,
            maximum_segment_frames,
        })
    }

    /// Creates a configuration from millisecond durations.
    ///
    /// Durations are rounded up to a complete 48 kHz PCM frame.
    pub fn from_millis(
        silence_threshold: u16,
        minimum_speech_ms: u32,
        trailing_silence_ms: u32,
        maximum_segment_ms: u32,
    ) -> Result<Self, PcmCaptureConfigError> {
        Self::from_frame_counts(
            silence_threshold,
            frames_from_millis(minimum_speech_ms)?,
            frames_from_millis(trailing_silence_ms)?,
            frames_from_millis(maximum_segment_ms)?,
        )
    }

    pub fn silence_threshold(&self) -> u16 {
        self.silence_threshold
    }

    pub fn minimum_speech_frames(&self) -> usize {
        self.minimum_speech_frames
    }

    pub fn trailing_silence_frames(&self) -> usize {
        self.trailing_silence_frames
    }

    pub fn maximum_segment_frames(&self) -> usize {
        self.maximum_segment_frames
    }
}

impl Default for PcmCaptureConfig {
    fn default() -> Self {
        Self {
            // A deliberately conservative starting point. This remains an
            // opt-in subsystem; deployments can tune it after collecting
            // microphone-level metrics.
            silence_threshold: 500,
            minimum_speech_frames: 4_800,      // 100 ms
            trailing_silence_frames: 96_000,   // 2 s
            maximum_segment_frames: 1_440_000, // 30 s
        }
    }
}

fn frames_from_millis(milliseconds: u32) -> Result<usize, PcmCaptureConfigError> {
    let numerator = u64::from(milliseconds)
        .checked_mul(u64::from(DISCORD_VOICE_SAMPLE_RATE_HZ))
        .ok_or(PcmCaptureConfigError::FrameCountOverflow)?;
    let rounded_up = numerator
        .checked_add(999)
        .ok_or(PcmCaptureConfigError::FrameCountOverflow)?
        / 1_000;
    usize::try_from(rounded_up).map_err(|_| PcmCaptureConfigError::FrameCountOverflow)
}

/// Why a PCM segment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentBoundary {
    /// The configured number of silent frames followed speech.
    Silence,
    /// The configured hard maximum segment size was reached.
    MaximumDuration,
    /// The owner explicitly flushed the speaker buffer.
    Flush,
}

/// One continuous, stereo PCM segment for one speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmSegment {
    /// Inclusive frame offset from the beginning of capture.
    pub start_frame: u64,
    /// Exclusive frame offset from the beginning of capture.
    pub end_frame: u64,
    /// Interleaved left/right signed 16-bit PCM samples.
    pub interleaved_samples: Vec<i16>,
    pub boundary: SegmentBoundary,
}

impl PcmSegment {
    pub fn frame_count(&self) -> usize {
        self.interleaved_samples.len() / usize::from(DISCORD_VOICE_CHANNELS)
    }

    pub fn duration_millis(&self) -> u64 {
        u64::try_from(self.frame_count())
            .unwrap_or(u64::MAX)
            .saturating_mul(1_000)
            / u64::from(DISCORD_VOICE_SAMPLE_RATE_HZ)
    }

    /// Encodes this segment as a standard little-endian PCM WAV file.
    pub fn to_wav_bytes(&self) -> Result<Vec<u8>, WavEncodeError> {
        encode_pcm_s16le_wav(
            &self.interleaved_samples,
            DISCORD_VOICE_SAMPLE_RATE_HZ,
            DISCORD_VOICE_CHANNELS,
        )
    }
}

/// Capture input errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmCaptureError {
    NonStereoSampleCount { sample_count: usize },
}

impl fmt::Display for PcmCaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonStereoSampleCount { sample_count } => write!(
                f,
                "interleaved stereo PCM requires an even sample count, got {sample_count}"
            ),
        }
    }
}

impl Error for PcmCaptureError {}

/// Cumulative accounting for one [`SpeakerPcmBuffer`].
///
/// Frames still in the active buffer are intentionally absent from both
/// `emitted_frames` and `ignored_frames`. Once flushed, the invariant is
/// `received_frames == emitted_frames + ignored_frames` (until `u64`
/// saturation, which is only defensive).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PcmCaptureStats {
    pub received_frames: u64,
    pub emitted_frames: u64,
    pub ignored_frames: u64,
    pub emitted_segments: u64,
}

/// Sample-count based segmenter for one Discord speaker.
///
/// Keep one instance per Discord user/SSRC mapping. Inactive leading silence
/// and too-short bursts are ignored. Trailing silence is used as the boundary
/// signal but is trimmed from emitted audio.
#[derive(Debug)]
pub struct SpeakerPcmBuffer {
    config: PcmCaptureConfig,
    samples: Vec<i16>,
    segment_start_frame: Option<u64>,
    speech_frames: usize,
    /// Silent frames after the last stored speech frame. These are deferred so
    /// a silent Songbird tick does not allocate PCM that will usually be
    /// trimmed. If speech resumes before the boundary, zero frames are then
    /// materialized to preserve the gap presented to STT.
    pending_silence_frames: usize,
    stats: PcmCaptureStats,
}

impl SpeakerPcmBuffer {
    pub fn new(config: PcmCaptureConfig) -> Self {
        Self {
            config,
            samples: Vec::new(),
            segment_start_frame: None,
            speech_frames: 0,
            pending_silence_frames: 0,
            stats: PcmCaptureStats::default(),
        }
    }

    pub fn config(&self) -> PcmCaptureConfig {
        self.config
    }

    pub fn stats(&self) -> PcmCaptureStats {
        self.stats
    }

    pub fn buffered_frames(&self) -> usize {
        self.stored_frames()
            .saturating_add(self.pending_silence_frames)
    }

    pub fn is_active(&self) -> bool {
        self.segment_start_frame.is_some()
    }

    /// Pushes decoded, interleaved stereo PCM and returns every completed
    /// segment. A normal 20 ms voice tick usually returns zero or one segment.
    ///
    /// Input shape is validated before state is mutated.
    pub fn push_interleaved_stereo(
        &mut self,
        interleaved_samples: &[i16],
    ) -> Result<Vec<PcmSegment>, PcmCaptureError> {
        if !interleaved_samples
            .len()
            .is_multiple_of(usize::from(DISCORD_VOICE_CHANNELS))
        {
            return Err(PcmCaptureError::NonStereoSampleCount {
                sample_count: interleaved_samples.len(),
            });
        }

        let mut completed = Vec::new();
        for frame in interleaved_samples.chunks_exact(usize::from(DISCORD_VOICE_CHANNELS)) {
            let is_speech = frame
                .iter()
                .any(|sample| sample.unsigned_abs() > self.config.silence_threshold);

            if !is_speech {
                if let Some(segment) = self.push_silence_frames(1) {
                    completed.push(segment);
                }
                continue;
            }

            let frame_index = self.stats.received_frames;
            self.stats.received_frames = self.stats.received_frames.saturating_add(1);
            if !self.is_active() {
                self.segment_start_frame = Some(frame_index);
            } else {
                self.materialize_pending_silence();
            }
            self.samples.extend_from_slice(frame);
            self.speech_frames = self.speech_frames.saturating_add(1);

            if self.buffered_frames() >= self.config.maximum_segment_frames {
                if let Some(segment) = self.finish_segment(SegmentBoundary::MaximumDuration) {
                    completed.push(segment);
                }
            }
        }

        Ok(completed)
    }

    /// Advances capture by silent stereo frames without requiring the caller to
    /// allocate a zero-filled PCM buffer.
    ///
    /// Songbird reports an SSRC in `VoiceTick::silent` without decoded samples;
    /// pass 960 here for one ordinary 20 ms tick at 48 kHz. At most one segment
    /// can complete: after the active segment ends, all remaining frames are
    /// inactive leading silence and are accounted as ignored.
    ///
    /// Silent PCM is also deferred internally and therefore does not allocate
    /// unless speech later resumes before the configured silence boundary.
    pub fn push_silence_frames(&mut self, frame_count: usize) -> Option<PcmSegment> {
        if frame_count == 0 {
            return None;
        }

        if !self.is_active() {
            self.account_inactive_silence(frame_count);
            return None;
        }

        let until_silence_boundary = self
            .config
            .trailing_silence_frames
            .saturating_sub(self.pending_silence_frames);
        let until_maximum_boundary = self
            .config
            .maximum_segment_frames
            .saturating_sub(self.buffered_frames());
        let until_boundary = until_silence_boundary.min(until_maximum_boundary);
        let consumed = frame_count.min(until_boundary);

        self.pending_silence_frames = self.pending_silence_frames.saturating_add(consumed);
        self.stats.received_frames = self
            .stats
            .received_frames
            .saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));

        let boundary = if self.pending_silence_frames >= self.config.trailing_silence_frames {
            Some(SegmentBoundary::Silence)
        } else if self.buffered_frames() >= self.config.maximum_segment_frames {
            Some(SegmentBoundary::MaximumDuration)
        } else {
            None
        };

        let segment = boundary.and_then(|boundary| self.finish_segment(boundary));
        self.account_inactive_silence(frame_count - consumed);
        segment
    }

    /// Finalizes a partial segment, trimming any silence already accumulated.
    /// Too-short speech bursts are discarded.
    pub fn flush(&mut self) -> Option<PcmSegment> {
        self.finish_segment(SegmentBoundary::Flush)
    }

    fn finish_segment(&mut self, boundary: SegmentBoundary) -> Option<PcmSegment> {
        let start_frame = self.segment_start_frame?;

        let kept_frames = self.stored_frames();
        let ignored_trailing_frames = self.pending_silence_frames;
        let buffered_frames = kept_frames.saturating_add(ignored_trailing_frames);
        let qualifies = self.speech_frames >= self.config.minimum_speech_frames && kept_frames > 0;

        let samples = std::mem::take(&mut self.samples);
        self.segment_start_frame = None;
        self.speech_frames = 0;
        self.pending_silence_frames = 0;

        if !qualifies {
            self.stats.ignored_frames = self
                .stats
                .ignored_frames
                .saturating_add(u64::try_from(buffered_frames).unwrap_or(u64::MAX));
            return None;
        }

        self.stats.ignored_frames = self
            .stats
            .ignored_frames
            .saturating_add(u64::try_from(ignored_trailing_frames).unwrap_or(u64::MAX));
        self.stats.emitted_frames = self
            .stats
            .emitted_frames
            .saturating_add(u64::try_from(kept_frames).unwrap_or(u64::MAX));
        self.stats.emitted_segments = self.stats.emitted_segments.saturating_add(1);

        let end_frame = start_frame.saturating_add(u64::try_from(kept_frames).unwrap_or(u64::MAX));
        Some(PcmSegment {
            start_frame,
            end_frame,
            interleaved_samples: samples,
            boundary,
        })
    }

    fn stored_frames(&self) -> usize {
        self.samples.len() / usize::from(DISCORD_VOICE_CHANNELS)
    }

    fn materialize_pending_silence(&mut self) {
        if self.pending_silence_frames == 0 {
            return;
        }
        let additional_samples = self
            .pending_silence_frames
            .saturating_mul(usize::from(DISCORD_VOICE_CHANNELS));
        self.samples
            .resize(self.samples.len().saturating_add(additional_samples), 0);
        self.pending_silence_frames = 0;
    }

    fn account_inactive_silence(&mut self, frame_count: usize) {
        let frame_count = u64::try_from(frame_count).unwrap_or(u64::MAX);
        self.stats.received_frames = self.stats.received_frames.saturating_add(frame_count);
        self.stats.ignored_frames = self.stats.ignored_frames.saturating_add(frame_count);
    }
}

/// WAV encoding errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavEncodeError {
    ZeroChannels,
    SamplesNotChannelAligned { sample_count: usize, channels: u16 },
    BlockAlignOverflow,
    ByteRateOverflow,
    DataTooLarge,
}

impl fmt::Display for WavEncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => f.write_str("WAV channel count must be non-zero"),
            Self::SamplesNotChannelAligned {
                sample_count,
                channels,
            } => write!(
                f,
                "{sample_count} samples are not aligned to {channels} channels"
            ),
            Self::BlockAlignOverflow => f.write_str("WAV block alignment exceeds u16"),
            Self::ByteRateOverflow => f.write_str("WAV byte rate exceeds u32"),
            Self::DataTooLarge => f.write_str("WAV data exceeds the RIFF 32-bit size limit"),
        }
    }
}

impl Error for WavEncodeError {}

/// Encodes interleaved signed 16-bit PCM into a little-endian RIFF/WAV file.
///
/// The output contains the canonical 44-byte PCM header followed by sample
/// bytes. Encoding is explicit rather than native-endian, so output is
/// identical on all supported platforms.
pub fn encode_pcm_s16le_wav(
    interleaved_samples: &[i16],
    sample_rate_hz: u32,
    channels: u16,
) -> Result<Vec<u8>, WavEncodeError> {
    if channels == 0 {
        return Err(WavEncodeError::ZeroChannels);
    }
    if !interleaved_samples
        .len()
        .is_multiple_of(usize::from(channels))
    {
        return Err(WavEncodeError::SamplesNotChannelAligned {
            sample_count: interleaved_samples.len(),
            channels,
        });
    }

    let block_align = channels
        .checked_mul(BYTES_PER_I16_SAMPLE)
        .ok_or(WavEncodeError::BlockAlignOverflow)?;
    let byte_rate = sample_rate_hz
        .checked_mul(u32::from(block_align))
        .ok_or(WavEncodeError::ByteRateOverflow)?;
    let data_len = interleaved_samples
        .len()
        .checked_mul(usize::from(BYTES_PER_I16_SAMPLE))
        .and_then(|length| u32::try_from(length).ok())
        .ok_or(WavEncodeError::DataTooLarge)?;
    let riff_size = 36_u32
        .checked_add(data_len)
        .ok_or(WavEncodeError::DataTooLarge)?;
    let capacity = 44_usize
        .checked_add(usize::try_from(data_len).map_err(|_| WavEncodeError::DataTooLarge)?)
        .ok_or(WavEncodeError::DataTooLarge)?;

    let mut wav = Vec::with_capacity(capacity);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in interleaved_samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}

/// One successful transcript segment, timed relative to the voice session.
///
/// `start_frame` and `end_frame` use the same 48 kHz timeline as
/// [`PcmSegment`]. Discord snowflakes remain plain `u64` values so this model
/// stays independent from Serenity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub speaker_id: u64,
    pub speaker_name: String,
    pub start_frame: u64,
    pub end_frame: u64,
    pub text: String,
}

impl TranscriptEntry {
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }

    pub fn start_millis(&self) -> u64 {
        self.start_frame.saturating_mul(1_000) / u64::from(DISCORD_VOICE_SAMPLE_RATE_HZ)
    }

    pub fn end_millis(&self) -> u64 {
        self.end_frame.saturating_mul(1_000) / u64::from(DISCORD_VOICE_SAMPLE_RATE_HZ)
    }
}

/// Invalid limits for [`TranscriptStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptStoreError {
    ZeroMaximumEntries,
    ZeroMaximumTextBytes,
}

impl fmt::Display for TranscriptStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaximumEntries => {
                f.write_str("transcript maximum entry count must be non-zero")
            }
            Self::ZeroMaximumTextBytes => {
                f.write_str("transcript maximum text byte count must be non-zero")
            }
        }
    }
}

impl Error for TranscriptStoreError {}

/// Per-push result from [`TranscriptStore::push`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptPushOutcome {
    pub accepted: bool,
    pub evicted_entries: u64,
    pub evicted_text_bytes: u64,
}

/// Cumulative bounded-store accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptStoreStats {
    pub accepted_entries: u64,
    pub rejected_entries: u64,
    pub evicted_entries: u64,
    pub evicted_text_bytes: u64,
}

/// A transcript window bounded by both entry count and transcript text bytes.
///
/// When necessary, the oldest entries are evicted before a new entry is
/// accepted. An individual entry larger than `maximum_text_bytes` is rejected
/// without evicting useful existing context. Byte accounting intentionally
/// covers transcript text only, not speaker display metadata.
#[derive(Debug)]
pub struct TranscriptStore {
    entries: VecDeque<TranscriptEntry>,
    maximum_entries: usize,
    maximum_text_bytes: usize,
    text_bytes: usize,
    stats: TranscriptStoreStats,
}

impl TranscriptStore {
    pub fn new(
        maximum_entries: usize,
        maximum_text_bytes: usize,
    ) -> Result<Self, TranscriptStoreError> {
        if maximum_entries == 0 {
            return Err(TranscriptStoreError::ZeroMaximumEntries);
        }
        if maximum_text_bytes == 0 {
            return Err(TranscriptStoreError::ZeroMaximumTextBytes);
        }
        Ok(Self {
            entries: VecDeque::new(),
            maximum_entries,
            maximum_text_bytes,
            text_bytes: 0,
            stats: TranscriptStoreStats::default(),
        })
    }

    pub fn push(&mut self, entry: TranscriptEntry) -> TranscriptPushOutcome {
        let incoming_bytes = entry.text_bytes();
        if incoming_bytes > self.maximum_text_bytes {
            self.stats.rejected_entries = self.stats.rejected_entries.saturating_add(1);
            return TranscriptPushOutcome::default();
        }

        let mut outcome = TranscriptPushOutcome {
            accepted: true,
            ..TranscriptPushOutcome::default()
        };
        while self.entries.len() >= self.maximum_entries
            || self
                .text_bytes
                .checked_add(incoming_bytes)
                .is_none_or(|total| total > self.maximum_text_bytes)
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            let evicted_bytes = evicted.text_bytes();
            self.text_bytes -= evicted_bytes;
            outcome.evicted_entries = outcome.evicted_entries.saturating_add(1);
            outcome.evicted_text_bytes = outcome
                .evicted_text_bytes
                .saturating_add(u64::try_from(evicted_bytes).unwrap_or(u64::MAX));
        }

        self.text_bytes += incoming_bytes;
        self.entries.push_back(entry);
        self.stats.accepted_entries = self.stats.accepted_entries.saturating_add(1);
        self.stats.evicted_entries = self
            .stats
            .evicted_entries
            .saturating_add(outcome.evicted_entries);
        self.stats.evicted_text_bytes = self
            .stats
            .evicted_text_bytes
            .saturating_add(outcome.evicted_text_bytes);
        outcome
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &TranscriptEntry> + ExactSizeIterator {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn text_bytes(&self) -> usize {
        self.text_bytes
    }

    pub fn stats(&self) -> TranscriptStoreStats {
        self.stats
    }

    /// Clears the current window while preserving cumulative store statistics.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.text_bytes = 0;
    }
}

/// Snapshot of work discarded because an external bounded queue was full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceDropSnapshot {
    pub pcm_chunks: u64,
    pub pcm_frames: u64,
    pub segments: u64,
    pub transcripts: u64,
}

/// Thread-safe, saturating counters for bounded worker-queue overflow.
///
/// This type does not own a queue. Call the relevant method when a bounded
/// channel's non-blocking send rejects an item. Relaxed atomics are sufficient:
/// the counters carry metrics, not synchronization state.
#[derive(Debug, Default)]
pub struct VoiceDropCounters {
    pcm_chunks: AtomicU64,
    pcm_frames: AtomicU64,
    segments: AtomicU64,
    transcripts: AtomicU64,
}

impl VoiceDropCounters {
    pub fn record_pcm_chunk(&self, frame_count: u64) {
        saturating_atomic_add(&self.pcm_chunks, 1);
        saturating_atomic_add(&self.pcm_frames, frame_count);
    }

    pub fn record_segment(&self) {
        saturating_atomic_add(&self.segments, 1);
    }

    pub fn record_transcript(&self) {
        saturating_atomic_add(&self.transcripts, 1);
    }

    pub fn snapshot(&self) -> VoiceDropSnapshot {
        VoiceDropSnapshot {
            pcm_chunks: self.pcm_chunks.load(Ordering::Relaxed),
            pcm_frames: self.pcm_frames.load(Ordering::Relaxed),
            segments: self.segments.load(Ordering::Relaxed),
            transcripts: self.transcripts.load(Ordering::Relaxed),
        }
    }

    /// Atomically returns the current snapshot and resets all counters.
    ///
    /// Concurrent increments may land before or after the corresponding swap;
    /// each increment is still represented in either this snapshot or the next.
    pub fn take_snapshot(&self) -> VoiceDropSnapshot {
        VoiceDropSnapshot {
            pcm_chunks: self.pcm_chunks.swap(0, Ordering::Relaxed),
            pcm_frames: self.pcm_frames.swap(0, Ordering::Relaxed),
            segments: self.segments.swap(0, Ordering::Relaxed),
            transcripts: self.transcripts.swap(0, Ordering::Relaxed),
        }
    }
}

fn saturating_atomic_add(counter: &AtomicU64, amount: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(amount);
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn config(
        threshold: u16,
        minimum_speech_frames: usize,
        trailing_silence_frames: usize,
        maximum_segment_frames: usize,
    ) -> PcmCaptureConfig {
        PcmCaptureConfig::from_frame_counts(
            threshold,
            minimum_speech_frames,
            trailing_silence_frames,
            maximum_segment_frames,
        )
        .expect("valid test capture config")
    }

    fn stereo_frames(amplitudes: &[i16]) -> Vec<i16> {
        amplitudes
            .iter()
            .flat_map(|sample| [*sample, *sample])
            .collect()
    }

    fn transcript(speaker_id: u64, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            speaker_id,
            speaker_name: format!("speaker-{speaker_id}"),
            start_frame: 0,
            end_frame: 48_000,
            text: text.to_owned(),
        }
    }

    #[test]
    fn default_capture_durations_are_explicit() {
        let config = PcmCaptureConfig::default();
        assert_eq!(config.silence_threshold(), 500);
        assert_eq!(config.minimum_speech_frames(), 4_800);
        assert_eq!(config.trailing_silence_frames(), 96_000);
        assert_eq!(config.maximum_segment_frames(), 1_440_000);
    }

    #[test]
    fn millisecond_configuration_rounds_up_to_frames() {
        let config = PcmCaptureConfig::from_millis(10, 1, 2, 3).unwrap();
        assert_eq!(config.minimum_speech_frames(), 48);
        assert_eq!(config.trailing_silence_frames(), 96);
        assert_eq!(config.maximum_segment_frames(), 144);
    }

    #[test]
    fn invalid_capture_configuration_is_rejected() {
        assert_eq!(
            PcmCaptureConfig::from_frame_counts(0, 0, 1, 1),
            Err(PcmCaptureConfigError::ZeroMinimumSpeech)
        );
        assert_eq!(
            PcmCaptureConfig::from_frame_counts(0, 1, 0, 1),
            Err(PcmCaptureConfigError::ZeroTrailingSilence)
        );
        assert_eq!(
            PcmCaptureConfig::from_frame_counts(0, 1, 1, 0),
            Err(PcmCaptureConfigError::ZeroMaximumSegment)
        );
        assert_eq!(
            PcmCaptureConfig::from_frame_counts(0, 2, 1, 1),
            Err(PcmCaptureConfigError::MinimumSpeechExceedsMaximumSegment)
        );
    }

    #[test]
    fn malformed_stereo_input_does_not_mutate_state() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 1, 2, 20));
        let error = buffer.push_interleaved_stereo(&[100, 100, 100]);
        assert_eq!(
            error,
            Err(PcmCaptureError::NonStereoSampleCount { sample_count: 3 })
        );
        assert_eq!(buffer.stats(), PcmCaptureStats::default());
        assert!(!buffer.is_active());
    }

    #[test]
    fn leading_and_trailing_silence_are_trimmed() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 2, 2, 20));
        let samples = stereo_frames(&[0, 0, 50, 60, 0, 0]);
        let segments = buffer.push_interleaved_stereo(&samples).unwrap();

        assert_eq!(segments.len(), 1);
        let segment = &segments[0];
        assert_eq!(segment.start_frame, 2);
        assert_eq!(segment.end_frame, 4);
        assert_eq!(segment.interleaved_samples, stereo_frames(&[50, 60]));
        assert_eq!(segment.boundary, SegmentBoundary::Silence);
        assert_eq!(
            buffer.stats(),
            PcmCaptureStats {
                received_frames: 6,
                emitted_frames: 2,
                ignored_frames: 4,
                emitted_segments: 1,
            }
        );
    }

    #[test]
    fn silence_detection_uses_samples_not_push_timing() {
        let samples = stereo_frames(&[40, 40, 0, 0]);

        let mut one_push = SpeakerPcmBuffer::new(config(10, 2, 2, 20));
        let expected = one_push.push_interleaved_stereo(&samples).unwrap();

        let mut many_pushes = SpeakerPcmBuffer::new(config(10, 2, 2, 20));
        let mut actual = Vec::new();
        for frame in samples.chunks_exact(2) {
            actual.extend(many_pushes.push_interleaved_stereo(frame).unwrap());
        }

        assert_eq!(actual, expected);
        assert_eq!(many_pushes.stats(), one_push.stats());
    }

    #[test]
    fn either_stereo_channel_can_trigger_speech() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 1, 1, 20));
        let segments = buffer
            .push_interleaved_stereo(&[0, i16::MIN, 0, 0])
            .unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].interleaved_samples, vec![0, i16::MIN]);
    }

    #[test]
    fn short_noise_burst_is_discarded() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 3, 2, 20));
        let segments = buffer
            .push_interleaved_stereo(&stereo_frames(&[50, 50, 0, 0]))
            .unwrap();
        assert!(segments.is_empty());
        assert!(!buffer.is_active());
        assert_eq!(buffer.stats().received_frames, 4);
        assert_eq!(buffer.stats().ignored_frames, 4);
        assert_eq!(buffer.stats().emitted_frames, 0);
    }

    #[test]
    fn maximum_duration_splits_long_speech_without_gaps() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 1, 2, 3));
        let mut segments = buffer
            .push_interleaved_stereo(&stereo_frames(&[50, 50, 50, 50, 50, 50, 50, 50]))
            .unwrap();
        segments.extend(buffer.flush());

        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].start_frame, 0);
        assert_eq!(segments[0].end_frame, 3);
        assert_eq!(segments[0].boundary, SegmentBoundary::MaximumDuration);
        assert_eq!(segments[1].start_frame, 3);
        assert_eq!(segments[1].end_frame, 6);
        assert_eq!(segments[1].boundary, SegmentBoundary::MaximumDuration);
        assert_eq!(segments[2].start_frame, 6);
        assert_eq!(segments[2].end_frame, 8);
        assert_eq!(segments[2].boundary, SegmentBoundary::Flush);
        assert_eq!(buffer.stats().emitted_frames, 8);
        assert_eq!(buffer.stats().ignored_frames, 0);
    }

    #[test]
    fn flush_trims_partial_trailing_silence() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 2, 5, 20));
        let segments = buffer
            .push_interleaved_stereo(&stereo_frames(&[50, 50, 0, 0]))
            .unwrap();
        assert!(segments.is_empty());

        let segment = buffer.flush().expect("speech qualifies on flush");
        assert_eq!(segment.interleaved_samples, stereo_frames(&[50, 50]));
        assert_eq!(segment.start_frame, 0);
        assert_eq!(segment.end_frame, 2);
        assert_eq!(segment.boundary, SegmentBoundary::Flush);
        assert_eq!(buffer.stats().received_frames, 4);
        assert_eq!(buffer.stats().emitted_frames, 2);
        assert_eq!(buffer.stats().ignored_frames, 2);
    }

    #[test]
    fn silent_voice_tick_finishes_segment_without_pcm_allocation() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 1, 960, 4_800));
        buffer.push_interleaved_stereo(&[50, 50]).unwrap();
        let capacity_after_speech = buffer.samples.capacity();

        let segment = buffer
            .push_silence_frames(960)
            .expect("20 ms silent tick reaches the boundary");

        assert_eq!(segment.start_frame, 0);
        assert_eq!(segment.end_frame, 1);
        assert_eq!(segment.interleaved_samples, vec![50, 50]);
        assert_eq!(segment.boundary, SegmentBoundary::Silence);
        assert_eq!(buffer.stats().received_frames, 961);
        assert_eq!(buffer.stats().emitted_frames, 1);
        assert_eq!(buffer.stats().ignored_frames, 960);
        // The completed segment owns the original allocation; no zero-filled
        // buffer was allocated for the silent tick.
        assert_eq!(
            capacity_after_speech,
            segment.interleaved_samples.capacity()
        );
        assert_eq!(buffer.samples.capacity(), 0);
    }

    #[test]
    fn partial_silent_ticks_are_deferred_until_speech_resumes() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 2, 10, 100));
        buffer.push_interleaved_stereo(&[50, 50]).unwrap();
        let capacity_before_silence = buffer.samples.capacity();

        assert!(buffer.push_silence_frames(3).is_none());
        assert_eq!(buffer.samples.capacity(), capacity_before_silence);
        assert_eq!(buffer.samples, vec![50, 50]);
        assert_eq!(buffer.buffered_frames(), 4);

        buffer.push_interleaved_stereo(&[60, 60]).unwrap();
        let segment = buffer.flush().unwrap();
        assert_eq!(
            segment.interleaved_samples,
            stereo_frames(&[50, 0, 0, 0, 60])
        );
        assert_eq!(segment.start_frame, 0);
        assert_eq!(segment.end_frame, 5);
    }

    #[test]
    fn inactive_silent_tick_is_accounted_in_bulk() {
        let mut buffer = SpeakerPcmBuffer::new(config(10, 1, 960, 4_800));
        assert!(buffer.push_silence_frames(960).is_none());
        assert_eq!(buffer.stats().received_frames, 960);
        assert_eq!(buffer.stats().ignored_frames, 960);
        assert_eq!(buffer.samples.capacity(), 0);
    }

    #[test]
    fn wav_encoder_writes_canonical_header_and_little_endian_samples() {
        let samples = [1_i16, -2_i16, i16::MIN, i16::MAX];
        let wav = encode_pcm_s16le_wav(&samples, 48_000, 2).unwrap();

        assert_eq!(wav.len(), 52);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 44);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 192_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 8);
        assert_eq!(&wav[44..], &[1, 0, 254, 255, 0, 128, 255, 127]);
    }

    #[test]
    fn wav_encoder_rejects_invalid_shape_and_overflow() {
        assert_eq!(
            encode_pcm_s16le_wav(&[], 48_000, 0),
            Err(WavEncodeError::ZeroChannels)
        );
        assert_eq!(
            encode_pcm_s16le_wav(&[1, 2, 3], 48_000, 2),
            Err(WavEncodeError::SamplesNotChannelAligned {
                sample_count: 3,
                channels: 2,
            })
        );
        assert_eq!(
            encode_pcm_s16le_wav(&[], u32::MAX, 2),
            Err(WavEncodeError::ByteRateOverflow)
        );
    }

    #[test]
    fn segment_wav_wrapper_uses_discord_audio_format() {
        let segment = PcmSegment {
            start_frame: 0,
            end_frame: 1,
            interleaved_samples: vec![1, -1],
            boundary: SegmentBoundary::Flush,
        };
        let wav = segment.to_wav_bytes().unwrap();
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
        assert_eq!(&wav[44..], &[1, 0, 255, 255]);
    }

    #[test]
    fn transcript_timing_uses_capture_frames() {
        let entry = TranscriptEntry {
            speaker_id: 42,
            speaker_name: "Can".into(),
            start_frame: 24_000,
            end_frame: 72_000,
            text: "hello".into(),
        };
        assert_eq!(entry.start_millis(), 500);
        assert_eq!(entry.end_millis(), 1_500);
    }

    #[test]
    fn transcript_store_evicts_oldest_by_entry_count() {
        let mut store = TranscriptStore::new(2, 100).unwrap();
        assert!(store.push(transcript(1, "one")).accepted);
        assert!(store.push(transcript(2, "two")).accepted);
        let outcome = store.push(transcript(3, "three"));

        assert_eq!(outcome.evicted_entries, 1);
        assert_eq!(outcome.evicted_text_bytes, 3);
        assert_eq!(
            store
                .entries()
                .map(|entry| entry.speaker_id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(store.text_bytes(), 8);
        assert_eq!(store.stats().accepted_entries, 3);
        assert_eq!(store.stats().evicted_entries, 1);
    }

    #[test]
    fn transcript_store_evicts_oldest_until_text_fits() {
        let mut store = TranscriptStore::new(10, 8).unwrap();
        store.push(transcript(1, "aaa"));
        store.push(transcript(2, "bbb"));
        let outcome = store.push(transcript(3, "ccccc"));

        assert_eq!(outcome.evicted_entries, 1);
        assert_eq!(outcome.evicted_text_bytes, 3);
        assert_eq!(store.len(), 2);
        assert_eq!(store.text_bytes(), 8);
        assert_eq!(store.entries().next().unwrap().speaker_id, 2);
    }

    #[test]
    fn oversized_transcript_is_rejected_without_evicting_context() {
        let mut store = TranscriptStore::new(2, 4).unwrap();
        store.push(transcript(1, "good"));
        let outcome = store.push(transcript(2, "too large"));

        assert_eq!(outcome, TranscriptPushOutcome::default());
        assert_eq!(store.len(), 1);
        assert_eq!(store.entries().next().unwrap().speaker_id, 1);
        assert_eq!(store.stats().accepted_entries, 1);
        assert_eq!(store.stats().rejected_entries, 1);
    }

    #[test]
    fn transcript_text_limit_is_utf8_bytes_and_clear_keeps_stats() {
        let mut store = TranscriptStore::new(2, 6).unwrap();
        let outcome = store.push(transcript(1, "中文"));
        assert!(outcome.accepted);
        assert_eq!(store.text_bytes(), 6);
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.text_bytes(), 0);
        assert_eq!(store.stats().accepted_entries, 1);
    }

    #[test]
    fn transcript_store_rejects_zero_limits() {
        assert!(matches!(
            TranscriptStore::new(0, 1),
            Err(TranscriptStoreError::ZeroMaximumEntries)
        ));
        assert!(matches!(
            TranscriptStore::new(1, 0),
            Err(TranscriptStoreError::ZeroMaximumTextBytes)
        ));
    }

    #[test]
    fn drop_counters_are_thread_safe_and_resettable() {
        let counters = Arc::new(VoiceDropCounters::default());
        let workers = (0..4)
            .map(|_| {
                let counters = Arc::clone(&counters);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        counters.record_pcm_chunk(960);
                        counters.record_segment();
                        counters.record_transcript();
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }

        let expected = VoiceDropSnapshot {
            pcm_chunks: 400,
            pcm_frames: 384_000,
            segments: 400,
            transcripts: 400,
        };
        assert_eq!(counters.snapshot(), expected);
        assert_eq!(counters.take_snapshot(), expected);
        assert_eq!(counters.snapshot(), VoiceDropSnapshot::default());
    }

    #[test]
    fn saturating_counter_does_not_wrap() {
        let counter = AtomicU64::new(u64::MAX - 1);
        saturating_atomic_add(&counter, 10);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
}
