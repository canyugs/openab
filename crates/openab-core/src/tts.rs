use crate::config::TtsConfig;
use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use std::time::Duration;

/// Hard ceiling for a single synthesized response. Intent confirmations are
/// short; bounding the response prevents a provider or proxy from exhausting
/// the bot's memory with an unexpectedly large body.
pub const MAX_TTS_AUDIO_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Serialize)]
struct SpeechRequest<'a> {
    model: &'a str,
    voice: &'a str,
    input: &'a str,
    response_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
}

/// Synthesize `input` as WAV audio through an OpenAI-compatible
/// `/audio/speech` endpoint.
///
/// The response is streamed into a bounded buffer and must begin with a valid
/// RIFF/WAVE container header before it is returned to the caller.
pub async fn synthesize_wav(
    client: &reqwest::Client,
    cfg: &TtsConfig,
    input: &str,
) -> Result<Vec<u8>> {
    synthesize_wav_with_limit(client, cfg, input, MAX_TTS_AUDIO_BYTES).await
}

async fn synthesize_wav_with_limit(
    client: &reqwest::Client,
    cfg: &TtsConfig,
    input: &str,
    max_audio_bytes: usize,
) -> Result<Vec<u8>> {
    ensure!(cfg.enabled, "TTS is disabled");
    ensure!(!cfg.api_key.trim().is_empty(), "TTS API key is empty");
    ensure!(!cfg.model.trim().is_empty(), "TTS model is empty");
    ensure!(!cfg.voice.trim().is_empty(), "TTS voice is empty");
    ensure!(!input.trim().is_empty(), "TTS input is empty");
    ensure!(
        cfg.request_timeout_seconds > 0,
        "TTS request timeout must be greater than zero"
    );
    ensure!(max_audio_bytes >= 12, "TTS response limit is too small");

    let base_url = cfg.base_url.trim().trim_end_matches('/');
    ensure!(!base_url.is_empty(), "TTS base URL is empty");
    let url = format!("{base_url}/audio/speech");
    let instructions = cfg
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let request = SpeechRequest {
        model: cfg.model.trim(),
        voice: cfg.voice.trim(),
        input,
        response_format: "wav",
        instructions,
    };

    let response = client
        .post(&url)
        .bearer_auth(cfg.api_key.trim())
        .timeout(Duration::from_secs(cfg.request_timeout_seconds))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("TTS request to {url} failed"))?;

    let status = response.status();
    if !status.is_success() {
        let body = read_bounded_body(response, 16 * 1024).await?;
        let body = String::from_utf8_lossy(&body);
        bail!("TTS API returned HTTP {status}: {body}");
    }

    if let Some(content_length) = response.content_length() {
        ensure!(
            content_length <= max_audio_bytes as u64,
            "TTS response exceeds {max_audio_bytes} byte limit"
        );
    }

    let audio = read_bounded_body(response, max_audio_bytes).await?;
    validate_wav(&audio)?;
    Ok(audio)
}

async fn read_bounded_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed to read TTS response body")?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= limit,
            "TTS response exceeds {limit} byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_wav(audio: &[u8]) -> Result<()> {
    if audio.len() < 12 || &audio[..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
        bail!("TTS response is not a valid RIFF/WAVE payload");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn enabled_config(base_url: String) -> TtsConfig {
        TtsConfig {
            enabled: true,
            api_key: "test-secret".into(),
            base_url,
            instructions: Some(" Speak briefly. ".into()),
            ..TtsConfig::default()
        }
    }

    async fn spawn_server(response: &'static [u8]) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end + 4]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }

            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.write_all(response).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/v1"), task)
    }

    #[tokio::test]
    async fn posts_explicit_wav_request_and_returns_audio() {
        let wav = b"RIFF\x04\x00\x00\x00WAVEdata";
        let (base_url, request_task) = spawn_server(wav).await;
        let cfg = enabled_config(base_url);

        let audio = synthesize_wav(&reqwest::Client::new(), &cfg, "確認嗎？")
            .await
            .unwrap();
        assert_eq!(audio, wav);

        let request = request_task.await.unwrap();
        assert!(request.starts_with("POST /v1/audio/speech HTTP/1.1"));
        assert!(request.contains("authorization: Bearer test-secret"));
        assert!(request.contains("\"model\":\"gpt-4o-mini-tts\""));
        assert!(request.contains("\"voice\":\"marin\""));
        assert!(request.contains("\"response_format\":\"wav\""));
        assert!(request.contains("\"instructions\":\"Speak briefly.\""));
    }

    #[tokio::test]
    async fn rejects_non_wav_response() {
        let (base_url, request_task) = spawn_server(b"not audio data").await;
        let error = synthesize_wav(&reqwest::Client::new(), &enabled_config(base_url), "hello")
            .await
            .unwrap_err();
        request_task.await.unwrap();

        assert!(error.to_string().contains("RIFF/WAVE"));
    }

    #[tokio::test]
    async fn rejects_response_over_limit() {
        let wav = b"RIFF\x04\x00\x00\x00WAVEdata";
        let (base_url, request_task) = spawn_server(wav).await;
        let error = synthesize_wav_with_limit(
            &reqwest::Client::new(),
            &enabled_config(base_url),
            "hello",
            12,
        )
        .await
        .unwrap_err();
        request_task.await.unwrap();

        assert!(error.to_string().contains("exceeds 12 byte limit"));
    }

    #[tokio::test]
    async fn rejects_disabled_configuration_before_request() {
        let error = synthesize_wav(&reqwest::Client::new(), &TtsConfig::default(), "hello")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "TTS is disabled");
    }
}
