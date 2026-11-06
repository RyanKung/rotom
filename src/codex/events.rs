use crate::{
    Error, Result,
    codex::sse,
    openai::{
        response::Usage,
        types::{FunctionCall, ToolCall},
    },
};
use futures_util::StreamExt;
use reqwest::Response;
use serde_json::{Value, json};

/// Aggregated output built from a streamed Codex response.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChatOutput {
    /// Concatenated assistant text collected from deltas or final message items.
    pub text: String,
    /// Function calls emitted by the model during the response.
    pub tool_calls: Vec<ToolCall>,
    /// Generated images emitted by hosted image generation tools.
    pub images: Vec<crate::openai::response::GeneratedImage>,
    /// Token usage reported by the upstream response, when available.
    pub usage: Option<Usage>,
    /// OpenAI-compatible finish reason derived from the terminal response event.
    pub finish_reason: String,
}

impl ChatOutput {
    fn has_usable_result(&self) -> bool {
        !self.text.is_empty() || !self.tool_calls.is_empty() || !self.images.is_empty()
    }
}

/// Consumes a streamed Codex HTTP response and folds its SSE events into one output.
///
/// # Errors
///
/// Returns an error when the SSE stream contains an upstream failure event or
/// cannot be decoded into JSON events.
pub async fn collect_output(response: Response) -> Result<ChatOutput> {
    let mut events = Box::pin(sse::json_events(Box::pin(response.bytes_stream())));
    let mut output = ChatOutput {
        finish_reason: "stop".to_owned(),
        ..ChatOutput::default()
    };

    while let Some(event) = events.next().await {
        let event = event?;
        apply_event(&mut output, &event)?;
        if is_done_event(&event) {
            break;
        }
    }

    if output.tool_calls.is_empty() {
        let has_usable_result = output.has_usable_result();
        output.finish_reason =
            normalize_incomplete_result_finish_reason(output.finish_reason, has_usable_result);
    } else {
        "tool_calls".clone_into(&mut output.finish_reason);
    }

    Ok(output)
}

/// Collects the final upstream `response` envelope from a streamed Codex request.
///
/// # Errors
///
/// Returns an error when the upstream stream reports a failure or ends before
/// emitting a final response envelope.
pub async fn collect_response_value(response: Response) -> Result<Value> {
    let mut events = Box::pin(sse::json_events(Box::pin(response.bytes_stream())));
    let mut output = ChatOutput {
        finish_reason: "stop".to_owned(),
        ..ChatOutput::default()
    };
    while let Some(event) = events.next().await {
        let event = event?;
        apply_event(&mut output, &event)?;
        if is_done_event(&event) {
            let mut response = event.get("response").cloned().ok_or_else(|| {
                Error::upstream("Codex response completed without a response payload")
            })?;
            backfill_response_output(&mut response, &output);
            normalize_incomplete_result_response(&mut response);
            return Ok(response);
        }
    }

    Err(Error::upstream(
        "Codex response stream ended before completion",
    ))
}

fn backfill_response_output(response: &mut Value, output: &ChatOutput) {
    let response_id = response_id(response).to_owned();
    let Some(object) = response.as_object_mut() else {
        return;
    };
    if !response_output_needs_backfill(object.get("output")) {
        return;
    }

    object.insert(
        "output".to_owned(),
        Value::Array(recovered_response_output_items(&response_id, output)),
    );
}

/// Treats upstream `incomplete_result` terminals as completed only when a
/// downstream-compatible output item is already available.
pub(crate) fn normalize_incomplete_result_response(response: &mut Value) -> bool {
    if response.get("status").and_then(Value::as_str) != Some("incomplete")
        || incomplete_details_code(response) != Some("incomplete_result")
        || !response_output_has_usable_result(response.get("output"))
    {
        return false;
    }

    let Some(object) = response.as_object_mut() else {
        return false;
    };
    object.insert("status".to_owned(), Value::String("completed".to_owned()));
    object.insert("incomplete_details".to_owned(), Value::Null);
    true
}

pub(crate) fn normalize_incomplete_result_finish_reason(
    reason: String,
    has_usable_result: bool,
) -> String {
    if reason == "incomplete_result" && has_usable_result {
        "stop".to_owned()
    } else {
        reason
    }
}

pub(crate) fn response_has_usable_output(response: &Value) -> bool {
    response_output_has_usable_result(response.get("output"))
}

fn incomplete_details_code(response: &Value) -> Option<&str> {
    response
        .pointer("/incomplete_details/reason")
        .or_else(|| response.pointer("/incomplete_details/code"))
        .and_then(Value::as_str)
}

fn response_output_has_usable_result(output: Option<&Value>) -> bool {
    output
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(output_item_has_usable_result))
}

fn output_item_has_usable_result(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => item
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(output_part_has_text)),
        Some("function_call") => {
            item.get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
                && item
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && item.get("arguments").and_then(Value::as_str).is_some()
        }
        Some("image_generation_call") => item
            .get("result")
            .or_else(|| item.get("b64_json"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        _ => false,
    }
}

fn output_part_has_text(part: &Value) -> bool {
    match part.get("type").and_then(Value::as_str) {
        Some("output_text") => part
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        Some("refusal") => part
            .get("refusal")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        _ => false,
    }
}

fn response_id(response: &Value) -> &str {
    response.get("id").and_then(Value::as_str).unwrap_or("resp")
}

fn response_output_needs_backfill(output: Option<&Value>) -> bool {
    match output {
        Some(Value::Array(items)) => items.is_empty(),
        Some(Value::Null) | None => true,
        Some(_) => false,
    }
}

fn recovered_response_output_items(response_id: &str, output: &ChatOutput) -> Vec<Value> {
    let mut items = Vec::new();
    if !output.text.is_empty() {
        items.push(json!({
            "id": format!("msg_{response_id}"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": output.text,
                "annotations": []
            }]
        }));
    }

    for tool_call in &output.tool_calls {
        items.push(json!({
            "type": "function_call",
            "call_id": tool_call.id,
            "name": tool_call.function.name,
            "arguments": tool_call.function.arguments
        }));
    }

    items
}

/// Applies one parsed Codex event to an in-progress chat output.
///
/// # Errors
///
/// Returns an error when the event encodes an upstream failure.
pub fn apply_event(output: &mut ChatOutput, event: &Value) -> Result<()> {
    if let Some(message) = event_error(event) {
        return Err(Error::upstream(message));
    }

    if let Some(delta) = text_delta(event) {
        output.text.push_str(&delta);
    }

    // Streaming item completion events are the earliest stable point where
    // function calls can be collected without waiting for the final envelope.
    if matches!(event_type(event), Some("response.output_item.done")) {
        if let Some(item) = event.get("item") {
            apply_output_item(output, item, false);
        }
    }

    if let Some(response) = event.get("response") {
        if let Some(usage) = parse_usage(response.get("usage")) {
            output.usage = Some(usage);
        }
        // Some responses only expose final text in the completed output array,
        // so backfill it if no incremental text deltas were observed.
        for item in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            apply_output_item(output, item, output.text.is_empty());
        }
    }

    if is_done_event(event) {
        finish_reason(event).clone_into(&mut output.finish_reason);
    }

    Ok(())
}

/// Extracts a text delta from a streaming event, if the event carries one.
#[must_use]
pub fn text_delta(event: &Value) -> Option<String> {
    matches!(event_type(event), Some("response.output_text.delta"))
        .then(|| {
            event
                .get("delta")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .flatten()
}

/// Returns whether an event marks the end of a streamed Codex response.
#[must_use]
pub fn is_done_event(event: &Value) -> bool {
    matches!(
        event_type(event),
        Some("response.completed" | "response.done" | "response.incomplete")
    )
}

/// Maps a terminal Codex event to the finish reason exposed to chat clients.
#[must_use]
pub fn finish_reason(event: &Value) -> String {
    match event_type(event) {
        Some("response.incomplete") => incomplete_finish_reason(event),
        _ => event
            .get("response")
            .and_then(|response| {
                response
                    .get("stop_reason")
                    .or_else(|| response.get("finish_reason"))
                    .and_then(Value::as_str)
            })
            .map_or_else(|| "stop".to_owned(), map_upstream_finish_reason),
    }
}

/// Extracts an upstream error message from an event, if the event represents failure.
#[must_use]
pub fn event_error(event: &Value) -> Option<String> {
    match event_type(event) {
        Some("error") => event
            .get("message")
            .or_else(|| event.pointer("/error/message"))
            .or_else(|| event.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(event.to_string())),
        Some("response.failed") => event
            .pointer("/response/error/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some("Codex response failed".to_owned())),
        _ => None,
    }
}

/// Extracts a completed function call from a streaming output-item event.
pub fn event_tool_call(event: &Value) -> Option<ToolCall> {
    matches!(event_type(event), Some("response.output_item.done"))
        .then(|| event.get("item"))
        .flatten()
        .and_then(parse_tool_call)
}

/// Extracts function calls from the final `response.output` payload on a terminal event.
pub fn response_tool_calls(event: &Value) -> Vec<ToolCall> {
    event
        .get("response")
        .and_then(|response| response.get("output"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_tool_call)
        .collect()
}

fn apply_output_item(output: &mut ChatOutput, item: &Value, fill_text: bool) {
    match item.get("type").and_then(Value::as_str) {
        Some("message") if fill_text => {
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(output_text)
                .collect::<Vec<_>>()
                .join("");
            output.text.push_str(&text);
        }
        Some("function_call") => {
            if let Some(tool_call) = parse_tool_call(item) {
                if !output.tool_calls.iter().any(|call| call.id == tool_call.id) {
                    output.tool_calls.push(tool_call);
                }
            }
        }
        Some("image_generation_call") => {
            if let Some(image) = crate::openai::response::generated_image_from_item(item) {
                if !output
                    .images
                    .iter()
                    .any(|existing| existing.b64_json == image.b64_json)
                {
                    output.images.push(image);
                }
            }
        }
        _ => {}
    }
}

fn output_text(part: &Value) -> Option<&str> {
    match part.get("type").and_then(Value::as_str) {
        Some("output_text") => part.get("text").and_then(Value::as_str),
        Some("refusal") => part.get("refusal").and_then(Value::as_str),
        _ => None,
    }
}

fn incomplete_finish_reason(event: &Value) -> String {
    event
        .pointer("/response/incomplete_details/reason")
        .or_else(|| event.pointer("/incomplete_details/reason"))
        .and_then(Value::as_str)
        .map_or_else(|| "length".to_owned(), map_upstream_finish_reason)
}

fn map_upstream_finish_reason(reason: &str) -> String {
    match reason {
        "tool_calls" | "tool_use" => "tool_calls".to_owned(),
        "max_output_tokens" | "max_tokens" | "length" => "length".to_owned(),
        "content_filter" => "content_filter".to_owned(),
        "stop" | "end_turn" => "stop".to_owned(),
        other => other.to_owned(),
    }
}

fn parse_tool_call(item: &Value) -> Option<ToolCall> {
    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))?
        .as_str()?
        .to_owned();
    let name = item.get("name")?.as_str()?.to_owned();
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}")
        .to_owned();

    Some(ToolCall {
        id,
        kind: "function".to_owned(),
        function: FunctionCall { name, arguments },
    })
}

fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let usage = value?;
    let prompt_tokens = u32::try_from(usage.get("input_tokens")?.as_u64()?).ok()?;
    let completion_tokens = u32::try_from(usage.get("output_tokens")?.as_u64()?).ok()?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .map_or_else(
            || Some(prompt_tokens.saturating_add(completion_tokens)),
            |value| u32::try_from(value).ok(),
        )?;
    let cached_tokens = usage
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());

    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        prompt_tokens_details: cached_tokens
            .map(|cached_tokens| crate::openai::response::PromptTokensDetails { cached_tokens }),
    })
}

fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_text_delta_and_usage() {
        let mut output = ChatOutput::default();
        apply_event(
            &mut output,
            &json!({"type": "response.output_text.delta", "delta": "hi"}),
        )
        .unwrap();
        apply_event(
            &mut output,
            &json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 2, "output_tokens": 3, "total_tokens": 5}}
            }),
        )
        .unwrap();

        assert_eq!(output.text, "hi");
        assert_eq!(output.usage.unwrap().total_tokens, 5);
    }

    #[test]
    fn extracts_final_text_when_no_delta_was_seen() {
        let mut output = ChatOutput::default();
        apply_event(
            &mut output,
            &json!({
                "type": "response.completed",
                "response": {
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "final"}]
                    }]
                }
            }),
        )
        .unwrap();

        assert_eq!(output.text, "final");
    }

    #[test]
    fn extracts_function_call_items() {
        let mut output = ChatOutput::default();
        apply_event(
            &mut output,
            &json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{\"q\":\"x\"}"
                }
            }),
        )
        .unwrap();

        assert_eq!(output.tool_calls[0].function.name, "lookup");
    }

    #[test]
    fn extracts_tool_call_from_streaming_item_done_event() {
        let tool_call = event_tool_call(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"q\":\"x\"}"
            }
        }))
        .unwrap();

        assert_eq!(tool_call.id, "call_1");
        assert_eq!(tool_call.function.arguments, "{\"q\":\"x\"}");
    }

    #[test]
    fn extracts_tool_calls_from_final_response_output() {
        let tool_calls = response_tool_calls(&json!({
            "type": "response.completed",
            "response": {
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{}"
                }]
            }
        }));

        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "lookup");
    }

    #[test]
    fn backfills_empty_response_output_from_collected_text() {
        let mut response = json!({
            "id": "resp_1",
            "output": []
        });
        let output = ChatOutput {
            text: "ok".to_owned(),
            ..ChatOutput::default()
        };

        backfill_response_output(&mut response, &output);

        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "ok");
    }

    #[test]
    fn normalizes_incomplete_result_when_output_is_usable() {
        let mut response = json!({
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": {"reason": "incomplete_result"},
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "ok"}]
            }]
        });

        assert!(normalize_incomplete_result_response(&mut response));

        assert_eq!(response["status"], "completed");
        assert!(response["incomplete_details"].is_null());
    }

    #[test]
    fn normalizes_incomplete_result_finish_reason_with_usable_output() {
        let output = ChatOutput {
            text: "ok".to_owned(),
            finish_reason: "incomplete_result".to_owned(),
            ..ChatOutput::default()
        };
        let has_usable_result = output.has_usable_result();

        assert_eq!(
            normalize_incomplete_result_finish_reason(output.finish_reason, has_usable_result,),
            "stop"
        );
    }

    #[test]
    fn preserves_incomplete_result_finish_reason_without_usable_output() {
        let output = ChatOutput {
            finish_reason: "incomplete_result".to_owned(),
            ..ChatOutput::default()
        };
        let has_usable_result = output.has_usable_result();

        assert_eq!(
            normalize_incomplete_result_finish_reason(output.finish_reason, has_usable_result,),
            "incomplete_result"
        );
    }

    #[test]
    fn normalizes_incomplete_result_for_complete_function_call() {
        let mut response = json!({
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": {"reason": "incomplete_result"},
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"q\":\"x\"}"
            }]
        });

        assert!(normalize_incomplete_result_response(&mut response));

        assert_eq!(response["status"], "completed");
    }

    #[test]
    fn preserves_incomplete_result_without_usable_output() {
        let mut response = json!({
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": {"reason": "incomplete_result"},
            "output": []
        });

        assert!(!normalize_incomplete_result_response(&mut response));

        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "incomplete_result"
        );
    }

    #[test]
    fn preserves_incomplete_result_for_reasoning_only_output() {
        let mut response = json!({
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": {"reason": "incomplete_result"},
            "output": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking"}],
                "encrypted_content": "sig"
            }]
        });

        assert!(!normalize_incomplete_result_response(&mut response));

        assert_eq!(response["status"], "incomplete");
    }

    #[test]
    fn preserves_incomplete_for_true_truncation_even_with_output() {
        let mut response = json!({
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "partial"}]
            }]
        });

        assert!(!normalize_incomplete_result_response(&mut response));

        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn extracts_cached_tokens_into_prompt_tokens_details() {
        let mut output = ChatOutput::default();
        apply_event(
            &mut output,
            &json!({
                "type": "response.completed",
                "response": {
                    "usage": {
                        "input_tokens": 71900,
                        "output_tokens": 300,
                        "total_tokens": 72200,
                        "input_tokens_details": {
                            "cached_tokens": 70000
                        }
                    }
                }
            }),
        )
        .unwrap();

        let usage = output.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 71900);
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 70000);
    }
}
