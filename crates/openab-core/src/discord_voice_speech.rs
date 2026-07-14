//! Conversion of bounded TTS WAV responses into Songbird playback input.

use anyhow::{bail, ensure, Context, Result};
use songbird::input::{Input, RawAdapter};
use std::io::Cursor;
use std::time::Duration;

const PCM_FORMAT: u16 = 1;
const IEEE_FLOAT_FORMAT: u16 = 3;

/// Decoded, interleaved `f32` PCM ready for Songbird's raw input adapter.
#[derive(Debug)]
pub struct DiscordVoiceSpeechAudio {
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

impl DiscordVoiceSpeechAudio {
    pub fn from_wav(wav: &[u8]) -> Result<Self> {
        ensure!(
            wav.len() >= 12 && &wav[..4] == b"RIFF" && &wav[8..12] == b"WAVE",
            "TTS response is not a RIFF/WAVE payload"
        );

        let mut format = None;
        let mut data = None;
        let mut cursor = 12_usize;
        while cursor.checked_add(8).is_some_and(|end| end <= wav.len()) {
            let chunk_id = &wav[cursor..cursor + 4];
            let declared_chunk_len = u32::from_le_bytes(
                wav[cursor + 4..cursor + 8]
                    .try_into()
                    .expect("WAV chunk length has four bytes"),
            );
            let chunk_start = cursor + 8;
            let streaming_data = declared_chunk_len == u32::MAX && chunk_id == b"data";
            let chunk_end = if streaming_data {
                // OpenAI's low-latency WAV response uses 0xffffffff for the
                // data size because the final length is unknown when the
                // streaming header is emitted. The completed HTTP body is the
                // authoritative boundary in that representation.
                wav.len()
            } else {
                let chunk_len = usize::try_from(declared_chunk_len)
                    .context("WAV chunk length does not fit usize")?;
                let chunk_end = chunk_start
                    .checked_add(chunk_len)
                    .context("WAV chunk length overflow")?;
                ensure!(chunk_end <= wav.len(), "truncated WAV chunk");
                chunk_end
            };

            match chunk_id {
                b"fmt " if format.is_none() => {
                    format = Some(parse_format(&wav[chunk_start..chunk_end])?)
                }
                b"data" if data.is_none() => data = Some(&wav[chunk_start..chunk_end]),
                _ => {}
            }
            cursor = if streaming_data {
                chunk_end
            } else {
                chunk_end
                    .checked_add((declared_chunk_len % 2) as usize)
                    .context("WAV chunk padding overflow")?
            };
        }

        let format = format.context("WAV payload has no fmt chunk")?;
        let data = data.context("WAV payload has no data chunk")?;
        let samples = decode_samples(data, format)?;
        ensure!(!samples.is_empty(), "WAV payload has no audio samples");
        Ok(Self {
            sample_rate: format.sample_rate,
            channels: format.channels,
            samples,
        })
    }

    pub fn duration(&self) -> Duration {
        let frames = self.samples.len() as u64 / u64::from(self.channels);
        Duration::from_secs_f64(frames as f64 / f64::from(self.sample_rate))
    }

    pub fn into_songbird_input(self) -> Input {
        let mut bytes = Vec::with_capacity(self.samples.len() * std::mem::size_of::<f32>());
        for sample in self.samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        RawAdapter::new(
            Cursor::new(bytes),
            self.sample_rate,
            u32::from(self.channels),
        )
        .into()
    }
}

#[derive(Clone, Copy)]
struct WavFormat {
    encoding: u16,
    channels: u16,
    sample_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
}

fn parse_format(chunk: &[u8]) -> Result<WavFormat> {
    ensure!(chunk.len() >= 16, "WAV fmt chunk is too short");
    let format = WavFormat {
        encoding: u16::from_le_bytes([chunk[0], chunk[1]]),
        channels: u16::from_le_bytes([chunk[2], chunk[3]]),
        sample_rate: u32::from_le_bytes(
            chunk[4..8].try_into().expect("sample rate has four bytes"),
        ),
        block_align: u16::from_le_bytes([chunk[12], chunk[13]]),
        bits_per_sample: u16::from_le_bytes([chunk[14], chunk[15]]),
    };
    ensure!(
        matches!(format.channels, 1 | 2),
        "TTS WAV must be mono or stereo"
    );
    ensure!(
        format.sample_rate > 0,
        "TTS WAV sample rate must be positive"
    );
    Ok(format)
}

fn decode_samples(data: &[u8], format: WavFormat) -> Result<Vec<f32>> {
    let bytes_per_sample = usize::from(format.bits_per_sample / 8);
    ensure!(bytes_per_sample > 0, "invalid WAV bits per sample");
    let expected_align = usize::from(format.channels)
        .checked_mul(bytes_per_sample)
        .context("WAV block alignment overflow")?;
    ensure!(
        usize::from(format.block_align) == expected_align,
        "unsupported WAV block alignment"
    );
    ensure!(
        data.len().is_multiple_of(bytes_per_sample),
        "truncated WAV sample data"
    );

    match (format.encoding, format.bits_per_sample) {
        (PCM_FORMAT, 16) => Ok(data
            .chunks_exact(2)
            .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32_768.0)
            .collect()),
        (IEEE_FLOAT_FORMAT, 32) => data
            .chunks_exact(4)
            .map(|sample| {
                let value =
                    f32::from_le_bytes(sample.try_into().expect("f32 sample has four bytes"));
                if value.is_finite() {
                    Ok(value.clamp(-1.0, 1.0))
                } else {
                    bail!("WAV contains a non-finite floating-point sample")
                }
            })
            .collect(),
        (encoding, bits) => bail!("unsupported WAV encoding {encoding} with {bits}-bit samples"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36_u32 + data_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&PCM_FORMAT.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    #[test]
    fn decodes_pcm16_and_reports_duration() {
        let audio =
            DiscordVoiceSpeechAudio::from_wav(&pcm16_wav(24_000, 1, &vec![i16::MAX; 24_000]))
                .unwrap();
        assert_eq!(audio.sample_rate, 24_000);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.samples.len(), 24_000);
        assert_eq!(audio.duration(), Duration::from_secs(1));
    }

    #[test]
    fn decodes_streaming_wav_with_unknown_riff_and_data_lengths() {
        let mut wav = pcm16_wav(24_000, 1, &vec![i16::MAX; 24_000]);
        wav[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());

        let audio = DiscordVoiceSpeechAudio::from_wav(&wav).unwrap();

        assert_eq!(audio.sample_rate, 24_000);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.samples.len(), 24_000);
        assert_eq!(audio.duration(), Duration::from_secs(1));
    }

    #[test]
    fn songbird_can_parse_and_decode_the_pcm_input() {
        let audio =
            DiscordVoiceSpeechAudio::from_wav(&pcm16_wav(24_000, 1, &vec![i16::MAX; 24_000]))
                .unwrap();
        let input = audio.into_songbird_input();
        let Input::Live(live, _) = input else {
            panic!("raw adapter must produce live input");
        };
        let songbird::input::LiveInput::Parsed(mut parsed) = live
            .promote(
                songbird::input::codecs::get_codec_registry(),
                songbird::input::codecs::get_probe(),
            )
            .unwrap()
        else {
            panic!("raw input must promote to parsed input");
        };

        let packet = parsed.format.next_packet().unwrap();
        let decoded = parsed.decoder.decode(&packet).unwrap();

        assert_eq!(decoded.spec().rate, 24_000);
        assert_eq!(decoded.spec().channels.count(), 1);
        assert_eq!(decoded.frames(), 480);
    }

    #[test]
    fn rejects_unsupported_and_truncated_wav() {
        let mut unsupported = pcm16_wav(24_000, 1, &[0]);
        unsupported[20..22].copy_from_slice(&6_u16.to_le_bytes());
        assert!(DiscordVoiceSpeechAudio::from_wav(&unsupported)
            .unwrap_err()
            .to_string()
            .contains("unsupported WAV encoding"));

        let mut truncated = pcm16_wav(24_000, 1, &[0]);
        truncated.pop();
        assert!(DiscordVoiceSpeechAudio::from_wav(&truncated)
            .unwrap_err()
            .to_string()
            .contains("truncated WAV chunk"));
    }
}
