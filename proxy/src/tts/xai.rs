//! xAI `grok-tts` implementation of [`TtsProvider`].
//!
//! The unary REST path remains available as a fallback. The turn-level
//! WebSocket path is the prosody-oriented path: it accepts `text.delta`
//! frames and returns `audio.delta` frames inside one continuous xAI
//! synthesis session.
//!
//! We pin the response codec to `Mp3_44100_128` so the existing
//! [`crate::tts::silence`] splicer keeps working unchanged across
//! providers. xAI supports other codecs and sample rates; surfacing
//! them waits until [`AudioFormat`] grows matching variants.
//!
//! ## Endpoint shape (§5.4)
//!
//! ```text
//! POST https://api.x.ai/v1/tts
//! Headers: Authorization: Bearer <KEY>, content-type: application/json
//! Body: {"text":"…","voice_id":"eve","language":"en",
//!        "output_format":{"codec":"mp3","sample_rate":44100,"bit_rate":128000},
//!        "text_normalization":true}
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use async_stream::try_stream;
use async_trait::async_trait;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use parley_core::chat::Cost;
use parley_core::tts::{ChunkPolicy, VoiceDescriptor};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

use super::{
    AudioFormat, SynthesisContext, TtsChunk, TtsError, TtsProvider, TtsRequest, TtsStream,
    TtsTextFrame, TtsTurnStreamRequest, TurnTextStream,
};

/// Default REST endpoint. Constructor takes an override so tests can
/// point at a `wiremock` server.
pub const XAI_TTS_REST_URL: &str = "https://api.x.ai/v1/tts";

/// Default TTS WebSocket endpoint. Unlike REST, this accepts
/// incremental `text.delta` frames for one continuous turn.
pub const XAI_TTS_WS_URL: &str = "wss://api.x.ai/v1/tts";

/// Default voices catalog endpoint (spec §5.6). 24-hour cache TTL
/// matches the server-side contract.
pub const XAI_TTS_VOICES_URL: &str = "https://api.x.ai/v1/tts/voices";

/// TTL for the voices-catalog cache. Spec §5.6 mandates 24h.
pub const XAI_TTS_VOICES_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Pinned MP3 sample rate in Hz — matches [`AudioFormat::Mp3_44100_128`]
/// so the silence splicer can crossfade without re-encoding.
pub const XAI_TTS_SAMPLE_RATE: u32 = 44100;

/// Pinned MP3 bit rate in bits-per-second — matches
/// [`AudioFormat::Mp3_44100_128`].
pub const XAI_TTS_BIT_RATE: u32 = 128_000;

/// Per-character USD cost — $4.20 / 1 M chars. Spec §5 / §5.4 pricing.
pub const XAI_TTS_COST_PER_CHAR_USD: f64 = 4.20 / 1_000_000.0;

/// Default voice. `eve` is the documented default in spec §5.4.
pub const XAI_TTS_DEFAULT_VOICE: &str = "eve";

/// Default BCP-47 language tag sent on every synth request. xAI's
/// `/v1/tts` endpoint requires `language` in the body (it returned
/// 422 "missing field `language`" without it). English is the only
/// language Parley speaks today; surfacing this as a constant keeps
/// the choice explicit.
pub const XAI_TTS_DEFAULT_LANGUAGE: &str = "en";

/// The five canonical xAI voice IDs shipped today (spec §5.4). The
/// voices catalog endpoint (§5.6) can override this at runtime; this
/// list is the known-good baseline used when the catalog hasn't been
/// fetched yet.
pub const XAI_TTS_KNOWN_VOICES: &[&str] = &["eve", "ara", "rex", "sal", "leo"];

/// Concrete xAI implementation of [`TtsProvider`]. One instance per
/// (key, endpoint) pair; cheap to clone.
#[derive(Clone)]
pub struct XaiTts {
    api_key: Arc<str>,
    endpoint: Arc<str>,
    ws_endpoint: Arc<str>,
    voices_endpoint: Arc<str>,
    client: reqwest::Client,
    voices_cache: Arc<Mutex<Option<VoicesCacheEntry>>>,
}

#[derive(Clone)]
struct VoicesCacheEntry {
    fetched_at: Instant,
    voices: Vec<VoiceDescriptor>,
}

impl XaiTts {
    /// Build a provider pointed at the production endpoints.
    pub fn new(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self::with_all_endpoints(
            api_key,
            XAI_TTS_REST_URL,
            XAI_TTS_WS_URL,
            XAI_TTS_VOICES_URL,
            client,
        )
    }

    /// Build a provider with a custom synthesize endpoint. Voices URL
    /// defaults to the production catalog — callers who want to stub
    /// both should use [`Self::with_endpoints`].
    pub fn with_endpoint(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self::with_all_endpoints(
            api_key,
            endpoint,
            XAI_TTS_WS_URL,
            XAI_TTS_VOICES_URL,
            client,
        )
    }

    /// Build a provider with overrides for both the synthesize and
    /// voices-catalog endpoints — used by tests that stand up a
    /// `wiremock` server.
    pub fn with_endpoints(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        voices_endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self::with_all_endpoints(api_key, endpoint, XAI_TTS_WS_URL, voices_endpoint, client)
    }

    /// Build a provider with overrides for synthesize, WebSocket, and
    /// voices endpoints. Tests use this to point the WS path at a
    /// local fixture without changing the REST fixture.
    pub fn with_all_endpoints(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        ws_endpoint: impl Into<String>,
        voices_endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_key: api_key.into().into(),
            endpoint: endpoint.into().into(),
            ws_endpoint: ws_endpoint.into().into(),
            voices_endpoint: voices_endpoint.into().into(),
            client,
            voices_cache: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl TtsProvider for XaiTts {
    fn id(&self) -> &'static str {
        "xai"
    }

    async fn synthesize(
        &self,
        request: TtsRequest,
        _ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError> {
        if request.text.is_empty() {
            return Err(TtsError::Other("empty text".into()));
        }
        let characters = request.text.chars().count() as u32;

        // xAI accepts either a specific voice id or the string "eve" as
        // default. An empty voice_id from the caller means "use default"
        // per the spec's "default eve" note.
        let voice = if request.voice_id.is_empty() {
            XAI_TTS_DEFAULT_VOICE.to_string()
        } else {
            request.voice_id
        };

        let body = json!({
            "text": request.text,
            "voice_id": voice,
            "language": XAI_TTS_DEFAULT_LANGUAGE,
            "output_format": {
                "codec": "mp3",
                "sample_rate": XAI_TTS_SAMPLE_RATE,
                "bit_rate": XAI_TTS_BIT_RATE,
            },
            "text_normalization": true,
        });

        let resp = self
            .client
            .post(self.endpoint.as_ref())
            .bearer_auth(self.api_key.as_ref())
            .header("accept", "audio/mpeg")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| TtsError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let mut bytes = resp.bytes_stream();
        let stream = try_stream! {
            while let Some(next) = bytes.next().await {
                let chunk = next.map_err(|e| TtsError::Transport(e.to_string()))?;
                if !chunk.is_empty() {
                    yield TtsChunk::Audio(chunk.to_vec());
                }
            }
            yield TtsChunk::Done { characters };
        };
        Ok(Box::pin(stream))
    }

    fn supports_turn_text_stream(&self) -> bool {
        true
    }

    async fn open_turn_text_stream(
        &self,
        request: TtsTurnStreamRequest,
    ) -> Result<TurnTextStream, TtsError> {
        let voice = if request.voice_id.is_empty() {
            XAI_TTS_DEFAULT_VOICE.to_string()
        } else {
            request.voice_id
        };
        let url = format!(
            "{}?language={}&voice={}&codec=mp3&sample_rate={}&bit_rate={}",
            self.ws_endpoint,
            XAI_TTS_DEFAULT_LANGUAGE,
            urlencoding::encode(&voice),
            XAI_TTS_SAMPLE_RATE,
            XAI_TTS_BIT_RATE,
        );
        let mut ws_request = url
            .into_client_request()
            .map_err(|e| TtsError::Transport(format!("bad ws url: {e}")))?;
        let bearer = format!("Bearer {}", self.api_key.as_ref())
            .parse()
            .map_err(|e| TtsError::Other(format!("bearer header: {e}")))?;
        ws_request.headers_mut().insert("Authorization", bearer);

        let (ws, _resp) =
            tokio_tungstenite::connect_async(ws_request)
                .await
                .map_err(|e| match e {
                    tokio_tungstenite::tungstenite::Error::Http(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp
                            .body()
                            .as_ref()
                            .map(|b| String::from_utf8_lossy(b).to_string())
                            .unwrap_or_default();
                        TtsError::Http { status, body }
                    }
                    other => TtsError::Transport(other.to_string()),
                })?;
        let (mut ws_sink, mut ws_stream) = ws.split();
        let (text_tx, mut text_rx) = mpsc::channel::<TtsTextFrame>(64);
        let (close_tx, close_rx) = oneshot::channel::<()>();
        let billable_characters = Arc::new(AtomicU32::new(0));
        let forwarded_characters = billable_characters.clone();

        tokio::spawn(async move {
            while let Some(frame) = text_rx.recv().await {
                match frame {
                    TtsTextFrame::Delta(delta) => {
                        forwarded_characters
                            .fetch_add(delta.chars().count() as u32, Ordering::Relaxed);
                        let payload = json!({ "type": "text.delta", "delta": delta });
                        if ws_sink
                            .send(Message::Text(payload.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    TtsTextFrame::Done => break,
                }
            }
            if ws_sink
                .send(Message::Text(json!({ "type": "text.done" }).to_string()))
                .await
                .is_err()
            {
                return;
            }
            let _ = close_rx.await;
            let _ = ws_sink.send(Message::Close(None)).await;
        });

        let mut close_signal = Some(close_tx);
        let stream = try_stream! {
            while let Some(msg) = ws_stream.next().await {
                let msg = msg.map_err(|e| TtsError::Transport(e.to_string()))?;
                match msg {
                    Message::Text(text) => match parse_ws_tts_text(&text)? {
                        XaiWsTtsFrame::Audio(bytes) => {
                            if !bytes.is_empty() {
                                yield TtsChunk::Audio(bytes);
                            }
                        }
                        XaiWsTtsFrame::Done => {
                            if let Some(tx) = close_signal.take() {
                                let _ = tx.send(());
                            }
                            let characters = billable_characters.load(Ordering::Relaxed);
                            yield TtsChunk::Done { characters };
                            break;
                        }
                        XaiWsTtsFrame::Ignore => continue,
                    },
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                }
            }
        };

        Ok(TurnTextStream {
            text_tx,
            audio: Box::pin(stream),
        })
    }

    fn cost(&self, characters: u32) -> Cost {
        Cost::from_usd(characters as f64 * XAI_TTS_COST_PER_CHAR_USD)
    }

    fn output_format(&self) -> AudioFormat {
        // Pinned to 44.1 kHz 128 kbps MP3 so the silence splicer's
        // pre-baked silence frame lines up.
        AudioFormat::Mp3_44100_128
    }

    fn tune_chunk_policy(&self, policy: ChunkPolicy) -> ChunkPolicy {
        let mut tuned = policy;
        tuned.first_chunk_max_sentences = 0;
        tuned.idle_timeout_ms = tuned.idle_timeout_ms.max(tuned.paragraph_wait_ms);
        tuned
    }

    fn expression_tag_instruction(&self) -> Option<String> {
        Some(xai_expression_instruction())
    }

    fn translate_expression_tags(&self, text: &str) -> String {
        translate_for_xai(text)
    }

    async fn voices(&self) -> Result<Vec<VoiceDescriptor>, TtsError> {
        if let Some(cached) = self.voices_from_cache() {
            return Ok(cached);
        }

        let resp = self
            .client
            .get(self.voices_endpoint.as_ref())
            .bearer_auth(self.api_key.as_ref())
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| TtsError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::Http {
                status: status.as_u16(),
                body,
            });
        }

        // Read as text first so we can include a snippet of the raw
        // body in the error when JSON parsing fails. xAI's voices
        // catalog schema is not yet documented and has shifted; the
        // snippet lets us iterate when shape mismatches surface in
        // production.
        let raw = resp
            .text()
            .await
            .map_err(|e| TtsError::Transport(e.to_string()))?;
        let body: VoicesResponse = serde_json::from_str(&raw).map_err(|e| {
            let snippet: String = raw.chars().take(300).collect();
            TtsError::Protocol(format!("voices parse: {e}; body starts with: {snippet}"))
        })?;
        let voices = body.into_descriptors();
        self.store_voices_cache(&voices);
        Ok(voices)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum XaiWsTtsFrame {
    Audio(Vec<u8>),
    Done,
    Ignore,
}

#[derive(Debug, Deserialize)]
struct XaiWsTextFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
}

fn parse_ws_tts_text(text: &str) -> Result<XaiWsTtsFrame, TtsError> {
    let frame: XaiWsTextFrame = serde_json::from_str(text)
        .map_err(|e| TtsError::Protocol(format!("xai tts ws json: {e}")))?;
    match frame.kind.as_str() {
        "audio.delta" => {
            let delta = frame
                .delta
                .ok_or_else(|| TtsError::Protocol("xai tts audio.delta missing delta".into()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(delta)
                .map_err(|e| TtsError::Protocol(format!("xai tts audio.delta base64: {e}")))?;
            Ok(XaiWsTtsFrame::Audio(bytes))
        }
        "audio.done" => Ok(XaiWsTtsFrame::Done),
        _ => Ok(XaiWsTtsFrame::Ignore),
    }
}

impl XaiTts {
    fn voices_from_cache(&self) -> Option<Vec<VoiceDescriptor>> {
        let guard = self.voices_cache.lock().ok()?;
        let entry = guard.as_ref()?;
        if entry.fetched_at.elapsed() < XAI_TTS_VOICES_TTL {
            Some(entry.voices.clone())
        } else {
            None
        }
    }

    fn store_voices_cache(&self, voices: &[VoiceDescriptor]) {
        if let Ok(mut guard) = self.voices_cache.lock() {
            *guard = Some(VoicesCacheEntry {
                fetched_at: Instant::now(),
                voices: voices.to_vec(),
            });
        }
    }
}

/// xAI's voices response (observed live, schema not yet documented):
/// ```json
/// {"voices":[
///   {"voice_id":"ara","name":"Ara","language":"multilingual"},
///   ...
/// ]}
/// ```
/// We project this onto [`VoiceDescriptor`] (which uses `id` /
/// `display_name` / `language_tags`) so the frontend doesn't need to
/// special-case provider shapes.
#[derive(Debug, Deserialize)]
struct VoicesResponse {
    #[serde(default)]
    voices: Vec<VoiceEntry>,
}

#[derive(Debug, Deserialize)]
struct VoiceEntry {
    /// xAI uses `voice_id` (we accept the legacy `id` too in case the
    /// schema unifies later).
    #[serde(alias = "id")]
    voice_id: String,
    /// Human-readable label. xAI uses `name`; older / synthetic
    /// fixtures used `display_name`.
    #[serde(alias = "display_name")]
    #[serde(default)]
    name: Option<String>,
    /// Single BCP-47-or-`multilingual` string from xAI; we lift it to
    /// the descriptor's `language_tags` vec (one entry).
    #[serde(default)]
    language: Option<String>,
    /// Forward-compat: accept the multi-tag form even though xAI
    /// emits the singular today.
    #[serde(default)]
    language_tags: Vec<String>,
    // Accepted for forward-compat even if we don't project it into
    // `VoiceDescriptor` today.
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
}

impl VoicesResponse {
    fn into_descriptors(self) -> Vec<VoiceDescriptor> {
        self.voices
            .into_iter()
            .map(|v| {
                let display_name = v.name.unwrap_or_else(|| title_case(&v.voice_id));
                let language_tags = if !v.language_tags.is_empty() {
                    v.language_tags
                } else if let Some(lang) = v.language {
                    vec![lang]
                } else {
                    Vec::new()
                };
                VoiceDescriptor {
                    id: v.voice_id,
                    display_name,
                    language_tags,
                }
            })
            .collect()
    }
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn xai_expression_instruction() -> String {
    "You may annotate spoken responses with these xAI-compatible expression tags inline. \
     Use them sparingly and only when they enhance meaning; most sentences should carry no tags. \
     Place event tags exactly where the cue should land. Place style tags immediately before the \
     sentence or clause they should color; Parley will render the style through the next strong \
     punctuation boundary.\n\n\
     Available tags:\n\
     - {laugh} — Actual short laugh sound.\n\
     - {sigh} — Short audible exhale. Use sparingly.\n\
     - {pause:short} — Deliberate short beat.\n\
     - {pause:medium} — Deliberate longer beat.\n\
     - {pause:long} — Deliberate long beat.\n\
     - {soft} — Quieter, intimate delivery for the following sentence or clause.\n\
     - {thoughtful} — Slower, considered delivery for the following sentence or clause.\n\
     - {emphasis} — Stress the following word or short phrase.\n\
     - {excited} — Faster, animated delivery for the following sentence or clause.\n\n\
     Example: \"That's a good question. {pause:short} {thoughtful}Let me think about it. \
     {soft}I hear you.\"\n\n\
     Do not invent new tags. Do not nest tags. Do not write provider-native xAI tags like \
     [pause] or <soft>; use only the brace tags above."
        .to_string()
}

/// Map Parley's neutral tags to xAI's native speech-event tags.
///
/// Parley's current neutral expression model only gives us inline point
/// markers. xAI's style controls are scoped wrappers, so selected style cues
/// open a wrapper around the following text unit and close at the next strong
/// punctuation boundary. Broad emotional labels without a close native control
/// still strip rather than pretending we know how to render them.
fn translate_for_xai(text: &str) -> String {
    use parley_core::expression::{Segment, split_into_segments};

    let mut out = String::with_capacity(text.len());
    let mut pending_styles: Vec<&'static str> = Vec::new();
    let mut open_styles: Vec<&'static str> = Vec::new();
    for segment in split_into_segments(text) {
        match segment {
            Segment::Text(text) => {
                push_text_for_xai(text, &mut out, &mut pending_styles, &mut open_styles)
            }
            Segment::Tag(id) => match id {
                "laugh" => out.push_str("[laugh]"),
                "sigh" => out.push_str("[sigh]"),
                "pause:short" => out.push_str("[pause]"),
                "pause:medium" | "pause:long" => out.push_str("[long-pause]"),
                "soft" => pending_styles.push("soft"),
                "emphasis" => pending_styles.push("emphasis"),
                "thoughtful" => pending_styles.push("slow"),
                "excited" => pending_styles.push("fast"),
                _ => {}
            },
        }
    }
    close_styles(&mut out, &mut open_styles);
    out
}

fn push_text_for_xai(
    text: &str,
    out: &mut String,
    pending_styles: &mut Vec<&'static str>,
    open_styles: &mut Vec<&'static str>,
) {
    if open_styles.is_empty() && !pending_styles.is_empty() {
        let body_start = text
            .char_indices()
            .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx));
        let Some(body_start) = body_start else {
            out.push_str(text);
            return;
        };

        out.push_str(&text[..body_start]);
        open_pending_styles(out, pending_styles, open_styles);
        push_text_with_open_styles(&text[body_start..], out, open_styles);
    } else {
        push_text_with_open_styles(text, out, open_styles);
    }
}

fn open_pending_styles(
    out: &mut String,
    pending_styles: &mut Vec<&'static str>,
    open_styles: &mut Vec<&'static str>,
) {
    for style in pending_styles.drain(..) {
        out.push('<');
        out.push_str(style);
        out.push('>');
        open_styles.push(style);
    }
}

fn push_text_with_open_styles(text: &str, out: &mut String, open_styles: &mut Vec<&'static str>) {
    if open_styles.is_empty() {
        out.push_str(text);
        return;
    }

    if let Some(end) = style_scope_end(text) {
        out.push_str(&text[..end]);
        close_styles(out, open_styles);
        out.push_str(&text[end..]);
    } else {
        out.push_str(text);
    }
}

fn style_scope_end(text: &str) -> Option<usize> {
    text.char_indices()
        .find_map(|(idx, ch)| matches!(ch, '.' | '!' | '?' | ';').then_some(idx + ch.len_utf8()))
}

fn close_styles(out: &mut String, open_styles: &mut Vec<&'static str>) {
    while let Some(style) = open_styles.pop() {
        out.push_str("</");
        out.push_str(style);
        out.push('>');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use futures::StreamExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> XaiTts {
        XaiTts::with_endpoint(
            "test-key",
            format!("{}/v1/tts", server.uri()),
            reqwest::Client::new(),
        )
    }

    fn provider_with_ws(ws_url: String) -> XaiTts {
        XaiTts::with_all_endpoints(
            "test-key",
            "http://example.test/v1/tts",
            ws_url,
            "http://example.test/v1/tts/voices",
            reqwest::Client::new(),
        )
    }

    async fn spawn_mock_ws(
        scripted: Vec<Message>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/");

        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut captured = Vec::new();
            while let Some(msg) = ws.next().await {
                match msg.unwrap() {
                    Message::Text(text) => {
                        let done = text.contains(r#""type":"text.done""#);
                        captured.push(text);
                        if done {
                            break;
                        }
                    }
                    Message::Close(_) => return captured,
                    _ => {}
                }
            }
            for msg in scripted {
                ws.send(msg).await.unwrap();
            }
            let _ = ws.next().await;
            captured
        });

        (url, handle)
    }

    fn server_audio(bytes: &[u8]) -> Message {
        Message::Text(
            json!({
                "type": "audio.delta",
                "delta": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .to_string(),
        )
    }

    fn server_done() -> Message {
        Message::Text(json!({ "type": "audio.done", "trace_id": "trace-1" }).to_string())
    }

    #[tokio::test]
    async fn synthesize_emits_audio_then_done() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"\xff\xfb\x90\x00fake-mp3-bytes" as &[u8]),
            )
            .mount(&server)
            .await;

        let mut stream = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "eve".into(),
                    text: "hello world".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();

        let mut audio = Vec::new();
        let mut saw_done = false;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                TtsChunk::Audio(bytes) => audio.extend(bytes),
                TtsChunk::Done { characters } => {
                    assert_eq!(characters, 11);
                    saw_done = true;
                }
            }
        }
        assert!(saw_done);
        assert_eq!(audio, b"\xff\xfb\x90\x00fake-mp3-bytes");
    }

    #[tokio::test]
    async fn synthesize_sends_expected_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"x" as &[u8]),
            )
            .mount(&server)
            .await;

        let mut s = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "rex".into(),
                    text: "ok".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        while let Some(item) = s.next().await {
            item.unwrap();
        }

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body = std::str::from_utf8(&received[0].body).unwrap();
        assert!(body.contains("\"text\":\"ok\""));
        assert!(body.contains("\"voice_id\":\"rex\""));
        assert!(body.contains("\"language\":\"en\""));
        assert!(body.contains("\"codec\":\"mp3\""));
        assert!(body.contains("\"sample_rate\":44100"));
        assert!(body.contains("\"bit_rate\":128000"));
        assert!(body.contains("\"text_normalization\":true"));
    }

    #[tokio::test]
    async fn empty_voice_id_falls_back_to_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"x" as &[u8]),
            )
            .mount(&server)
            .await;

        let mut s = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: String::new(),
                    text: "hi".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        while let Some(item) = s.next().await {
            item.unwrap();
        }
        let received = server.received_requests().await.unwrap();
        let body = std::str::from_utf8(&received[0].body).unwrap();
        assert!(body.contains("\"voice_id\":\"eve\""));
    }

    #[tokio::test]
    async fn http_error_surfaces_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let result = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "eve".into(),
                    text: "hi".into(),
                },
                SynthesisContext::default(),
            )
            .await;
        match result {
            Err(TtsError::Http { status, body }) => {
                assert_eq!(status, 401);
                assert_eq!(body, "bad key");
            }
            Err(other) => panic!("expected Http, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[tokio::test]
    async fn empty_text_is_rejected_locally() {
        let server = MockServer::start().await;
        let result = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "eve".into(),
                    text: String::new(),
                },
                SynthesisContext::default(),
            )
            .await;
        match result {
            Err(TtsError::Other(_)) => {}
            Err(other) => panic!("expected Other, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
        // Mock was never hit.
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn cost_uses_spec_per_char_rate() {
        let p = XaiTts::new("k", reqwest::Client::new());
        // 1_000_000 chars × $4.20/1M = $4.20.
        let c = p.cost(1_000_000);
        assert!((c.usd - 4.20).abs() < 1e-9, "got {}", c.usd);
    }

    #[test]
    fn id_and_format_are_stable() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(p.id(), "xai");
        assert_eq!(p.output_format(), AudioFormat::Mp3_44100_128);
    }

    #[test]
    fn turn_text_stream_capability_is_advertised() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert!(p.supports_turn_text_stream());
    }

    #[test]
    fn parse_ws_tts_text_decodes_audio_delta() {
        let frame = json!({
            "type": "audio.delta",
            "delta": base64::engine::general_purpose::STANDARD.encode(b"mp3"),
        })
        .to_string();
        assert_eq!(
            parse_ws_tts_text(&frame).unwrap(),
            XaiWsTtsFrame::Audio(b"mp3".to_vec())
        );
    }

    #[test]
    fn parse_ws_tts_text_rejects_bad_audio_base64() {
        let frame = json!({ "type": "audio.delta", "delta": "not base64!!" }).to_string();
        match parse_ws_tts_text(&frame) {
            Err(TtsError::Protocol(message)) => assert!(message.contains("base64")),
            other => panic!("expected protocol error, got {other:?}"),
        }
    }

    #[test]
    fn parse_ws_tts_text_marks_audio_done() {
        let frame = json!({ "type": "audio.done", "trace_id": "trace-1" }).to_string();
        assert_eq!(parse_ws_tts_text(&frame).unwrap(), XaiWsTtsFrame::Done);
    }

    #[tokio::test]
    async fn open_turn_text_stream_sends_deltas_and_decodes_audio() {
        let (ws_url, handle) = spawn_mock_ws(vec![server_audio(b"one"), server_done()]).await;
        let session = provider_with_ws(ws_url)
            .open_turn_text_stream(TtsTurnStreamRequest {
                voice_id: "rex".into(),
            })
            .await
            .unwrap();

        session
            .text_tx
            .send(TtsTextFrame::Delta("Hello ".into()))
            .await
            .unwrap();
        session
            .text_tx
            .send(TtsTextFrame::Delta("world.".into()))
            .await
            .unwrap();
        session.text_tx.send(TtsTextFrame::Done).await.unwrap();

        let mut audio_stream = session.audio;
        let mut audio = Vec::new();
        let mut done_characters = None;
        while let Some(item) = audio_stream.next().await {
            match item.unwrap() {
                TtsChunk::Audio(bytes) => audio.extend(bytes),
                TtsChunk::Done { characters } => done_characters = Some(characters),
            }
        }

        assert_eq!(audio, b"one");
        assert_eq!(done_characters, Some(12));
        let captured = handle.await.unwrap();
        assert_eq!(captured.len(), 3);
        assert!(captured[0].contains(r#""type":"text.delta""#));
        assert!(captured[0].contains(r#""delta":"Hello ""#));
        assert!(captured[1].contains(r#""delta":"world.""#));
        assert!(captured[2].contains(r#""type":"text.done""#));
    }

    #[test]
    fn tune_chunk_policy_prefers_paragraph_continuity() {
        let p = XaiTts::new("k", reqwest::Client::new());
        let policy = ChunkPolicy {
            first_chunk_max_sentences: 2,
            paragraph_wait_ms: 3_000,
            idle_timeout_ms: 1_500,
            ..ChunkPolicy::default()
        };

        let tuned = p.tune_chunk_policy(policy);

        assert_eq!(tuned.first_chunk_max_sentences, 0);
        assert_eq!(tuned.paragraph_wait_ms, 3_000);
        assert_eq!(tuned.idle_timeout_ms, 3_000);
    }

    #[test]
    fn expression_instruction_matches_xai_supported_tags() {
        let p = XaiTts::new("k", reqwest::Client::new());
        let instruction = p.expression_tag_instruction().unwrap();
        assert!(instruction.contains("{soft}"));
        assert!(instruction.contains("{thoughtful}"));
        assert!(instruction.contains("{pause:short}"));
        assert!(!instruction.contains("{warm}"));
        assert!(instruction.contains("Do not write provider-native xAI tags"));
    }

    #[test]
    fn translate_expression_tags_maps_native_point_events() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("Hold on {pause:short} I see it. {sigh} Okay {laugh} yes."),
            "Hold on [pause] I see it. [sigh] Okay [laugh] yes.",
        );
    }

    #[test]
    fn translate_expression_tags_maps_medium_and_long_pauses_to_long_pause() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("First {pause:medium} second {pause:long} third"),
            "First [long-pause] second [long-pause] third",
        );
    }

    #[test]
    fn translate_expression_tags_wraps_supported_style_cues() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags(
                "{soft}I hear you. {thoughtful}Let me think. This part is plain."
            ),
            "<soft>I hear you.</soft> <slow>Let me think.</slow> This part is plain.",
        );
    }

    #[test]
    fn translate_expression_tags_wraps_emphasis_and_excited() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags(
                "Please {emphasis}really consider this. {excited}That works!"
            ),
            "Please <emphasis>really consider this.</emphasis> <fast>That works!</fast>",
        );
    }

    #[test]
    fn translate_expression_tags_keeps_style_open_across_point_events() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("{soft}I hear {pause:short} you."),
            "<soft>I hear [pause] you.</soft>",
        );
    }

    #[test]
    fn translate_expression_tags_strips_unsupported_style_cues() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("{warm}Hello.{empathetic} I hear you. {sarcastic}Really."),
            "Hello. I hear you. Really.",
        );
    }

    #[test]
    fn translate_expression_tags_preserves_literal_text_and_malformed_braces() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags(
                r#"Use config {\"voice\":\"eve\"}; then pause {pause:short}."#
            ),
            r#"Use config {\"voice\":\"eve\"}; then pause [pause]."#,
        );
    }

    #[test]
    fn known_voices_contains_documented_five() {
        for id in ["eve", "ara", "rex", "sal", "leo"] {
            assert!(XAI_TTS_KNOWN_VOICES.contains(&id), "missing voice: {id}");
        }
        assert_eq!(XAI_TTS_KNOWN_VOICES.len(), 5);
    }

    // ── voices() catalog ────────────────────────────────────────────

    fn provider_with_voices(server: &MockServer) -> XaiTts {
        XaiTts::with_endpoints(
            "test-key",
            format!("{}/v1/tts", server.uri()),
            format!("{}/v1/tts/voices", server.uri()),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn voices_parses_catalog_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"voices":[
                    {"id":"eve","display_name":"Eve","language_tags":["en-US","es-MX"]},
                    {"id":"rex","language_tags":["en-US"]},
                    {"id":"sal"}
                ]}"#,
            ))
            .mount(&server)
            .await;

        let voices = provider_with_voices(&server).voices().await.unwrap();
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[0].id, "eve");
        assert_eq!(voices[0].display_name, "Eve");
        assert_eq!(voices[0].language_tags, vec!["en-US", "es-MX"]);
        // Missing display_name falls back to title-cased id.
        assert_eq!(voices[1].id, "rex");
        assert_eq!(voices[1].display_name, "Rex");
        // Missing language_tags decodes as empty.
        assert_eq!(voices[2].id, "sal");
        assert_eq!(voices[2].display_name, "Sal");
        assert!(voices[2].language_tags.is_empty());
    }

    #[tokio::test]
    async fn voices_second_call_uses_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"voices":[{"id":"eve"}]}"#),
            )
            .expect(1) // second .voices() must not hit the mock
            .mount(&server)
            .await;

        let p = provider_with_voices(&server);
        let first = p.voices().await.unwrap();
        let second = p.voices().await.unwrap();
        assert_eq!(first, second);
        // `expect(1)` above is the real assertion — wiremock panics on
        // Drop if the mount count doesn't match.
    }

    #[tokio::test]
    async fn voices_surfaces_http_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        match provider_with_voices(&server).voices().await {
            Err(TtsError::Http { status, body }) => {
                assert_eq!(status, 401);
                assert_eq!(body, "bad key");
            }
            Err(other) => panic!("expected Http, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[tokio::test]
    async fn voices_surfaces_protocol_error_on_bad_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("not-json"),
            )
            .mount(&server)
            .await;

        match provider_with_voices(&server).voices().await {
            Err(TtsError::Protocol(msg)) => {
                assert!(msg.contains("voices parse"));
            }
            Err(other) => panic!("expected Protocol, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn title_case_capitalizes_first_char_only() {
        assert_eq!(title_case("eve"), "Eve");
        assert_eq!(title_case(""), "");
        assert_eq!(title_case("e"), "E");
        assert_eq!(title_case("MiXeD"), "MiXeD");
    }
}
