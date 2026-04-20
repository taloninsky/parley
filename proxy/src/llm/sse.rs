//! Server-Sent Events parsing for streaming LLM responses.
//!
//! This module is intentionally **provider-neutral at the byte layer**:
//! it takes a stream of byte chunks (as reqwest yields them) and emits
//! a stream of complete `(event, data)` SSE frames. Provider-specific
//! interpretation (mapping `content_block_delta` to `ChatToken::TextDelta`,
//! etc.) lives next to the provider impl.
//!
//! ## Why it's split out
//!
//! Streaming is the part of provider integration that's easiest to get
//! wrong — chunk boundaries can split a frame in half, blank lines
//! demarcate frames, and dispatchable "events" come from named
//! `event:` lines. By isolating the byte→frame transform behind a
//! pure-function-ish API we can unit-test it without a network call.
//!
//! ## SSE shape (RFC-relevant subset)
//!
//! Each SSE frame is one or more `field: value` lines terminated by a
//! blank line:
//!
//! ```text
//! event: message_start
//! data: {"type":"message_start", ...}
//!
//! event: content_block_delta
//! data: {"type":"content_block_delta","delta":{"text":"Hello"}}
//!
//! ```
//!
//! We only need the `event` and `data` fields. `id`, `retry`, and
//! comments (`:`) are ignored.

use std::collections::VecDeque;

/// One decoded SSE frame: an optional event name plus its data payload.
/// Anthropic uses both fields; a provider that omits `event:` lines
/// simply leaves it `None` and dispatches based on the JSON shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field if present.
    pub event: Option<String>,
    /// The concatenated `data:` field. Multiple `data:` lines in a
    /// single frame are joined with `\n` per the spec.
    pub data: String,
}

/// Incremental SSE byte-stream decoder. Feed bytes in with `push`;
/// drain complete frames with `next_frame`. Maintains an internal
/// buffer so frames split across chunk boundaries are reassembled
/// correctly.
///
/// This struct is single-threaded and not `Send` across await points;
/// the streaming provider impl owns one and pulls from it inside the
/// stream future.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received but not yet split into lines.
    buffer: String,
    /// Frame fragments currently being accumulated.
    pending_event: Option<String>,
    pending_data: Vec<String>,
    /// Frames that have been fully decoded and are awaiting the
    /// caller.
    ready: VecDeque<SseFrame>,
}

impl SseDecoder {
    /// Construct an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes from the network. UTF-8 invalid bytes are
    /// replaced with the replacement character — providers we care
    /// about are guaranteed UTF-8, but defensive decoding keeps the
    /// stream alive instead of erroring on a single bad byte.
    pub fn push(&mut self, chunk: &[u8]) {
        // Append, splitting on '\n'. A trailing partial line stays in
        // self.buffer for the next push.
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        while let Some(idx) = self.buffer.find('\n') {
            // Take through the newline; line excludes the newline
            // (and its preceding \r if any).
            let mut line = self.buffer[..idx].to_string();
            self.buffer.drain(..=idx);
            if line.ends_with('\r') {
                line.pop();
            }
            self.ingest_line(&line);
        }
    }

    /// Pop the next fully-decoded frame, if any.
    pub fn next_frame(&mut self) -> Option<SseFrame> {
        self.ready.pop_front()
    }

    /// True when no more frames are buffered. The byte buffer may
    /// still hold a partial line.
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    fn ingest_line(&mut self, line: &str) {
        if line.is_empty() {
            // Blank line dispatches the in-progress frame.
            if self.pending_event.is_some() || !self.pending_data.is_empty() {
                let frame = SseFrame {
                    event: self.pending_event.take(),
                    data: self.pending_data.join("\n"),
                };
                self.pending_data.clear();
                self.ready.push_back(frame);
            }
            return;
        }
        if line.starts_with(':') {
            // Comment per SSE spec — ignored.
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // Field with no value: spec says treat the whole line as
            // the field name with empty value. We don't use those
            // fields, so it's safe to ignore.
            None => return,
        };
        match field {
            "event" => self.pending_event = Some(value.to_string()),
            "data" => self.pending_data.push(value.to_string()),
            // We don't use `id` or `retry`; ignore.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(d: &mut SseDecoder) -> Vec<SseFrame> {
        let mut out = Vec::new();
        while let Some(f) = d.next_frame() {
            out.push(f);
        }
        out
    }

    #[test]
    fn parses_single_complete_frame() {
        let mut d = SseDecoder::new();
        d.push(b"event: ping\ndata: hello\n\n");
        let frames = drain(&mut d);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("ping".into()),
                data: "hello".into(),
            }]
        );
    }

    #[test]
    fn frame_split_across_chunks_is_reassembled() {
        let mut d = SseDecoder::new();
        d.push(b"event: ping\nda");
        assert!(d.is_empty());
        d.push(b"ta: hel");
        assert!(d.is_empty());
        d.push(b"lo\n\n");
        let frames = drain(&mut d);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("ping".into()),
                data: "hello".into(),
            }]
        );
    }

    #[test]
    fn multiple_frames_in_one_push() {
        let mut d = SseDecoder::new();
        d.push(b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n");
        let frames = drain(&mut d);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[0].data, "1");
        assert_eq!(frames[1].event.as_deref(), Some("b"));
        assert_eq!(frames[1].data, "2");
    }

    #[test]
    fn data_only_frame_has_none_event() {
        let mut d = SseDecoder::new();
        d.push(b"data: lonely\n\n");
        let frames = drain(&mut d);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "lonely".into(),
            }]
        );
    }

    #[test]
    fn multiple_data_lines_join_with_newline() {
        let mut d = SseDecoder::new();
        d.push(b"data: line1\ndata: line2\ndata: line3\n\n");
        let frames = drain(&mut d);
        assert_eq!(frames[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let mut d = SseDecoder::new();
        d.push(b": this is a comment\nid: 42\nretry: 1000\nevent: x\ndata: y\n\n");
        let frames = drain(&mut d);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("x".into()),
                data: "y".into(),
            }]
        );
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let mut d = SseDecoder::new();
        d.push(b"event: ping\r\ndata: hello\r\n\r\n");
        let frames = drain(&mut d);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("ping".into()),
                data: "hello".into(),
            }]
        );
    }

    #[test]
    fn data_without_leading_space_is_preserved() {
        // SSE spec: strip exactly one leading space after the colon if
        // present. "data:hello" should preserve "hello".
        let mut d = SseDecoder::new();
        d.push(b"data:hello\n\n");
        assert_eq!(d.next_frame().unwrap().data, "hello");
    }

    #[test]
    fn anthropic_style_streaming_payload_decodes() {
        // Realistic shape of the first few frames from Anthropic's
        // /v1/messages SSE stream (truncated for brevity).
        let payload = b"event: message_start\n\
                        data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\
                        \n\
                        event: content_block_start\n\
                        data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
                        \n\
                        event: content_block_delta\n\
                        data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
                        \n\
                        event: content_block_delta\n\
                        data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
                        \n\
                        event: message_delta\n\
                        data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\
                        \n\
                        event: message_stop\n\
                        data: {\"type\":\"message_stop\"}\n\
                        \n";
        let mut d = SseDecoder::new();
        d.push(payload);
        let frames = drain(&mut d);
        assert_eq!(frames.len(), 6);
        let events: Vec<_> = frames.iter().map(|f| f.event.as_deref()).collect();
        assert_eq!(
            events,
            vec![
                Some("message_start"),
                Some("content_block_start"),
                Some("content_block_delta"),
                Some("content_block_delta"),
                Some("message_delta"),
                Some("message_stop"),
            ]
        );
        // Data on each frame must be valid JSON.
        for f in &frames {
            serde_json::from_str::<serde_json::Value>(&f.data)
                .unwrap_or_else(|e| panic!("bad json in frame: {e} → {}", f.data));
        }
    }

    #[test]
    fn no_blank_line_means_no_frame_emitted_yet() {
        let mut d = SseDecoder::new();
        d.push(b"event: ping\ndata: hello\n");
        // Only one newline — frame not yet dispatched.
        assert!(d.is_empty());
        d.push(b"\n");
        // Now blank line arrives and the frame dispatches.
        assert_eq!(d.next_frame().unwrap().event.as_deref(), Some("ping"));
    }
}
