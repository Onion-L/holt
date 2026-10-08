//! Server-sent event decoding ported from the shared SSE reader in
//! `pi-core/ai/src/api/anthropic-messages.ts` (`iterateSseMessages` and its
//! decoder state machine). All stream providers reuse the same semantics:
//! blank lines flush the pending event, `event:`/`data:` fields accumulate,
//! comment lines (`:` prefix) are ignored, and `\r\n`/`\r`/`\n` all terminate
//! lines.

use crate::telemetry::lock;
use futures::StreamExt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Port of `ServerSentEvent`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    pub raw: Vec<String>,
}

#[derive(Default)]
struct SseDecoderState {
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl SseDecoderState {
    /// Port of `flushSseEvent`.
    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        let event = ServerSentEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
            raw: self.raw.clone(),
        };
        self.data.clear();
        self.raw.clear();
        Some(event)
    }

    /// Port of `decodeSseLine`.
    fn decode_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }

        self.raw.push(line.to_string());
        if line.starts_with(':') {
            return None;
        }

        let (field_name, value) = match line.find(':') {
            Some(index) => {
                let mut value = &line[index + 1..];
                if let Some(stripped) = value.strip_prefix(' ') {
                    value = stripped;
                }
                (&line[..index], value)
            }
            None => (line, ""),
        };

        if field_name == "event" {
            self.event = Some(value.to_string());
        } else if field_name == "data" {
            self.data.push(value.to_string());
        }

        None
    }
}

/// Streaming SSE decoder fed byte chunks; yields complete events. The flush
/// semantics mirror the TypeScript iterator: trailing partial lines decode,
/// and a final flush emits any pending event.
#[derive(Default)]
pub struct SseDecoder {
    state: SseDecoderState,
    buffer: String,
    byte_buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes the next complete line if one is buffered. The `bool` reports
    /// whether a line was consumed; the event is present when the line
    /// completed an SSE event (blank line flush).
    fn consume_line(&mut self) -> (bool, Option<ServerSentEvent>) {
        let Some(line_break_index) = next_line_break_index(&self.buffer) else {
            return (false, None);
        };
        let line: String = self.buffer[..line_break_index].to_string();
        let mut next_index = line_break_index + 1;
        if self.buffer[line_break_index..].starts_with("\r\n") {
            next_index += 1;
        }
        self.buffer = self.buffer[next_index..].to_string();
        (true, self.state.decode_line(&line))
    }

    /// Feeds a decoded text chunk; returns any completed events in order.
    pub fn push_text(&mut self, text: &str) -> Vec<ServerSentEvent> {
        self.buffer.push_str(text);
        let mut events = Vec::new();
        loop {
            let (consumed, event) = self.consume_line();
            if let Some(event) = event {
                events.push(event);
            }
            if !consumed {
                break;
            }
        }
        events
    }

    /// Feeds raw bytes with incremental UTF-8 handling, mirroring
    /// `TextDecoder.decode(value, { stream: true })`: incomplete trailing
    /// sequences are buffered until more bytes arrive.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<ServerSentEvent> {
        let mut scratch = std::mem::take(&mut self.byte_buffer);
        scratch.extend_from_slice(bytes);
        let mut events = Vec::new();
        while !scratch.is_empty() {
            match std::str::from_utf8(&scratch) {
                Ok(text) => {
                    let text = text.to_string();
                    scratch.clear();
                    events.extend(self.push_text(&text));
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid == 0 {
                        // Incomplete multi-byte sequence at the chunk
                        // boundary; wait for more bytes.
                        break;
                    }
                    let text = String::from_utf8_lossy(&scratch[..valid]).to_string();
                    scratch.drain(..valid);
                    events.extend(self.push_text(&text));
                }
            }
        }
        self.byte_buffer = scratch;
        events
    }

    /// Ends the stream: decodes any buffered line and flushes the trailing
    /// event, mirroring the iterator's tail handling. Incomplete UTF-8
    /// sequences decode lossily, matching the final `decoder.decode()`.
    pub fn finish(&mut self) -> Vec<ServerSentEvent> {
        let mut events = Vec::new();
        if !self.byte_buffer.is_empty() {
            let remainder =
                String::from_utf8_lossy(&std::mem::take(&mut self.byte_buffer)).to_string();
            events.extend(self.push_text(&remainder));
        }
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(event) = self.state.decode_line(&line) {
                events.push(event);
            }
        }
        if let Some(event) = self.state.flush() {
            events.push(event);
        }
        events
    }
}

fn next_line_break_index(text: &str) -> Option<usize> {
    text.find(['\r', '\n'])
}

/// An async SSE line iterator over a byte stream, mirroring
/// `iterateSseMessages`. Cancelled when the token cancels (surfacing "Request
/// was aborted").
pub struct SseStream {
    decoder: SseDecoder,
    body: futures::stream::BoxStream<'static, Result<bytes::Bytes, super::http::HttpFetchError>>,
    buffer: Arc<Mutex<SseBuffer>>,
}

#[derive(Default)]
struct SseBuffer {
    events: VecDeque<ServerSentEvent>,
    error: Option<String>,
    done: bool,
    wakers: VecDeque<Waker>,
}

impl SseStream {
    pub(crate) fn with_signal(
        body: futures::stream::BoxStream<
            'static,
            Result<bytes::Bytes, super::http::HttpFetchError>,
        >,
        signal: Option<tokio_util::sync::CancellationToken>,
    ) -> Self {
        Self::new(super::http::abortable_body(body, signal))
    }

    /// Wraps a response body into an SSE event stream.
    pub fn new(
        body: futures::stream::BoxStream<
            'static,
            Result<bytes::Bytes, super::http::HttpFetchError>,
        >,
    ) -> Self {
        Self {
            decoder: SseDecoder::new(),
            body,
            buffer: Arc::new(Mutex::new(SseBuffer::default())),
        }
    }

    /// Polls the next event, driving the byte stream.
    fn poll_next_impl(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerSentEvent>> {
        loop {
            {
                let mut buffer = lock(&self.buffer);
                if let Some(event) = buffer.events.pop_front() {
                    return Poll::Ready(Some(event));
                }
                if buffer.done {
                    return Poll::Ready(None);
                }
                if let Some(error) = &buffer.error {
                    let error = error.clone();
                    buffer.error = None;
                    buffer.done = true;
                    drop(buffer);
                    return Poll::Ready(Some(ServerSentEvent {
                        event: Some("__error__".to_string()),
                        data: error,
                        raw: Vec::new(),
                    }));
                }
                buffer.wakers.push_back(cx.waker().clone());
            }

            match futures::ready!(self.body.poll_next_unpin(cx)) {
                Some(Ok(chunk)) => {
                    let events = self.decoder.push_bytes(&chunk);
                    let mut buffer = lock(&self.buffer);
                    let has_waiters = !buffer.wakers.is_empty();
                    for event in events {
                        buffer.events.push_back(event);
                    }
                    if has_waiters && let Some(waker) = buffer.wakers.pop_front() {
                        waker.wake();
                    }
                }
                Some(Err(error)) => {
                    let mut buffer = lock(&self.buffer);
                    buffer.error = Some(error.to_string());
                    for waker in buffer.wakers.drain(..) {
                        waker.wake();
                    }
                }
                None => {
                    let events = self.decoder.finish();
                    let mut buffer = lock(&self.buffer);
                    for event in events {
                        buffer.events.push_back(event);
                    }
                    buffer.done = true;
                    for waker in buffer.wakers.drain(..) {
                        waker.wake();
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

impl futures::Stream for SseStream {
    type Item = ServerSentEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.poll_next_impl(cx)
    }
}

/// Collects every remaining SSE event (test helper).
pub async fn collect_sse_events(mut stream: SseStream) -> Vec<ServerSentEvent> {
    let mut events = Vec::new();
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        events.push(event);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&str]) -> Vec<ServerSentEvent> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push_text(chunk));
        }
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn decodes_basic_events() {
        let events = decode_all(&["event: message_start\ndata: {\"a\":1}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(
            events[0].raw,
            vec!["event: message_start", "data: {\"a\":1}"]
        );
    }

    #[test]
    fn joins_multiple_data_lines_with_newlines() {
        let events = decode_all(&["data: line1\ndata: line2\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
        assert_eq!(events[0].event, None);
    }

    #[test]
    fn ignores_comment_lines_and_preserves_them_in_raw() {
        let events = decode_all(&[": keep-alive\ndata: x\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
        assert_eq!(events[0].raw, vec![": keep-alive", "data: x"]);
    }

    #[test]
    fn handles_crlf_and_cr_line_breaks() {
        let events = decode_all(&["event: a\r\ndata: 1\r\n\r"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[0].data, "1");
    }

    #[test]
    fn splits_chunks_at_arbitrary_boundaries() {
        let events = decode_all(&["event: x\nda", "ta: value\n", "\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("x"));
        assert_eq!(events[0].data, "value");
    }

    #[test]
    fn flushes_trailing_event_without_final_blank_line() {
        let events = decode_all(&["event: ping\ndata: 123"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "123");
    }

    #[test]
    fn empty_events_are_not_emitted() {
        let events = decode_all(&["\n\n"]);
        assert!(events.is_empty());
    }

    #[test]
    fn value_space_prefix_is_stripped_once() {
        let events = decode_all(&["data:  spaced\n\n"]);
        assert_eq!(events[0].data, " spaced");
    }

    #[test]
    fn field_without_colon_has_empty_value() {
        let events = decode_all(&["data\n\n"]);
        assert_eq!(events[0].data, "");
    }

    #[tokio::test]
    async fn sse_stream_yields_events_from_byte_chunks() {
        let chunks: Vec<Result<bytes::Bytes, super::super::http::HttpFetchError>> = vec![
            Ok(bytes::Bytes::from(
                "event: message_start\ndata: {\"type\":\"message",
            )),
            Ok(bytes::Bytes::from(
                "_start\"}\n\nevent: message_stop\ndata: {}\n\n",
            )),
        ];
        let body = futures::stream::iter(chunks);
        let stream = SseStream::new(Box::pin(body));
        let events = collect_sse_events(stream).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"type\":\"message_start\"}");
        assert_eq!(events[1].event.as_deref(), Some("message_stop"));
    }

    #[tokio::test]
    async fn sse_stream_decodes_multibyte_characters_split_across_chunks() {
        // "你好" splits the middle byte of 你 across the chunk boundary.
        let full = "data: 你好\n\n";
        let bytes = full.as_bytes();
        let split = 9; // inside the 3-byte 你
        let chunks: Vec<Result<bytes::Bytes, super::super::http::HttpFetchError>> = vec![
            Ok(bytes::Bytes::copy_from_slice(&bytes[..split])),
            Ok(bytes::Bytes::copy_from_slice(&bytes[split..])),
        ];
        let body = futures::stream::iter(chunks);
        let stream = SseStream::new(Box::pin(body));
        let events = collect_sse_events(stream).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "你好");
    }
}
