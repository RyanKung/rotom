use crate::{Error, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;

/// Boxed byte stream returned by the upstream HTTP client for SSE responses.
pub type ByteStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>;

/// Parsed server-sent event frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional SSE event name from the `event:` field.
    pub event: Option<String>,
    /// Combined payload from one or more `data:` lines.
    pub data: String,
}

/// Parsed JSON payload plus the original SSE event name when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSseEvent {
    /// Optional SSE event name from the `event:` field.
    pub event: Option<String>,
    /// Parsed JSON payload from the `data:` field.
    pub value: Value,
}

/// Parses a byte stream of SSE frames into JSON payload events.
pub fn json_events(stream: ByteStream) -> impl Stream<Item = Result<Value>> + Send {
    async_stream::try_stream! {
        let mut stream = stream;
        let mut buffer = Vec::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            buffer.extend_from_slice(&bytes);

            // Keep incomplete trailing bytes in the buffer so frames split across
            // network chunks are only parsed once a full separator arrives.
            for event in drain_events_bytes(&mut buffer)? {
                if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                    continue;
                }
                crate::logging::trace_text("upstream.codex.sse_event", &event.data);
                yield serde_json::from_str::<Value>(&event.data)?;
            }
        }

        // Flush a final unterminated frame after the stream closes.
        for event in drain_last_event_bytes(&mut buffer)? {
            if !event.data.trim().is_empty() && event.data.trim() != "[DONE]" {
                crate::logging::trace_text("upstream.codex.sse_event", &event.data);
                yield serde_json::from_str::<Value>(&event.data)?;
            }
        }
    }
}

/// Parses a byte stream of SSE frames into named JSON payload events.
pub fn json_named_events(stream: ByteStream) -> impl Stream<Item = Result<JsonSseEvent>> + Send {
    async_stream::try_stream! {
        let mut stream = stream;
        let mut buffer = Vec::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            buffer.extend_from_slice(&bytes);

            for event in drain_events_bytes(&mut buffer)? {
                if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                    continue;
                }
                crate::logging::trace_text("upstream.codex.sse_event", &event.data);
                yield JsonSseEvent {
                    event: event.event,
                    value: serde_json::from_str::<Value>(&event.data)?,
                };
            }
        }

        for event in drain_last_event_bytes(&mut buffer)? {
            if !event.data.trim().is_empty() && event.data.trim() != "[DONE]" {
                crate::logging::trace_text("upstream.codex.sse_event", &event.data);
                yield JsonSseEvent {
                    event: event.event,
                    value: serde_json::from_str::<Value>(&event.data)?,
                };
            }
        }
    }
}

/// Drains every complete SSE frame currently buffered, leaving any partial tail in place.
pub fn drain_events(buffer: &mut String) -> Vec<SseEvent> {
    let mut events = Vec::new();
    while let Some(index) = find_frame_end(buffer) {
        let frame = buffer[..index].to_owned();
        let next = if buffer[index..].starts_with("\r\n\r\n") {
            index + 4
        } else {
            index + 2
        };
        buffer.drain(..next);
        if let Some(event) = parse_frame(&frame) {
            events.push(event);
        }
    }
    events
}

fn drain_events_bytes(buffer: &mut Vec<u8>) -> Result<Vec<SseEvent>> {
    let mut events = Vec::new();
    while let Some(index) = find_frame_end_bytes(buffer) {
        let frame = buffer[..index].to_vec();
        let next = if starts_with_bytes(buffer, index, b"\r\n\r\n") {
            index + 4
        } else {
            index + 2
        };
        buffer.drain(..next);
        if let Some(event) = parse_frame_bytes(&frame)? {
            events.push(event);
        }
    }
    Ok(events)
}

fn drain_last_event_bytes(buffer: &mut Vec<u8>) -> Result<Vec<SseEvent>> {
    if buffer.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }

    let frame = std::mem::take(buffer);
    Ok(parse_frame_bytes(&frame)?.into_iter().collect())
}

fn find_frame_end(buffer: &str) -> Option<usize> {
    match (buffer.find("\n\n"), buffer.find("\r\n\r\n")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_frame_end_bytes(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .into_iter()
        .chain(buffer.windows(4).position(|window| window == b"\r\n\r\n"))
        .min()
}

fn starts_with_bytes(buffer: &[u8], index: usize, needle: &[u8]) -> bool {
    buffer
        .get(index..index.saturating_add(needle.len()))
        .is_some_and(|slice| slice == needle)
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = Vec::new();

    for line in frame.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_owned());
        }
    }

    (!data.is_empty()).then(|| SseEvent {
        event,
        data: data.join("\n"),
    })
}

fn parse_frame_bytes(frame: &[u8]) -> Result<Option<SseEvent>> {
    let text =
        std::str::from_utf8(frame).map_err(|_| Error::upstream("upstream SSE was not UTF-8"))?;
    Ok(parse_frame(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_complete_frames_and_keeps_partial() {
        let mut buffer = "data: {\"a\":1}\n\n".to_owned();
        buffer.push_str("data: {\"b\":");

        let events = drain_events(&mut buffer);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(buffer, "data: {\"b\":");
    }

    #[test]
    fn combines_multiline_data() {
        let mut buffer = "event: message\ndata: hello\ndata: world\n\n".to_owned();
        let events = drain_events(&mut buffer);

        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message".into()),
                data: "hello\nworld".into()
            }]
        );
    }

    #[test]
    fn drains_utf8_frame_split_across_byte_chunks() {
        let mut buffer = b"data: {\"text\":\"".to_vec();
        buffer.extend_from_slice(&[0xE4, 0xBD]);
        assert_eq!(
            drain_events_bytes(&mut buffer).unwrap(),
            Vec::<SseEvent>::new()
        );

        buffer.extend_from_slice(&[0xA0, 0xE5, 0xA5, 0xBD]);
        buffer.extend_from_slice(b"\"}\n\n");
        let events = drain_events_bytes(&mut buffer).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"text\":\"你好\"}");
    }
}
