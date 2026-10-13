use super::responses::{
    response_object, response_object_from_upstream, stored_response_input_items,
};
use crate::{
    codex::events::{is_done_event, normalize_incomplete_result_response},
    config::Provider,
    error::Result,
    openai::response::ResponseObject,
};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt, stream};
use serde_json::{Value, json};
use std::{convert::Infallible, pin::Pin};

use crate::openai::types::ToolCall;

mod anthropic;
mod chat;
pub(super) use anthropic::{anthropic_error_response, anthropic_raw_messages_sse_response};
use chat::OpenAiResponsesStreamState;

fn response_created_event(response: &ResponseObject) -> Event {
    Event::default().event("response.created").data(
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response,
        })
        .to_string(),
    )
}

fn response_in_progress_event(response: &ResponseObject) -> Event {
    Event::default().event("response.in_progress").data(
        json!({
            "type": "response.in_progress",
            "sequence_number": 1,
            "response": response,
        })
        .to_string(),
    )
}

fn response_output_item_added_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
) -> Event {
    Event::default().event("response.output_item.added").data(
        json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        })
        .to_string(),
    )
}

fn output_text_part(text: &str) -> Value {
    json!({
        "type": "output_text",
        "text": text,
        "annotations": []
    })
}

fn response_content_part_added_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
) -> Event {
    Event::default().event("response.content_part.added").data(
        json!({
            "type": "response.content_part.added",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "part": output_text_part("")
        })
        .to_string(),
    )
}

fn response_started_lifecycle_events(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
) -> [Event; 2] {
    [
        response_output_item_added_event(sequence_number, response_id, output_index, item_id),
        response_content_part_added_event(
            sequence_number.saturating_add(1),
            response_id,
            output_index,
            item_id,
        ),
    ]
}

fn response_output_text_delta_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_text.delta").data(
        json!({
            "type": "response.output_text.delta",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "delta": text,
        })
        .to_string(),
    )
}

fn response_output_text_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_text.done").data(
        json!({
            "type": "response.output_text.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "text": text,
        })
        .to_string(),
    )
}

fn response_content_part_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.content_part.done").data(
        json!({
            "type": "response.content_part.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "part": output_text_part(text)
        })
        .to_string(),
    )
}

fn response_output_item_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_item.done").data(
        json!({
            "type": "response.output_item.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [output_text_part(text)]
            }
        })
        .to_string(),
    )
}

fn response_finished_lifecycle_events(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    text: &str,
) -> [Event; 3] {
    [
        response_output_text_done_event(sequence_number, response_id, output_index, item_id, text),
        response_content_part_done_event(
            sequence_number.saturating_add(1),
            response_id,
            output_index,
            item_id,
            text,
        ),
        response_output_item_done_event(
            sequence_number.saturating_add(2),
            response_id,
            output_index,
            item_id,
            text,
        ),
    ]
}

fn response_completed_event(sequence_number: u64, response: &ResponseObject) -> Event {
    Event::default().event("response.completed").data(
        json!({
            "type": "response.completed",
            "sequence_number": sequence_number,
            "response": response,
        })
        .to_string(),
    )
}

fn response_function_call_item_added_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    tool_call: &ToolCall,
) -> Event {
    Event::default().event("response.output_item.added").data(
        json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "function_call",
                "status": "in_progress",
                "call_id": tool_call.id,
                "name": tool_call.function.name,
                "arguments": ""
            }
        })
        .to_string(),
    )
}

fn response_function_call_arguments_delta_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    arguments: &str,
) -> Event {
    Event::default()
        .event("response.function_call_arguments.delta")
        .data(
            json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": sequence_number,
                "response_id": response_id,
                "output_index": output_index,
                "item_id": item_id,
                "delta": arguments
            })
            .to_string(),
        )
}

fn response_function_call_arguments_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    name: &str,
    arguments: &str,
) -> Event {
    Event::default()
        .event("response.function_call_arguments.done")
        .data(
            json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": sequence_number,
                "response_id": response_id,
                "output_index": output_index,
                "item_id": item_id,
                "name": name,
                "arguments": arguments
            })
            .to_string(),
        )
}

fn response_function_call_item_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    tool_call: &ToolCall,
) -> Event {
    Event::default().event("response.output_item.done").data(
        json!({
            "type": "response.output_item.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": tool_call.id,
                "name": tool_call.function.name,
                "arguments": tool_call.function.arguments
            }
        })
        .to_string(),
    )
}

fn response_error_event(error: &crate::Error) -> Event {
    Event::default().event("error").data(
        json!({
            "type": "error",
            "error": {
                "message": error.to_string(),
                "type": "upstream_error",
            }
        })
        .to_string(),
    )
}

pub(super) fn sse_response(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = stream.map(|item| {
        let event = match item {
            Ok(chunk) => Event::default().data(serde_json::to_string(&chunk).unwrap_or_default()),
            Err(error) => Event::default().data(
                json!({"error": {"message": error.to_string(), "type": "upstream_error"}})
                    .to_string(),
            ),
        };
        Ok(event)
    });

    let done = stream::once(async { Ok(Event::default().data("[DONE]")) });
    Sse::new(mapped.chain(done)).keep_alive(KeepAlive::default())
}

pub(super) fn openai_responses_sse(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
    response_id: String,
    request: crate::openai::types::ResponsesRequest,
    input_items: Vec<Value>,
    store: crate::server::store::ResponseStore,
    provider: Provider,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let created_at = crate::config::now_unix();
        let created_response = response_object(
            &request,
            response_id.clone(),
            created_at,
            "in_progress",
            Vec::new(),
            None,
        );
        yield Ok(response_created_event(&created_response));
        yield Ok(response_in_progress_event(&created_response));

        let mut stream = stream;
        let mut stream_state = OpenAiResponsesStreamState::new(response_id.clone());

        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };

                    if let Some(text) = choice.delta.content {
                        for event in stream_state.text_delta_events(&text) {
                            yield Ok(event);
                        }
                    }

                    for tool_call in choice.delta.tool_calls.into_iter().flatten() {
                        for event in stream_state.tool_call_events(tool_call) {
                            yield Ok(event);
                        }
                    }

                    if choice.finish_reason.is_some() {
                        for event in stream_state.finished_events() {
                            yield Ok(event);
                        }
                        let output = stream_state.take_completed_output();
                        let completed = response_object(
                            &request,
                            response_id.clone(),
                            created_at,
                            "completed",
                            output,
                            None,
                        );
                        if request.should_store() {
                            let stored_items =
                                stored_response_input_items(input_items.clone(), &completed);
                            store.insert(crate::server::store::StoredResponse {
                                response: completed.clone(),
                                input_items: stored_items,
                                provider,
                                upstream_resource: false,
                            }).await;
                        }
                        yield Ok(response_completed_event(stream_state.sequence_number(), &completed));
                        return;
                    }
                }
                Err(error) => {
                    yield Ok(response_error_event(&error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

pub(super) fn openai_raw_responses_sse(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    request: crate::openai::types::ResponsesRequest,
    input_items: Vec<Value>,
    store: crate::server::store::ResponseStore,
    provider: Provider,
    upstream_resource: bool,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let mut stream = stream;
        let mut state = RawResponsesStreamState::default();

        while let Some(item) = stream.next().await {
            match item {
                Ok(mut item) => {
                    sanitize_raw_responses_event(&mut item.value, &mut state);
                    if is_done_event(&item.value) {
                        if let Some(response) = item.value.get("response").cloned() {
                            let completed = response_object_from_upstream(&request, &response);
                            if request.should_store() {
                                let stored_items =
                                    stored_response_input_items(input_items.clone(), &completed);
                                store.insert(crate::server::store::StoredResponse {
                                    response: completed,
                                    input_items: stored_items,
                                    provider,
                                    upstream_resource,
                                }).await;
                            }
                        }
                    }

                    let event_name = item
                        .value
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(item.event)
                        .unwrap_or_else(|| "message".to_owned());
                    yield Ok(Event::default()
                        .event(event_name)
                        .data(serde_json::to_string(&item.value).unwrap_or_default()));
                }
                Err(error) => {
                    yield Ok(response_error_event(&error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

#[derive(Default)]
struct RawResponsesStreamState {
    output_items: std::collections::BTreeMap<u32, Value>,
    text_items: std::collections::BTreeMap<u32, RawTextOutput>,
    function_names: std::collections::BTreeMap<u32, String>,
}

#[derive(Default)]
struct RawTextOutput {
    item_id: Option<String>,
    text: String,
}

fn sanitize_raw_responses_event(event: &mut Value, state: &mut RawResponsesStreamState) {
    let is_terminal = is_done_event(event);
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            let index = output_index(event);
            let text = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let item = state.text_items.entry(index).or_default();
            remember_item_id(item, event);
            item.text.push_str(text);
        }
        Some("response.output_text.done") => {
            let index = output_index(event);
            let text = event.get("text").and_then(Value::as_str);
            let item = state.text_items.entry(index).or_default();
            remember_item_id(item, event);
            if let Some(text) = text {
                text.clone_into(&mut item.text);
            }
        }
        Some("response.output_item.added") => remember_function_name(state, event),
        Some("response.output_item.done") => {
            remember_function_name(state, event);
            if let Some(item) = event.get("item").cloned() {
                state.output_items.insert(output_index(event), item);
            }
        }
        Some("response.function_call_arguments.done") => {
            let index = output_index(event);
            if event.get("name").and_then(Value::as_str).is_none()
                && let Some(name) = state.function_names.get(&index)
                && let Some(object) = event.as_object_mut()
            {
                object.insert("name".to_owned(), Value::String(name.clone()));
            }
        }
        _ => {}
    }

    {
        let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) else {
            return;
        };
        if is_terminal && response_output_needs_backfill(response.get("output")) {
            response.insert("output".to_owned(), Value::Array(state.recovered_output()));
        } else if response_output_needs_array(response.get("output")) {
            response.insert("output".to_owned(), Value::Array(Vec::new()));
        }
    }

    if is_terminal
        && event.get("type").and_then(Value::as_str) == Some("response.incomplete")
        && event
            .get_mut("response")
            .is_some_and(normalize_incomplete_result_response)
    {
        event["type"] = Value::String("response.completed".to_owned());
    }
}

fn remember_item_id(item: &mut RawTextOutput, event: &Value) {
    if item.item_id.is_none() {
        item.item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
}

fn remember_function_name(state: &mut RawResponsesStreamState, event: &Value) {
    let Some(item) = event.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    if let Some(name) = item.get("name").and_then(Value::as_str) {
        state
            .function_names
            .insert(output_index(event), name.to_owned());
    }
}

fn response_output_needs_backfill(output: Option<&Value>) -> bool {
    match output {
        Some(Value::Array(items)) => items.is_empty(),
        Some(Value::Null) | None => true,
        Some(_) => false,
    }
}

const fn response_output_needs_array(output: Option<&Value>) -> bool {
    matches!(output, Some(Value::Null) | None)
}

impl RawResponsesStreamState {
    fn recovered_output(&self) -> Vec<Value> {
        let mut output = self.output_items.clone();
        for (index, text_item) in &self.text_items {
            if text_item.text.is_empty() {
                continue;
            }
            output
                .entry(*index)
                .or_insert_with(|| synthesized_message_item(*index, text_item));
        }

        output.into_values().collect()
    }
}

fn synthesized_message_item(index: u32, text_item: &RawTextOutput) -> Value {
    let id = text_item
        .item_id
        .clone()
        .unwrap_or_else(|| format!("msg_{index}"));
    json!({
        "type": "message",
        "id": id,
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text_item.text,
            "annotations": []
        }]
    })
}

#[allow(clippy::too_many_lines)]
fn output_index(event: &Value) -> u32 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_responses_sanitizer_backfills_null_output_from_done_items() {
        let mut state = RawResponsesStreamState::default();
        let mut done_item = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"q\":\"x\"}"
            }
        });
        sanitize_raw_responses_event(&mut done_item, &mut state);

        let mut completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5.5",
                "status": "completed",
                "output": null
            }
        });
        sanitize_raw_responses_event(&mut completed, &mut state);

        assert_eq!(completed["response"]["output"][0]["type"], "function_call");
        assert_eq!(completed["response"]["output"][0]["call_id"], "call_1");
    }

    #[test]
    fn raw_responses_sanitizer_synthesizes_output_from_text_deltas() {
        let mut state = RawResponsesStreamState::default();
        let mut delta = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "item_id": "msg_1",
            "delta": "OK"
        });
        sanitize_raw_responses_event(&mut delta, &mut state);

        let mut completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5.5",
                "status": "completed",
                "output": null
            }
        });
        sanitize_raw_responses_event(&mut completed, &mut state);

        assert_eq!(completed["response"]["output"][0]["type"], "message");
        assert_eq!(completed["response"]["output"][0]["id"], "msg_1");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "OK"
        );
    }
}
