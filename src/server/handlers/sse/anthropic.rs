use super::output_index;
use crate::{
    anthropic::{
        ImageSource, ResponseUsage, content_block_stop, error_body, image_block_start,
        message_delta_event, message_start_event, message_stop_event, signature_delta,
        text_block_start, text_delta, thinking_block_start, thinking_delta, tool_block_start,
        tool_json_delta,
    },
    codex::events::{event_error, finish_reason, is_done_event},
    error::Result,
    openai::response::generated_images_from_output,
    openai::types::{FunctionCall, ToolCall},
};
use axum::response::{
    IntoResponse,
    sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    convert::Infallible,
    pin::Pin,
};

trait StreamEventMapper {
    fn start_events(&self, stream_id: &str, model: &str) -> Result<Vec<Event>>;

    fn map_event(&mut self, item: &Value) -> Result<AnthropicStreamStep>;

    fn finish_events(&mut self) -> Result<Vec<Event>>;
}

pub(in crate::server::handlers) fn anthropic_raw_messages_sse_response(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    model: String,
    input_tokens: u32,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let id = format!("msg_{}", rand::random::<u64>());
        let mut stream = stream;
        let mut state = AnthropicRawStreamState::new(input_tokens);

        match state.start_events(&id, &model) {
            Ok(events) => {
                for event in events {
                    yield Ok(event);
                }
            }
            Err(error) => {
                yield Ok(anthropic_error_event(&error));
                return;
            }
        }

        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => match state.map_event(&item.value) {
                    Ok(step) => {
                        for event in step.events {
                            yield Ok(event);
                        }
                        if step.finished {
                            return;
                        }
                    }
                    Err(error) => {
                        yield Ok(anthropic_error_event(&error));
                        return;
                    }
                },
                Err(error) => {
                    yield Ok(anthropic_error_event(&error));
                    return;
                }
            }
        }

        match state.finish_events() {
            Ok(events) => {
                for event in events {
                    yield Ok(event);
                }
            }
            Err(error) => {
                yield Ok(anthropic_error_event(&error));
                return;
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

struct AnthropicStreamStep {
    events: Vec<Event>,
    finished: bool,
}

struct AnthropicRawStreamState {
    open_text_blocks: BTreeSet<u32>,
    open_tool_blocks: BTreeSet<u32>,
    open_thinking_blocks: BTreeSet<u32>,
    seen_tool_blocks: BTreeSet<u32>,
    /// Exact argument bytes already sent for each tool block index.
    emitted_tool_arguments: HashMap<u32, String>,
    seen_thinking_text_blocks: BTreeSet<u32>,
    tool_meta: HashMap<String, ToolMeta>,
    usage: ResponseUsage,
}

impl AnthropicRawStreamState {
    fn new(input_tokens: u32) -> Self {
        Self {
            open_text_blocks: BTreeSet::new(),
            open_tool_blocks: BTreeSet::new(),
            open_thinking_blocks: BTreeSet::new(),
            seen_tool_blocks: BTreeSet::new(),
            emitted_tool_arguments: HashMap::new(),
            seen_thinking_text_blocks: BTreeSet::new(),
            tool_meta: HashMap::new(),
            usage: ResponseUsage {
                input_tokens,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                output_tokens: 0,
                server_tool_use: None,
            },
        }
    }
}

impl StreamEventMapper for AnthropicRawStreamState {
    fn start_events(&self, id: &str, model: &str) -> Result<Vec<Event>> {
        Ok(vec![message_start_event(
            id,
            model,
            self.usage.input_tokens,
        )?])
    }

    fn map_event(&mut self, item: &Value) -> Result<AnthropicStreamStep> {
        if let Some(message) = event_error(item) {
            return Err(crate::Error::upstream(message));
        }

        let mut events = self.lifecycle_events(item)?;
        if is_done_event(item) {
            events.extend(self.done_events(item)?);
            return Ok(AnthropicStreamStep {
                events,
                finished: true,
            });
        }

        Ok(AnthropicStreamStep {
            events,
            finished: false,
        })
    }

    fn finish_events(&mut self) -> Result<Vec<Event>> {
        let mut events = self.close_open_blocks()?;
        events.push(message_delta_event("stop", &self.usage)?);
        events.push(message_stop_event()?);
        Ok(events)
    }
}

impl AnthropicRawStreamState {
    fn lifecycle_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        match item.get("type").and_then(Value::as_str) {
            Some("ping") => Ok(vec![
                Event::default().event("ping").data("{\"type\":\"ping\"}"),
            ]),
            Some("response.output_text.delta") => self.text_delta_events(item),
            Some("response.output_text.done") => self.text_done_events(item),
            Some("response.output_item.added") => self.output_item_added_events(item),
            Some("response.function_call_arguments.delta") => {
                self.tool_arguments_delta_events(item)
            }
            Some("response.function_call_arguments.done") => self.tool_arguments_done_events(item),
            Some("response.output_item.done") => self.output_item_done_events(item),
            Some("response.reasoning_text.delta" | "response.reasoning_summary_text.delta") => {
                self.reasoning_delta_events(item)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn text_delta_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let index = output_index(item);
        let delta = item
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        if self.open_text_blocks.insert(index) {
            events.push(text_block_start(index)?);
        }
        self.add_output_tokens(delta);
        events.push(text_delta(index, delta)?);
        Ok(events)
    }

    fn text_done_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let index = output_index(item);
        if self.open_text_blocks.remove(&index) {
            Ok(vec![content_block_stop(index)?])
        } else {
            Ok(Vec::new())
        }
    }

    fn output_item_added_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let Some(tool_call) = tool_call_from_event_item(item) else {
            return Ok(Vec::new());
        };
        let index = output_index(item);
        cache_tool_meta(&mut self.tool_meta, item, &tool_call);
        if self.open_tool_blocks.insert(index) {
            self.seen_tool_blocks.insert(index);
            Ok(vec![tool_block_start(index, &tool_call)?])
        } else {
            Ok(Vec::new())
        }
    }

    fn tool_arguments_delta_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let index = output_index(item);
        let mut events = self.open_tool_from_delta(item, index)?;
        if !self.open_tool_blocks.contains(&index) {
            return Ok(events);
        }
        let delta = item
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !delta.is_empty() {
            self.emitted_tool_arguments
                .entry(index)
                .or_default()
                .push_str(delta);
            self.add_output_tokens(delta);
            events.push(tool_json_delta(index, delta)?);
        }
        Ok(events)
    }

    fn open_tool_from_delta(&mut self, item: &Value, index: u32) -> Result<Vec<Event>> {
        let item_id = item.get("item_id").and_then(Value::as_str);
        let Some(meta) = item_id.and_then(|id| self.tool_meta.get(id)) else {
            return Ok(Vec::new());
        };
        if !self.open_tool_blocks.insert(index) {
            return Ok(Vec::new());
        }

        self.seen_tool_blocks.insert(index);
        let tool_call = ToolCall {
            id: meta.id.clone(),
            kind: "function".to_owned(),
            function: FunctionCall {
                name: meta.name.clone(),
                arguments: String::new(),
            },
        };
        Ok(vec![tool_block_start(index, &tool_call)?])
    }

    fn tool_arguments_done_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let index = output_index(item);
        let mut events = self.open_tool_from_delta(item, index)?;
        if self.open_tool_blocks.contains(&index)
            && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
        {
            events.extend(self.missing_tool_arguments_events(index, arguments)?);
        }
        // Keep the block open until output_item.done so its full arguments can
        // still repair a missing or partial arguments.done payload.
        Ok(events)
    }

    fn output_item_done_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        if let Some(tool_call) = tool_call_from_event_item(item) {
            self.completed_tool_events(item, &tool_call)
        } else if let Some(thinking) = reasoning_text_from_event_item(item) {
            self.completed_reasoning_events(item, &thinking)
        } else {
            Ok(Vec::new())
        }
    }

    fn completed_tool_events(&mut self, item: &Value, tool_call: &ToolCall) -> Result<Vec<Event>> {
        let index = output_index(item);
        let first_seen = self.seen_tool_blocks.insert(index);
        let mut events = Vec::new();
        if first_seen && self.open_tool_blocks.insert(index) {
            events.push(tool_block_start(index, tool_call)?);
        }
        if self.open_tool_blocks.contains(&index)
            && let Some(arguments) = item.pointer("/item/arguments").and_then(Value::as_str)
        {
            events.extend(self.missing_tool_arguments_events(index, arguments)?);
        }
        if self.open_tool_blocks.remove(&index) {
            events.push(content_block_stop(index)?);
        }
        Ok(events)
    }

    /// Emits only the suffix absent from prior deltas, keeping one JSON input per tool.
    ///
    /// # Errors
    ///
    /// Returns an error if upstream's final arguments contradict bytes already streamed.
    fn missing_tool_arguments_events(&mut self, index: u32, arguments: &str) -> Result<Vec<Event>> {
        let emitted = self.emitted_tool_arguments.entry(index).or_default();
        let missing = arguments.strip_prefix(emitted.as_str()).ok_or_else(|| {
            crate::Error::upstream(format!(
                "tool arguments for output index {index} changed after streaming"
            ))
        })?;
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        emitted.push_str(missing);
        self.add_output_tokens(missing);
        Ok(vec![tool_json_delta(index, missing)?])
    }

    fn completed_reasoning_events(&mut self, item: &Value, thinking: &str) -> Result<Vec<Event>> {
        let index = output_index(item);
        let mut events = Vec::new();
        if self.open_thinking_blocks.insert(index) {
            events.push(thinking_block_start(index)?);
        }
        if !thinking.is_empty() && !self.seen_thinking_text_blocks.contains(&index) {
            self.seen_thinking_text_blocks.insert(index);
            self.add_output_tokens(thinking);
            events.push(thinking_delta(index, thinking)?);
        }
        if let Some(signature) = reasoning_signature_from_event_item(item) {
            events.push(signature_delta(index, &signature)?);
        }
        if self.open_thinking_blocks.remove(&index) {
            events.push(content_block_stop(index)?);
        }
        Ok(events)
    }

    fn reasoning_delta_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let index = output_index(item);
        let delta = item
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        if self.open_thinking_blocks.insert(index) {
            events.push(thinking_block_start(index)?);
        }
        self.seen_thinking_text_blocks.insert(index);
        self.add_output_tokens(delta);
        events.push(thinking_delta(index, delta)?);
        Ok(events)
    }

    fn done_events(&mut self, item: &Value) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut stop_reason = finish_reason(item);
        if let Some(response) = item.get("response") {
            self.merge_final_response(response, &mut stop_reason);
            events.extend(Self::generated_image_events(response)?);
        }

        if stop_reason == "stop" && !self.seen_tool_blocks.is_empty() {
            "tool_calls".clone_into(&mut stop_reason);
        }

        events.extend(self.close_open_blocks()?);
        events.push(message_delta_event(&stop_reason, &self.usage)?);
        events.push(message_stop_event()?);
        Ok(events)
    }

    fn merge_final_response(&mut self, response: &Value, stop_reason: &mut String) {
        merge_final_anthropic_usage(&mut self.usage, response.get("usage"));
        if *stop_reason == "stop" && response_has_function_call(response) {
            "tool_calls".clone_into(stop_reason);
        }
    }

    fn generated_image_events(response: &Value) -> Result<Vec<Event>> {
        let output = response
            .get("output")
            .and_then(Value::as_array)
            .map_or(&[] as &[Value], Vec::as_slice);
        let mut events = Vec::new();
        for (index, image) in generated_images_from_output(output).into_iter().enumerate() {
            let source = ImageSource {
                kind: "base64".to_owned(),
                media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                data: Some(image.b64_json),
                text: None,
                url: None,
                file_id: None,
            };
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            events.push(image_block_start(index, &source)?);
            events.push(content_block_stop(index)?);
        }
        Ok(events)
    }

    fn close_open_blocks(&mut self) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        for index in std::mem::take(&mut self.open_text_blocks) {
            events.push(content_block_stop(index)?);
        }
        for index in std::mem::take(&mut self.open_tool_blocks) {
            events.push(content_block_stop(index)?);
        }
        for index in std::mem::take(&mut self.open_thinking_blocks) {
            events.push(content_block_stop(index)?);
        }
        Ok(events)
    }

    fn add_output_tokens(&mut self, text: &str) {
        self.usage.output_tokens = self
            .usage
            .output_tokens
            .saturating_add(estimate_stream_tokens(text));
    }
}

pub(in crate::server::handlers) fn anthropic_error_response(
    error: &crate::Error,
) -> axum::response::Response {
    (error.status_code(), axum::Json(error_body(error))).into_response()
}

fn anthropic_error_event(error: &crate::Error) -> Event {
    Event::default()
        .event("error")
        .data(error_body(error).to_string())
}

fn estimate_stream_tokens(text: &str) -> u32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        u32::try_from(trimmed.chars().count())
            .unwrap_or(u32::MAX)
            .saturating_div(4)
            .max(1)
    }
}

fn tool_call_from_event_item(event: &Value) -> Option<crate::openai::types::ToolCall> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }

    Some(crate::openai::types::ToolCall {
        id: item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: "function".to_owned(),
        function: crate::openai::types::FunctionCall {
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
    })
}

fn cache_tool_meta(
    tool_meta: &mut std::collections::HashMap<String, ToolMeta>,
    event: &Value,
    tool_call: &crate::openai::types::ToolCall,
) {
    tool_meta.insert(
        tool_call.id.clone(),
        ToolMeta {
            id: tool_call.id.clone(),
            name: tool_call.function.name.clone(),
        },
    );
    if let Some(item_id) = event
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
    {
        tool_meta.insert(
            item_id.to_owned(),
            ToolMeta {
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
            },
        );
    }
}

fn response_has_function_call(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|entry| entry.get("type").and_then(Value::as_str) == Some("function_call"))
        })
}

fn merge_final_anthropic_usage(usage: &mut crate::anthropic::ResponseUsage, value: Option<&Value>) {
    if let Some(value) = value {
        if let Some(input_tokens) = value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            usage.input_tokens = input_tokens;
        }
        if let Some(output_tokens) = value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            usage.output_tokens = output_tokens;
        }
        usage.cache_creation_input_tokens = value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        usage.cache_read_input_tokens = value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        usage.server_tool_use = value
            .get("server_tool_use")
            .filter(|value| !value.is_null())
            .cloned();
    }
}

fn reasoning_text_from_event_item(event: &Value) -> Option<String> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }

    let summary_text = item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if !summary_text.is_empty() {
        return Some(summary_text);
    }

    let content_text = item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!content_text.is_empty()).then_some(content_text)
}

fn reasoning_signature_from_event_item(event: &Value) -> Option<String> {
    event
        .get("item")
        .and_then(|item| item.get("encrypted_content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[derive(Clone)]
struct ToolMeta {
    id: String,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, response::IntoResponse};
    use futures_util::stream;
    use serde_json::json;

    #[tokio::test]
    async fn raw_messages_sse_maps_tool_call_lifecycle_without_server() {
        let upstream_events: Vec<Result<crate::codex::sse::JsonSseEvent>> = vec![
            Ok(json_sse_event(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_weather",
                    "call_id": "call_weather",
                    "name": "get_weather",
                    "arguments": ""
                }
            }))),
            Ok(json_sse_event(json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_weather",
                "delta": "{\"city\":\"Paris\"}"
            }))),
            Ok(json_sse_event(json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_weather"
            }))),
            Ok(json_sse_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_weather",
                    "call_id": "call_weather",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Paris\"}"
                }
            }))),
            Ok(json_sse_event(json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_weather",
                    "model": "gpt-5.5",
                    "status": "completed",
                    "output": [{
                        "type": "function_call",
                        "id": "fc_weather",
                        "call_id": "call_weather",
                        "name": "get_weather",
                        "arguments": "{\"city\":\"Paris\"}"
                    }],
                    "usage": {
                        "input_tokens": 7,
                        "output_tokens": 4,
                        "total_tokens": 11
                    }
                }
            }))),
        ];

        let response = anthropic_raw_messages_sse_response(
            Box::pin(stream::iter(upstream_events)),
            "gpt-5.5".to_owned(),
            3,
        )
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert_sse_events_in_order(
            &text,
            &[
                "event: message_start",
                "event: content_block_start",
                "event: content_block_delta",
                "event: content_block_stop",
                "event: message_delta",
                "event: message_stop",
            ],
        );
        assert!(text.contains("\"type\":\"tool_use\""));
        assert!(text.contains("\"id\":\"call_weather\""));
        assert!(text.contains("\"name\":\"get_weather\""));
        assert!(text.contains("\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\""));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));
        assert!(text.contains("\"input_tokens\":7"));
        assert!(text.contains("\"output_tokens\":4"));
        assert!(!text.contains("event: error"));
    }

    #[tokio::test]
    async fn interleaved_tools_recover_missing_and_partial_arguments_before_block_stop() {
        let upstream_events = [
            json!({"type": "response.output_item.added", "output_index": 0,
                "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a",
                    "name": "read_file", "arguments": ""}}),
            json!({"type": "response.output_item.added", "output_index": 1,
                "item": {"type": "function_call", "id": "fc_b", "call_id": "call_b",
                    "name": "list_directory", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_a", "delta": "{\"path\":\""}),
            json!({"type": "response.function_call_arguments.done", "output_index": 1,
                "item_id": "fc_b"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc_a", "arguments": "{\"path\":\"a.txt\"}"}),
            json!({"type": "response.output_item.done", "output_index": 1,
                "item": {"type": "function_call", "id": "fc_b", "call_id": "call_b",
                    "name": "list_directory", "arguments": "{\"path\":\".\"}"}}),
            json!({"type": "response.output_item.done", "output_index": 0,
                "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a",
                    "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}),
            json!({"type": "response.completed", "response": {"status": "completed",
                "finish_reason": "stop", "output": []}}),
        ]
        .into_iter()
        .map(|value| Ok(json_sse_event(value)))
        .collect::<Vec<_>>();
        let response = anthropic_raw_messages_sse_response(
            Box::pin(stream::iter(upstream_events)),
            "gpt-5.5".to_owned(),
            3,
        )
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str::<Value>(data).unwrap())
            .collect::<Vec<_>>();
        let mut arguments = HashMap::<u32, String>::new();
        let mut closed = BTreeSet::new();
        for event in &events {
            if event["type"] == "content_block_delta"
                && event["delta"]["type"] == "input_json_delta"
            {
                let index = u32::try_from(event["index"].as_u64().unwrap()).unwrap();
                arguments
                    .entry(index)
                    .or_default()
                    .push_str(event["delta"]["partial_json"].as_str().unwrap());
                assert!(
                    !closed.contains(&index),
                    "arguments arrived after block stop"
                );
            }
            if event["type"] == "content_block_stop" {
                closed.insert(u32::try_from(event["index"].as_u64().unwrap()).unwrap());
            }
        }

        assert_eq!(
            arguments.get(&0).map(String::as_str),
            Some("{\"path\":\"a.txt\"}")
        );
        assert_eq!(
            arguments.get(&1).map(String::as_str),
            Some("{\"path\":\".\"}")
        );
        assert_eq!(closed, BTreeSet::from([0, 1]));
        assert!(
            events.iter().any(|event| event["type"] == "message_delta"
                && event["delta"]["stop_reason"] == "tool_use")
        );
    }

    #[tokio::test]
    async fn incomplete_tool_stream_keeps_max_tokens_stop_reason() {
        let upstream_events = [
            json!({"type": "response.output_item.added", "output_index": 0,
                "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a",
                    "name": "read_file", "arguments": ""}}),
            json!({"type": "response.incomplete", "response": {"status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}, "output": []}}),
        ]
        .into_iter()
        .map(|value| Ok(json_sse_event(value)))
        .collect::<Vec<_>>();
        let response = anthropic_raw_messages_sse_response(
            Box::pin(stream::iter(upstream_events)),
            "gpt-5.5".to_owned(),
            3,
        )
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("\"stop_reason\":\"max_tokens\""));
        assert!(!text.contains("\"stop_reason\":\"tool_use\""));
    }

    fn json_sse_event(value: Value) -> crate::codex::sse::JsonSseEvent {
        let event = value.get("type").and_then(Value::as_str).map(str::to_owned);
        crate::codex::sse::JsonSseEvent { event, value }
    }

    fn assert_sse_events_in_order(stream: &str, events: &[&str]) {
        let mut offset = 0;
        for event in events {
            let remaining = &stream[offset..];
            let position = remaining
                .find(event)
                .unwrap_or_else(|| panic!("missing SSE event {event}"));
            offset += position + event.len();
        }
    }
}
