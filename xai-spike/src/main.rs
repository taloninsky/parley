//! xAI Speech WebSocket protocol spike.
//!
//! Usage:
//!   PARLEY_XAI_API_KEY=xai-... cargo run -p xai-spike -- stt-ws [--audio path.wav]
//!   PARLEY_XAI_API_KEY=xai-... cargo run -p xai-spike -- tts-ws --text "Hello"
//!
//! What this does:
//!   - Opens a WebSocket to the xAI STT or TTS streaming endpoint.
//!   - STT: pushes either a supplied audio file or a synthetic 2-second
//!     16 kHz PCM16 sine tone, then sends `{"type":"audio.done"}`.
//!   - TTS: sends `{"type":"text.delta"}` + `{"type":"text.done"}` with
//!     the supplied text.
//!   - Logs every inbound frame (type, size, and raw payload for text;
//!     kind + size for binary) to stdout.
//!   - Writes a transcript of the session to
//!     `docs/research/xai-ws-protocol-captures/<timestamp>-<mode>.log`
//!     (bearer token is never written to disk).
//!
//! Deliverable: after running both modes, transcribe observed event
//! type strings into `docs/research/xai-ws-protocol.md` and update
//! spec §5.3 / §5.5. See `docs/xai-speech-integration-spec.md` §10.1.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio::{fs, io::AsyncWriteExt, net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

const STT_WS_URL: &str = "wss://api.x.ai/v1/stt";
const TTS_WS_URL: &str = "wss://api.x.ai/v1/tts";

#[derive(Parser, Debug)]
#[command(name = "xai-spike", about = "xAI WS protocol verification spike")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
    /// Seconds to wait for server to drain after we send audio.done /
    /// text.done before forcing the socket closed.
    #[arg(long, default_value_t = 10)]
    drain_secs: u64,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// Exercise the STT streaming WebSocket.
    SttWs {
        /// Optional path to a WAV/PCM/Opus file. Default: synthetic
        /// 2-second 16 kHz PCM16 440 Hz sine tone.
        #[arg(long)]
        audio: Option<PathBuf>,
        /// Audio format hint appended as a query param to the URL.
        /// xAI docs don't enumerate these; common candidates are
        /// "pcm_s16le_16000", "pcm", "opus".
        #[arg(long)]
        audio_format: Option<String>,
        /// Language hint (ISO 639-1).
        #[arg(long, default_value = "en")]
        language: String,
    },
    /// Exercise the TTS streaming WebSocket.
    TtsWs {
        /// Text to synthesize.
        #[arg(long, default_value = "Hello! Welcome to parley.")]
        text: String,
        /// Voice id.
        #[arg(long, default_value = "eve")]
        voice: String,
        /// Codec.
        #[arg(long, default_value = "mp3")]
        codec: String,
        /// Sample rate in Hz.
        #[arg(long, default_value_t = 24000)]
        sample_rate: u32,
        /// Bit rate in bps (mp3 only).
        #[arg(long, default_value_t = 128000)]
        bit_rate: u32,
        #[arg(long, default_value = "en")]
        language: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xai_spike=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let api_key =
        std::env::var("PARLEY_XAI_API_KEY").context("PARLEY_XAI_API_KEY env var required")?;

    let captures_dir = Path::new("docs/research/xai-ws-protocol-captures");
    fs::create_dir_all(captures_dir).await.ok();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match cli.mode {
        Mode::SttWs {
            audio,
            audio_format,
            language,
        } => {
            let log_path = captures_dir.join(format!("{ts}-stt.log"));
            run_stt(
                &api_key,
                audio.as_deref(),
                audio_format.as_deref(),
                &language,
                cli.drain_secs,
                &log_path,
            )
            .await?;
            tracing::info!(?log_path, "capture written");
        }
        Mode::TtsWs {
            text,
            voice,
            codec,
            sample_rate,
            bit_rate,
            language,
        } => {
            let log_path = captures_dir.join(format!("{ts}-tts.log"));
            run_tts(
                &api_key,
                &text,
                &voice,
                &codec,
                sample_rate,
                bit_rate,
                &language,
                cli.drain_secs,
                &log_path,
            )
            .await?;
            tracing::info!(?log_path, "capture written");
        }
    }
    Ok(())
}

async fn connect(url: &str, api_key: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {api_key}")
            .parse()
            .map_err(|e| anyhow!("bearer header parse: {e}"))?,
    );
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, resp)) => {
            tracing::info!(status = %resp.status(), "connected");
            for (name, value) in resp.headers() {
                tracing::info!(header = %name, value = ?value, "response header");
            }
            Ok(ws)
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            tracing::error!(status = %resp.status(), "handshake rejected");
            for (name, value) in resp.headers() {
                tracing::error!(header = %name, value = ?value, "response header");
            }
            if let Some(body) = resp.body() {
                let txt = String::from_utf8_lossy(body);
                tracing::error!(body = %txt, "response body");
            } else {
                tracing::error!("no response body");
            }
            Err(anyhow!("WS handshake failed: {}", resp.status()))
        }
        Err(e) => Err(anyhow!("WS connect error: {e}")),
    }
}

async fn run_stt(
    api_key: &str,
    audio_path: Option<&Path>,
    audio_format: Option<&str>,
    language: &str,
    drain_secs: u64,
    log_path: &Path,
) -> Result<()> {
    let mut url = format!("{STT_WS_URL}?model=grok-stt&language={language}");
    if let Some(fmt) = audio_format {
        url.push_str(&format!("&audio_format={fmt}"));
    }
    tracing::info!(%url, "opening STT WS");

    let mut ws = connect(&url, api_key).await?;
    let mut log = fs::File::create(log_path).await?;
    log.write_all(format!("URL: {url}\n---\n").as_bytes())
        .await?;

    let audio = match audio_path {
        Some(p) => fs::read(p).await?,
        None => synth_pcm16_sine(2.0, 16000, 440.0),
    };
    tracing::info!(bytes = audio.len(), "pushing audio");

    for chunk in audio.chunks(4096) {
        ws.send(Message::Binary(chunk.to_vec().into())).await?;
    }
    ws.send(Message::Text(r#"{"type":"audio.done"}"#.into()))
        .await?;
    log.write_all(b">> sent audio.done\n").await?;

    drain(&mut ws, drain_secs, &mut log).await
}

async fn run_tts(
    api_key: &str,
    text: &str,
    voice: &str,
    codec: &str,
    sample_rate: u32,
    bit_rate: u32,
    language: &str,
    drain_secs: u64,
    log_path: &Path,
) -> Result<()> {
    let url = format!(
        "{TTS_WS_URL}?language={language}&voice={voice}&codec={codec}&sample_rate={sample_rate}&bit_rate={bit_rate}"
    );
    tracing::info!(%url, "opening TTS WS");

    let mut ws = connect(&url, api_key).await?;
    let mut log = fs::File::create(log_path).await?;
    log.write_all(format!("URL: {url}\n---\n").as_bytes())
        .await?;

    let payload = format!(r#"{{"type":"text.delta","delta":{}}}"#, json_string(text));
    ws.send(Message::Text(payload.into())).await?;
    ws.send(Message::Text(r#"{"type":"text.done"}"#.into()))
        .await?;
    log.write_all(b">> sent text.delta + text.done\n").await?;

    drain(&mut ws, drain_secs, &mut log).await
}

async fn drain(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    drain_secs: u64,
    log: &mut fs::File,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(drain_secs);
    let b64 = base64::engine::general_purpose::STANDARD;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::info!("drain deadline reached");
            break;
        }
        match timeout(remaining, ws.next()).await {
            Err(_) => {
                tracing::info!("drain deadline reached");
                break;
            }
            Ok(None) => {
                tracing::info!("stream ended");
                log.write_all(b"<< stream closed\n").await?;
                break;
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(error = %e, "ws error");
                log.write_all(format!("<< ERR {e}\n").as_bytes()).await?;
                break;
            }
            Ok(Some(Ok(msg))) => match msg {
                Message::Text(t) => {
                    tracing::info!(len = t.len(), body = %t, "<< text");
                    log.write_all(format!("<< TEXT {t}\n").as_bytes()).await?;
                }
                Message::Binary(b) => {
                    let sample = &b[..b.len().min(32)];
                    tracing::info!(len = b.len(), head = ?sample, "<< binary");
                    log.write_all(
                        format!("<< BIN {} bytes b64_head={}\n", b.len(), b64.encode(sample))
                            .as_bytes(),
                    )
                    .await?;
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(frame) => {
                    tracing::info!(?frame, "<< close");
                    log.write_all(format!("<< CLOSE {frame:?}\n").as_bytes())
                        .await?;
                    break;
                }
                Message::Frame(_) => {}
            },
        }
    }
    log.flush().await?;
    Ok(())
}

fn synth_pcm16_sine(seconds: f32, sample_rate: u32, freq_hz: f32) -> Vec<u8> {
    let samples = (seconds * sample_rate as f32) as usize;
    let mut out = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let t = i as f32 / sample_rate as f32;
        let v = (t * freq_hz * std::f32::consts::TAU).sin();
        let s = (v * i16::MAX as f32 * 0.5) as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
