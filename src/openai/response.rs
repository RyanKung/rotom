use crate::openai::types::{FunctionCall, ToolCall};
use serde::Serialize;
use serde_json::{Map, Value};

/// List response returned by the models endpoint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelList {
    /// Object kind, always `list`.
    pub object: &'static str,
    /// Models included in the listing.
    pub data: Vec<ModelObject>,
}

/// Single model entry in a models list response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelObject {
    /// Model identifier.
    pub id: String,
    /// Object kind, always `model`.
    pub object: &'static str,
    /// Owning organization label exposed to clients.
    pub owned_by: &'static str,
}

/// OpenAI-compatible non-streaming chat completion response body.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatCompletionResponse {
    /// Response identifier.
    pub id: String,
    /// Object kind, always `chat.completion`.
    pub object: &'static str,
    /// Unix timestamp when the response was created.
    pub created: i64,
    /// Model identifier that produced the response.
    pub model: String,
    /// Completion choices returned by the model.
    pub choices: Vec<ChatChoice>,
    /// Optional token accounting information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Single completion choice in a chat completion response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatChoice {
    /// Choice index in the response.
    pub index: u32,
    /// Assistant message produced for this choice.
    pub message: AssistantMessage,
    /// Reason generation stopped for this choice.
    pub finish_reason: String,
}

/// Assistant message returned in a non-streaming response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AssistantMessage {
    /// Message role, always `assistant`.
    pub role: &'static str,
    /// Assistant text content.
    pub content: String,
    /// Tool calls emitted alongside or instead of text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Generated images returned by hosted image generation tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<GeneratedImage>>,
}

/// Token usage metadata reported by the upstream provider.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub prompt_tokens: u32,
    /// Tokens generated in the completion.
    pub completion_tokens: u32,
    /// Total tokens consumed by the request.
    pub total_tokens: u32,
}

/// OpenAI-compatible Responses API object.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponseObject {
    /// Response identifier.
    pub id: String,
    /// Object kind, always `response`.
    pub object: &'static str,
    /// Unix timestamp when the response was created.
    pub created_at: i64,
    /// Terminal response status.
    pub status: String,
    /// Error details when generation failed.
    pub error: Option<Value>,
    /// Incomplete details when generation ended early.
    pub incomplete_details: Option<Value>,
    /// Top-level instructions associated with this response.
    pub instructions: Option<String>,
    /// Preferred upper bound for generated tokens.
    pub max_output_tokens: Option<u32>,
    /// Model identifier that produced the response.
    pub model: String,
    /// Output items emitted by the response.
    pub output: Vec<ResponseOutputItem>,
    /// Whether tool calls may run in parallel.
    pub parallel_tool_calls: bool,
    /// Whether the response was stored for later retrieval.
    pub store: bool,
    /// Optional sampling temperature recorded on the response.
    pub temperature: Option<f64>,
    /// Optional nucleus sampling parameter recorded on the response.
    pub top_p: Option<f64>,
    /// Text output configuration recorded on the response.
    pub text: Value,
    /// Tool choice recorded on the response.
    pub tool_choice: Option<Value>,
    /// Tool definitions recorded on the response.
    pub tools: Vec<Value>,
    /// Truncation strategy recorded on the response.
    pub truncation: String,
    /// Optional token accounting information.
    pub usage: Option<Usage>,
    /// Optional user identifier recorded on the response.
    pub user: Option<String>,
    /// Optional user metadata preserved on the response.
    pub metadata: Option<Map<String, Value>>,
    /// Optional reasoning configuration recorded on the response.
    pub reasoning: Option<Value>,
    /// Identifier of the referenced previous response, when supplied.
    pub previous_response_id: Option<String>,
}

/// Single output item returned by the Responses API.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseOutputItem {
    /// Output item identifier.
    pub id: String,
    /// Object type, such as `message` or `function_call`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Role associated with the output item when it is a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// Item status, always `completed` for fully collected local responses.
    pub status: String,
    /// Message content blocks, when the output item is a message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ResponseOutputContent>,
    /// Tool call identifier, when the output item is a function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Tool name, when the output item is a function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// JSON-encoded tool arguments, when the output item is a function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// Base64 image payload emitted by an image generation call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Revised prompt reported by the upstream image generator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
    /// Reasoning summary payload emitted by reasoning-capable upstream models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
    /// Opaque encrypted reasoning payload emitted by the upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
}

/// Message content block within a Responses API output item.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseOutputContent {
    /// Content type, currently `output_text`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Text payload emitted by the model.
    pub text: String,
    /// Output annotations attached to the text.
    pub annotations: Vec<Value>,
}

/// Generated image payload surfaced by compatibility endpoints.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GeneratedImage {
    /// Base64-encoded image payload.
    pub b64_json: String,
    /// MIME type of the generated image when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Revised prompt reported by the upstream image generator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

/// Classic OpenAI-compatible Images API response payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ImageGenerationResponse {
    /// Unix timestamp when the images were created.
    pub created: i64,
    /// Generated image entries.
    pub data: Vec<GeneratedImage>,
}

/// Response returned by `POST /v1/responses/input_tokens`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseInputTokens {
    /// Object kind, always `response.input_tokens`.
    pub object: &'static str,
    /// Estimated input token count for the submitted request.
    pub input_tokens: u32,
}

/// Response returned by `POST /v1/responses/compact`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseCompaction {
    /// Compacted input items suitable for later Responses API reuse.
    pub output: Vec<Value>,
}

/// Response returned by `DELETE /v1/responses/{response_id}`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseDeleted {
    /// Deleted response identifier.
    pub id: String,
    /// Object kind, always `response`.
    pub object: &'static str,
    /// Whether the resource was deleted.
    pub deleted: bool,
}

/// List response returned by `GET /v1/responses/{response_id}/input_items`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseInputItemList {
    /// Object kind, always `list`.
    pub object: &'static str,
    /// Returned input items.
    pub data: Vec<Value>,
    /// First returned item identifier, when available.
    pub first_id: Option<String>,
    /// Last returned item identifier, when available.
    pub last_id: Option<String>,
    /// Whether more items are available after this page.
    pub has_more: bool,
}

/// OpenAI-compatible streamed chat completion chunk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatCompletionChunk {
    /// Response identifier shared across chunks.
    pub id: String,
    /// Object kind, always `chat.completion.chunk`.
    pub object: &'static str,
    /// Unix timestamp when the stream was created.
    pub created: i64,
    /// Model identifier that produced the stream.
    pub model: String,
    /// Incremental choice updates contained in this chunk.
    pub choices: Vec<ChunkChoice>,
}

/// Single choice delta inside a streaming chunk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChunkChoice {
    /// Choice index in the response.
    pub index: u32,
    /// Incremental message delta for this choice.
    pub delta: DeltaMessage,
    /// Optional terminal stop reason when the choice finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Incremental assistant message payload carried by a stream chunk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeltaMessage {
    /// Role emitted at the start of the stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// Text delta appended to the current assistant message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Incremental tool call deltas emitted by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// Streaming representation of a single tool call delta.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolCallDelta {
    /// Zero-based tool call index within the assistant turn.
    pub index: u32,
    /// Stable identifier on the first delta; empty on argument continuations.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Tool call type on the first delta; empty on argument continuations.
    #[serde(rename = "type", skip_serializing_if = "str::is_empty")]
    pub kind: &'static str,
    /// Incremental payload; an empty name is omitted from serialized deltas.
    #[serde(serialize_with = "serialize_function_delta")]
    pub function: FunctionCall,
}

/// Serializes the streaming function shape without changing complete calls.
fn serialize_function_delta<S: serde::Serializer>(
    function: &FunctionCall,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    // A continuation carries arguments only; repeating the name would append it
    // again in clients that assemble every string field incrementally.
    let mut fields = serializer.serialize_map(None)?;
    if !function.name.is_empty() {
        fields.serialize_entry("name", &function.name)?;
    }
    fields.serialize_entry("arguments", &function.arguments)?;
    fields.end()
}

impl ModelList {
    /// Builds a model list from a sequence of model identifiers.
    #[must_use]
    pub fn from_ids(ids: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            object: "list",
            data: ids
                .into_iter()
                .map(|id| ModelObject {
                    id: id.as_ref().to_owned(),
                    object: "model",
                    owned_by: "openai-codex",
                })
                .collect(),
        }
    }

    /// Builds a model list from model identifiers paired with owner labels.
    #[must_use]
    pub fn from_id_owners(ids: impl IntoIterator<Item = (String, &'static str)>) -> Self {
        Self {
            object: "list",
            data: ids
                .into_iter()
                .map(|(id, owned_by)| ModelObject {
                    id,
                    object: "model",
                    owned_by,
                })
                .collect(),
        }
    }
}

/// Builds a message output item for the Responses API.
#[must_use]
pub fn response_message_item(id: String, text: Option<String>) -> ResponseOutputItem {
    let content = text
        .filter(|value| !value.is_empty())
        .map(|value| {
            vec![ResponseOutputContent {
                kind: "output_text",
                text: value,
                annotations: Vec::new(),
            }]
        })
        .unwrap_or_default();

    ResponseOutputItem {
        id,
        kind: "message".to_owned(),
        role: Some("assistant"),
        status: "completed".to_owned(),
        content,
        call_id: None,
        name: None,
        arguments: None,
        result: None,
        revised_prompt: None,
        summary: None,
        encrypted_content: None,
    }
}

/// Builds a function call output item for the Responses API.
#[must_use]
pub fn response_function_call_item(id: String, tool_call: ToolCall) -> ResponseOutputItem {
    ResponseOutputItem {
        id,
        kind: "function_call".to_owned(),
        role: None,
        status: "completed".to_owned(),
        content: Vec::new(),
        call_id: Some(tool_call.id),
        name: Some(tool_call.function.name),
        arguments: Some(tool_call.function.arguments),
        result: None,
        revised_prompt: None,
        summary: None,
        encrypted_content: None,
    }
}

/// Builds an image generation output item for the Responses API.
#[must_use]
pub fn response_image_generation_item(
    id: String,
    result: String,
    revised_prompt: Option<String>,
) -> ResponseOutputItem {
    ResponseOutputItem {
        id,
        kind: "image_generation_call".to_owned(),
        role: None,
        status: "completed".to_owned(),
        content: Vec::new(),
        call_id: None,
        name: None,
        arguments: None,
        result: Some(result),
        revised_prompt,
        summary: None,
        encrypted_content: None,
    }
}

/// Builds a reasoning output item for the Responses API.
#[must_use]
pub fn response_reasoning_item(
    id: String,
    summary: Option<Value>,
    encrypted_content: Option<String>,
) -> ResponseOutputItem {
    ResponseOutputItem {
        id,
        kind: "reasoning".to_owned(),
        role: None,
        status: "completed".to_owned(),
        content: Vec::new(),
        call_id: None,
        name: None,
        arguments: None,
        result: None,
        revised_prompt: None,
        summary,
        encrypted_content,
    }
}

/// Builds an empty Images API response payload.
#[must_use]
pub const fn image_generation_response(
    created: i64,
    data: Vec<GeneratedImage>,
) -> ImageGenerationResponse {
    ImageGenerationResponse { created, data }
}

/// Builds a local Responses API deletion result.
#[must_use]
pub const fn response_deleted(id: String) -> ResponseDeleted {
    ResponseDeleted {
        id,
        object: "response",
        deleted: true,
    }
}

/// Builds a Responses API input item list.
#[must_use]
pub fn response_input_item_list(data: Vec<Value>, has_more: bool) -> ResponseInputItemList {
    let first_id = data.first().and_then(response_item_id);
    let last_id = data.last().and_then(response_item_id);
    ResponseInputItemList {
        object: "list",
        data,
        first_id,
        last_id,
        has_more,
    }
}

fn response_item_id(item: &Value) -> Option<String> {
    item.get("id").and_then(Value::as_str).map(str::to_owned)
}

/// Parses a generated image payload from a Responses output item.
#[must_use]
pub fn generated_image_from_item(item: &Value) -> Option<GeneratedImage> {
    let raw = item
        .get("result")
        .or_else(|| item.get("b64_json"))
        .and_then(Value::as_str)?;
    let (media_type, b64_json) = parse_image_payload(raw, item);

    Some(GeneratedImage {
        b64_json,
        media_type,
        revised_prompt: item
            .get("revised_prompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn parse_image_payload(raw: &str, item: &Value) -> (Option<String>, String) {
    if let Some(rest) = raw.strip_prefix("data:") {
        if let Some((header, data)) = rest.split_once(',') {
            let media_type = header
                .split(';')
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            return (media_type, data.to_owned());
        }
    }

    let media_type = item
        .get("media_type")
        .or_else(|| item.get("mime_type"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            item.get("output_format")
                .and_then(Value::as_str)
                .map(|format| format!("image/{format}"))
        });

    (media_type, raw.to_owned())
}

/// Extracts every generated image from a Responses output array.
#[must_use]
pub fn generated_images_from_output(items: &[Value]) -> Vec<GeneratedImage> {
    items.iter().filter_map(generated_image_from_item).collect()
}

/// Extracts every generated image from a serialized response output item list.
#[must_use]
pub fn generated_images_from_response_items(items: &[ResponseOutputItem]) -> Vec<GeneratedImage> {
    items
        .iter()
        .filter_map(|item| {
            let raw = item.result.as_deref()?;
            Some(GeneratedImage {
                b64_json: raw.to_owned(),
                media_type: None,
                revised_prompt: item.revised_prompt.clone(),
            })
        })
        .collect()
}

/// Converts a generated image back into a data URL when a multimodal input needs it.
#[must_use]
pub fn generated_image_data_url(image: &GeneratedImage) -> String {
    let media_type = image.media_type.as_deref().unwrap_or("image/png");
    format!("data:{media_type};base64,{}", image.b64_json)
}

/// Builds the initial stream chunk that introduces the assistant role.
#[must_use]
pub fn chunk_with_role(id: &str, created: i64, model: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: Some("assistant"),
                content: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

/// Builds a stream chunk that carries an assistant text delta.
#[must_use]
pub fn chunk_with_content(
    id: &str,
    created: i64,
    model: &str,
    content: String,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content: Some(content),
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

/// Builds a stream chunk that carries a tool call delta.
#[must_use]
pub fn chunk_with_tool_call(
    id: &str,
    created: i64,
    model: &str,
    index: u32,
    tool_call: ToolCall,
) -> ChatCompletionChunk {
    chunk_with_tool_call_delta(
        id,
        created,
        model,
        ToolCallDelta {
            index,
            id: tool_call.id,
            kind: "function",
            function: tool_call.function,
        },
    )
}

/// Builds a chunk carrying a tool introduction or an argument continuation.
#[must_use]
pub fn chunk_with_tool_call_delta(
    id: &str,
    created: i64,
    model: &str,
    delta: ToolCallDelta,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content: None,
                tool_calls: Some(vec![delta]),
            },
            finish_reason: None,
        }],
    }
}

/// Builds the terminal stream chunk for a finished choice.
#[must_use]
pub fn chunk_finished(id: &str, created: i64, model: &str, reason: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content: None,
                tool_calls: None,
            },
            finish_reason: Some(reason.to_owned()),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_empty_assistant_content_as_string() {
        let message = AssistantMessage {
            role: "assistant",
            content: String::new(),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_owned(),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name: "lookup".to_owned(),
                    arguments: "{}".to_owned(),
                },
            }]),
            images: None,
        };

        assert_eq!(serde_json::to_value(message).unwrap()["content"], "");
    }

    #[test]
    fn extracts_generated_image_from_plain_base64_result() {
        let image = generated_image_from_item(&json!({
            "type": "image_generation_call",
            "result": "YWJj",
            "output_format": "png",
            "revised_prompt": "refined"
        }))
        .unwrap();

        assert_eq!(image.b64_json, "YWJj");
        assert_eq!(image.media_type.as_deref(), Some("image/png"));
        assert_eq!(image.revised_prompt.as_deref(), Some("refined"));
    }

    #[test]
    fn extracts_generated_image_from_data_url_result() {
        let image = generated_image_from_item(&json!({
            "type": "image_generation_call",
            "result": "data:image/webp;base64,AAAA"
        }))
        .unwrap();

        assert_eq!(image.b64_json, "AAAA");
        assert_eq!(image.media_type.as_deref(), Some("image/webp"));
    }
}
