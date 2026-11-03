use super::ReplayCache;
use crate::{
    Error, Result,
    codex::sse::{self, ByteStream, JsonSseEvent},
    config::now_unix,
};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::pin::Pin;

pub(super) fn events(
    bytes: ByteStream,
    body: Value,
    project: String,
    replay: ReplayCache,
) -> Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let validator = super::schema::validator(&body)?;
        let mut buffered = Vec::new();
        let mut state = State::new(&body);
        yield state.event("response.created", json!({"response":state.envelope("in_progress")}));
        let mut upstream = Box::pin(sse::json_events(bytes));
        while let Some(chunk) = upstream.next().await {
            for event in state.apply(&chunk?, &project, &replay).await? {
                if validator.is_some() { buffered.push(event); } else { yield event; }
            }
        }
        let final_events = state.finish()?;
        if let Some(validator) = validator {
            for event in &final_events {
                if let Some(response) = event.value.get("response") {
                    super::schema::validate_response(&validator, response)?;
                }
            }
            for event in buffered { yield event; }
        }
        for event in final_events { yield event; }
    })
}

struct State {
    id: String,
    model: String,
    created: i64,
    sequence: u64,
    output: Vec<Value>,
    text_index: Option<usize>,
    finish_reason: Option<String>,
    usage: Value,
}

impl State {
    fn new(body: &Value) -> Self {
        Self {
            id: format!("resp_{:032x}", rand::random::<u128>()),
            model: body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            created: now_unix(),
            sequence: 0,
            output: Vec::new(),
            text_index: None,
            finish_reason: None,
            usage: Value::Null,
        }
    }

    fn event(&mut self, kind: &str, mut value: Value) -> JsonSseEvent {
        value["type"] = json!(kind);
        value["sequence_number"] = json!(self.sequence);
        self.sequence = self.sequence.saturating_add(1);
        JsonSseEvent {
            event: Some(kind.into()),
            value,
        }
    }

    fn envelope(&self, status: &str) -> Value {
        json!({"id":self.id,"object":"response","created_at":self.created,
            "model":self.model,"status":status,"output":self.output,"usage":self.usage,
            "error":null,"incomplete_details":if status=="incomplete" {json!({"reason":"max_output_tokens"})} else {Value::Null}})
    }

    async fn apply(
        &mut self,
        chunk: &Value,
        project: &str,
        replay: &ReplayCache,
    ) -> Result<Vec<JsonSseEvent>> {
        if chunk.get("error").is_some() {
            return Err(Error::upstream("Gemini stream reported an error"));
        }
        let response = chunk.get("response").unwrap_or(chunk);
        if response.get("error").is_some() {
            return Err(Error::upstream("Gemini generation failed"));
        }
        if let Some(usage) = response.get("usageMetadata") {
            let input = usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let reasoning = usage
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(reasoning);
            self.usage = json!({"input_tokens":input,"output_tokens":output,"total_tokens":input.saturating_add(output),
                "input_tokens_details":{"cached_tokens":usage.get("cachedContentTokenCount").and_then(Value::as_u64).unwrap_or(0)},
                "output_tokens_details":{"reasoning_tokens":reasoning}});
        }
        if response.pointer("/promptFeedback/blockReason").is_some() {
            return Err(Error::upstream("Gemini blocked the prompt"));
        }
        let Some(candidate) = response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for part in candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if part.get("functionCall").is_some() {
                events.extend(self.tool_events(part, project, replay).await?);
            } else if part.get("thought").and_then(Value::as_bool) != Some(true) {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.extend(self.text_events(text)?);
                } else if part.get("inlineData").is_some() {
                    return Err(Error::upstream(
                        "Gemini generated media is not supported by this adapter",
                    ));
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            if !matches!(reason, "STOP" | "MAX_TOKENS") {
                return Err(Error::upstream(format!(
                    "Gemini generation stopped: {reason}"
                )));
            }
            self.finish_reason = Some(reason.into());
        }
        Ok(events)
    }

    fn text_events(&mut self, text: &str) -> Result<Vec<JsonSseEvent>> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        let index = if let Some(index) = self.text_index {
            index
        } else {
            let index = self.output.len();
            let item = json!({"id":format!("msg_{}_{}",self.id,index),"type":"message","role":"assistant","status":"in_progress","content":[]});
            events.push(self.event(
                "response.output_item.added",
                json!({"output_index":index,"item":item}),
            ));
            let mut item = item;
            item["content"] = json!([{"type":"output_text","text":"","annotations":[]}]);
            self.output.push(item);
            self.text_index = Some(index);
            events.push(self.event("response.content_part.added",json!({"output_index":index,"content_index":0,
                "item_id":format!("msg_{}_{}",self.id,index),"part":{"type":"output_text","text":"","annotations":[]}})));
            index
        };
        let item = self
            .output
            .get_mut(index)
            .ok_or_else(|| Error::upstream("missing Gemini text item"))?;
        let current = item
            .pointer_mut("/content/0/text")
            .ok_or_else(|| Error::upstream("missing Gemini text content"))?;
        let combined = format!("{}{text}", current.as_str().unwrap_or_default());
        *current = json!(combined);
        events.push(self.event(
            "response.output_text.delta",
            json!({"output_index":index,"content_index":0,
            "item_id":format!("msg_{}_{}",self.id,index),"delta":text}),
        ));
        Ok(events)
    }

    async fn tool_events(
        &mut self,
        part: &Value,
        project: &str,
        replay: &ReplayCache,
    ) -> Result<Vec<JsonSseEvent>> {
        let name = part
            .pointer("/functionCall/name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::upstream("Gemini function call has no name"))?;
        let args = part
            .pointer("/functionCall/args")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !args.is_object() {
            return Err(Error::upstream(
                "Gemini function call arguments must be an object",
            ));
        }
        let mut saved = part.clone();
        saved["functionCall"]["args"] = args.clone();
        let call_id = replay.insert(project, saved).await;
        let index = self.output.len();
        let id = format!("fc_{call_id}");
        let arguments = serde_json::to_string(&args)?;
        let item = json!({"id":id,"type":"function_call","call_id":call_id,"name":name,"arguments":"","status":"in_progress"});
        let mut events = self.close_text();
        events.push(self.event(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        ));
        events.push(self.event(
            "response.function_call_arguments.delta",
            json!({"output_index":index,"item_id":id,"delta":arguments}),
        ));
        events.push(self.event(
            "response.function_call_arguments.done",
            json!({"output_index":index,"item_id":id,"arguments":arguments}),
        ));
        let mut item = item;
        item["arguments"] = json!(arguments);
        item["status"] = json!("completed");
        events.push(self.event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        ));
        self.output.push(item);
        // A later text part must start a new output item after this tool call.
        self.text_index = None;
        Ok(events)
    }

    fn close_text(&mut self) -> Vec<JsonSseEvent> {
        let Some(index) = self.text_index.take() else {
            return Vec::new();
        };
        let Some(item) = self.output.get_mut(index) else {
            return Vec::new();
        };
        item["status"] = json!("completed");
        let item = item.clone();
        let text = item
            .pointer("/content/0/text")
            .cloned()
            .unwrap_or_else(|| json!(""));
        vec![
            self.event("response.output_text.done",json!({"output_index":index,"content_index":0,"item_id":item.get("id"),"text":text})),
            self.event("response.content_part.done",json!({"output_index":index,"content_index":0,"item_id":item.get("id"),"part":item.pointer("/content/0")})),
            self.event("response.output_item.done",json!({"output_index":index,"item":item})),
        ]
    }

    fn finish(&mut self) -> Result<Vec<JsonSseEvent>> {
        let reason = self
            .finish_reason
            .as_deref()
            .ok_or_else(|| Error::upstream("Gemini stream ended before finishReason"))?;
        let status = if reason == "MAX_TOKENS" {
            "incomplete"
        } else {
            "completed"
        };
        let mut events = self.close_text();
        let kind = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        events.push(self.event(kind, json!({"response":self.envelope(status)})));
        Ok(events)
    }
}
