//! Converts Responses events into Chat deltas while preserving tool identity.
//!
//! Each tool has one introduction, an append-only argument prefix, and a final
//! snapshot. Output indexes include non-tool items upstream; Chat indexes count
//! only introduced calls. Final snapshots contribute only a missing suffix.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use serde_json::Value;

#[cfg(test)]
mod tests;

use crate::{
    Error, Result,
    codex::events::{
        event_error, finish_reason, is_done_event, normalize_incomplete_result_finish_reason,
        response_has_usable_output, text_delta,
    },
    openai::{
        response::{
            ChatCompletionChunk, ToolCallDelta, chunk_finished, chunk_with_content,
            chunk_with_role, chunk_with_tool_call_delta,
        },
        types::FunctionCall,
    },
};

/// Invalid transitions that cannot be represented by append-only Chat deltas.
#[derive(Debug, thiserror::Error)]
enum ToolStreamError {
    /// Chat indexes cannot represent another distinct tool call.
    #[error("too many tool calls in one response")]
    TooManyCalls,
    /// A final snapshot disagrees with arguments already delivered to the client.
    #[error("completed tool arguments do not match the streamed prefix")]
    ConflictingArguments,
    /// Upstream stopped without a final snapshot for an introduced call.
    #[error("response stream ended with unfinished tool calls")]
    UnfinishedCalls,
}

/// One introduced tool and the argument prefix already sent downstream.
struct ToolState {
    /// Dense Chat index assigned once when this call is introduced.
    index: u32,
    /// Upstream output index, which may include preceding non-tool items.
    output_index: Option<u64>,
    /// Responses item ID used by argument events when no output index is present.
    item_id: Option<String>,
    /// Public call ID used by tool results and terminal snapshot deduplication.
    call_id: String,
    /// Exact UTF-8 argument prefix delivered to the client.
    arguments: String,
    /// Whether an authoritative arguments or item completion was observed.
    complete: bool,
}

impl ToolState {
    /// Matches delta events by their upstream output index or item identity.
    fn matches_event(&self, event: &Value) -> bool {
        event
            .get("output_index")
            .and_then(Value::as_u64)
            .is_some_and(|index| Some(index) == self.output_index)
            || event
                .get("item_id")
                .and_then(Value::as_str)
                .is_some_and(|id| Some(id) == self.item_id.as_deref())
    }

    /// Appends one fragment and creates a continuation without repeated metadata.
    fn append(&mut self, arguments: &str) -> ToolCallDelta {
        self.arguments.push_str(arguments);
        ToolCallDelta {
            index: self.index,
            id: String::new(),
            kind: "",
            function: FunctionCall {
                name: String::new(),
                arguments: arguments.to_owned(),
            },
        }
    }

    /// Reconciles the final snapshot without replaying bytes already delivered.
    fn finish(
        &mut self,
        arguments: &str,
    ) -> std::result::Result<Vec<ToolCallDelta>, ToolStreamError> {
        // Invariant: sent arguments are a prefix of every accepted final snapshot.
        let suffix = arguments
            .strip_prefix(&self.arguments)
            .ok_or(ToolStreamError::ConflictingArguments)?;
        let deltas = if suffix.is_empty() {
            Vec::new()
        } else {
            vec![self.append(suffix)]
        };
        self.complete = true;
        Ok(deltas)
    }
}

/// Per-response tool registry, in the order calls were introduced downstream.
#[derive(Default)]
struct ToolStream {
    /// Calls retained through the terminal event to suppress repeated snapshots.
    calls: Vec<ToolState>,
}

impl ToolStream {
    /// Applies one upstream event and returns only newly available tool deltas.
    fn apply(&mut self, event: &Value) -> std::result::Result<Vec<ToolCallDelta>, ToolStreamError> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.added" | "response.output_item.done") => {
                let Some(item) = event.get("item") else {
                    return Ok(Vec::new());
                };
                self.item(
                    item,
                    event.get("output_index").and_then(Value::as_u64),
                    event.get("type").and_then(Value::as_str) == Some("response.output_item.done"),
                )
            }
            Some("response.function_call_arguments.delta") => {
                let Some(call) = self.calls.iter_mut().find(|call| call.matches_event(event))
                else {
                    // Without metadata, wait for a complete item rather than inventing an ID.
                    return Ok(Vec::new());
                };
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if call.complete || delta.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![call.append(delta)])
            }
            Some("response.function_call_arguments.done") => {
                let Some(call) = self.calls.iter_mut().find(|call| call.matches_event(event))
                else {
                    return Ok(Vec::new());
                };
                event
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map_or_else(|| Ok(Vec::new()), |arguments| call.finish(arguments))
            }
            _ if is_done_event(event) => {
                let mut deltas = Vec::new();
                for item in event
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    deltas.extend(self.item(item, None, true)?);
                }
                Ok(deltas)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Introduces a call once, or reconciles a completed copy of an existing call.
    fn item(
        &mut self,
        item: &Value,
        output_index: Option<u64>,
        complete: bool,
    ) -> std::result::Result<Vec<ToolCallDelta>, ToolStreamError> {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Ok(Vec::new());
        }
        let Some(call_id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        else {
            return Ok(Vec::new());
        };
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        if let Some(call) = self.calls.iter_mut().find(|call| call.call_id == call_id) {
            call.output_index = call.output_index.or(output_index);
            if complete {
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                    return call.finish(arguments);
                }
                // Some completion snapshots omit arguments already sent as deltas.
                call.complete = true;
            }
            return Ok(Vec::new());
        }
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or(if complete { "{}" } else { "" });
        let index = u32::try_from(self.calls.len()).map_err(|_| ToolStreamError::TooManyCalls)?;
        self.calls.push(ToolState {
            index,
            output_index,
            item_id: item.get("id").and_then(Value::as_str).map(str::to_owned),
            call_id: call_id.to_owned(),
            arguments: arguments.to_owned(),
            complete,
        });
        Ok(vec![ToolCallDelta {
            index,
            id: call_id.to_owned(),
            kind: "function",
            function: FunctionCall {
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
        }])
    }

    /// Rejects a truncated call rather than presenting it as executable output.
    fn ensure_complete(&self) -> std::result::Result<(), ToolStreamError> {
        if self.calls.iter().any(|call| !call.complete) {
            Err(ToolStreamError::UnfinishedCalls)
        } else {
            Ok(())
        }
    }
}

/// Shares Chat conversion between native Responses SSE and Kiro-normalized events.
pub(super) fn chat_chunks(
    events: impl Stream<Item = Result<Value>> + Send + 'static,
    id: String,
    created: i64,
    model: String,
) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let mut events = Box::pin(events);
        let mut tools = ToolStream::default();
        let mut saw_usable_output = false;
        yield chunk_with_role(&id, created, &model);
        while let Some(event) = events.next().await {
            let event = event?;
            if let Some(message) = event_error(&event) {
                Err(Error::upstream(message))?;
            }
            if let Some(delta) = text_delta(&event).filter(|delta| !delta.is_empty()) {
                saw_usable_output = true;
                yield chunk_with_content(&id, created, &model, delta);
            }
            for delta in tools.apply(&event).map_err(|error| Error::upstream(error.to_string()))? {
                saw_usable_output = true;
                yield chunk_with_tool_call_delta(&id, created, &model, delta);
            }
            if is_done_event(&event) {
                tools.ensure_complete().map_err(|error| Error::upstream(error.to_string()))?;
                saw_usable_output |= event.get("response").is_some_and(response_has_usable_output);
                let reason = if tools.calls.is_empty() {
                    normalize_incomplete_result_finish_reason(finish_reason(&event), saw_usable_output)
                } else {
                    "tool_calls".to_owned()
                };
                yield chunk_finished(&id, created, &model, &reason);
                return;
            }
        }
        tools.ensure_complete().map_err(|error| Error::upstream(error.to_string()))?;
        let reason = if tools.calls.is_empty() { "stop" } else { "tool_calls" };
        yield chunk_finished(&id, created, &model, reason);
    })
}
