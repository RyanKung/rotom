use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Anthropic-compatible Messages API request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest {
    /// Target model identifier.
    pub model: String,
    /// Conversation history supplied to the model.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Optional top-level system prompt.
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    /// Maximum number of output tokens requested.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Whether the caller requested SSE streaming.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Sampling temperature, when supported upstream.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter, preserved for compatibility.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Tools exposed to the model.
    #[serde(default)]
    pub tools: Option<Vec<ToolDefinition>>,
    /// Requested tool selection mode or explicit tool choice.
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// Optional stop sequences understood by Anthropic clients.
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    /// Optional thinking configuration passed through by compatible clients.
    #[serde(default)]
    pub thinking: Option<Value>,
    /// Optional Anthropic output configuration, including effort.
    #[serde(default)]
    pub output_config: Option<Value>,
    /// Optional Anthropic service tier hint such as `auto` or `standard_only`.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Optional Anthropic beta speed hint such as `fast`.
    #[serde(default)]
    pub speed: Option<String>,
    /// Additional provider-specific fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

const CODEX_PRIORITY_SERVICE_TIER: &str = "priority";

impl MessagesRequest {
    /// Returns whether the request should use streaming responses.
    #[must_use]
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Maps Anthropic priority/speed controls to the upstream Codex service tier.
    #[must_use]
    pub(crate) fn upstream_service_tier(&self) -> Option<String> {
        if self
            .speed
            .as_deref()
            .map(str::trim)
            .is_some_and(|speed| speed.eq_ignore_ascii_case("fast"))
        {
            return Some(CODEX_PRIORITY_SERVICE_TIER.to_owned());
        }

        let tier = self.service_tier.as_deref()?.trim();
        if tier.eq_ignore_ascii_case("auto")
            || tier.eq_ignore_ascii_case("priority")
            || tier.eq_ignore_ascii_case("fast")
        {
            Some(CODEX_PRIORITY_SERVICE_TIER.to_owned())
        } else {
            None
        }
    }

    /// Returns the normalized Anthropic output effort, when the caller supplied one.
    #[must_use]
    pub(crate) fn output_effort(&self) -> Option<String> {
        self.output_config
            .as_ref()
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .and_then(|value| {
                let model = self
                    .model
                    .strip_prefix("openai-codex/")
                    .unwrap_or(&self.model);
                // Model routing happens before this conversion. GPT-6 supports
                // a distinct max effort; keep legacy aliases for other models.
                if value.trim().eq_ignore_ascii_case("max")
                    && (model.starts_with("gpt-6-") || model.starts_with("gpt-6."))
                {
                    Some("max".to_owned())
                } else {
                    normalize_anthropic_effort(value)
                }
            })
    }
}

/// Converts Anthropic effort aliases into Codex reasoning effort values.
pub fn normalize_anthropic_effort(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "low" | "medium" | "high" | "xhigh" => Some(normalized),
        "extra_high" | "extra" | "max" => Some("xhigh".to_owned()),
        "minimal" | "min" => Some("low".to_owned()),
        _ => None,
    }
}

/// Anthropic-compatible input message.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    /// Message role such as `user` or `assistant`.
    pub role: String,
    /// Message payload as text or structured blocks.
    pub content: MessageContent,
}

/// Anthropic accepts either a string or an array of blocks for message content.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text message content.
    Text(String),
    /// Structured content blocks.
    Blocks(Vec<ContentBlock>),
}

/// Top-level system prompt can be a string or an array of blocks.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// Plain text system prompt.
    Text(String),
    /// Structured system prompt blocks.
    Blocks(Vec<SystemBlock>),
}

/// Supported Anthropic system block shape.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemBlock {
    /// Block type, typically `text`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional text payload carried by the block.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional prompt caching directive attached to this block.
    #[serde(default)]
    pub cache_control: Option<Value>,
}

/// Minimal Anthropic content block support needed by SDKs and Claude Code.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContentBlock {
    /// Content block type such as `text`, `image`, `tool_use`, or `tool_result`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Inline text payload for text-like blocks.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional prompt caching directive attached to this block.
    #[serde(default)]
    pub cache_control: Option<Value>,
    /// Image source for image blocks.
    #[serde(default)]
    pub source: Option<ImageSource>,
    /// Optional nested content for document-like blocks.
    #[serde(default)]
    pub document_content: Option<Vec<Self>>,
    /// Tool use identifier for `tool_use` blocks.
    #[serde(default)]
    pub id: Option<String>,
    /// Tool name for `tool_use` blocks.
    #[serde(default)]
    pub name: Option<String>,
    /// JSON input payload for `tool_use` blocks.
    #[serde(default)]
    pub input: Option<Value>,
    /// Referenced tool call identifier for `tool_result` blocks.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Structured or plain-text tool result content.
    #[serde(default)]
    pub content: Option<ToolResultContent>,
    /// Whether a tool result represents an error.
    #[serde(default)]
    pub is_error: Option<bool>,
    /// Reasoning text for `thinking` blocks.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Optional signature attached to signed thinking blocks.
    #[serde(default)]
    pub signature: Option<String>,
}

/// Base64 image source accepted by the Anthropic Messages API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageSource {
    /// Source type, typically `base64`.
    #[serde(rename = "type")]
    pub kind: String,
    /// MIME type for the encoded image.
    #[serde(default)]
    pub media_type: Option<String>,
    /// Base64-encoded image payload.
    #[serde(default)]
    pub data: Option<String>,
    /// Inline text payload for text-backed sources.
    #[serde(default)]
    pub text: Option<String>,
    /// URL-backed sources when the caller references a remote asset.
    #[serde(default)]
    pub url: Option<String>,
    /// File-backed sources when the caller references an uploaded file.
    #[serde(default)]
    pub file_id: Option<String>,
}

/// Tool result content may arrive as a string or as a list of text blocks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// Plain-text tool output.
    Text(String),
    /// Structured tool output blocks.
    Blocks(Vec<ContentBlock>),
}

/// Anthropic tool definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolDefinition {
    /// Tool name exposed to the model.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing accepted tool input.
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// Optional prompt caching directive attached to this tool definition.
    #[serde(default)]
    pub cache_control: Option<Value>,
}

/// Anthropic Messages API response body.
#[derive(Debug, Clone, Serialize)]
pub struct MessageResponse {
    /// Anthropic message identifier.
    pub id: String,
    /// Object kind, always `message`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Message role, always `assistant` for generated responses.
    pub role: &'static str,
    /// Model identifier that produced the response.
    pub model: String,
    /// Response content blocks in Anthropic format.
    pub content: Vec<ResponseContentBlock>,
    /// Terminal reason reported to Anthropic clients.
    pub stop_reason: &'static str,
    /// Matching stop sequence when one caused termination.
    pub stop_sequence: Option<String>,
    /// Token usage metadata.
    pub usage: ResponseUsage,
}

/// Anthropic response content block.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ResponseContentBlock {
    /// Plain text response content.
    #[serde(rename = "text")]
    Text {
        /// Text emitted for this response block.
        text: String,
    },
    /// Tool invocation content.
    #[serde(rename = "tool_use")]
    ToolUse {
        /// Tool call identifier.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed JSON tool input.
        input: Value,
    },
    /// Non-standard rotom extension for generated image output.
    #[serde(rename = "image")]
    Image {
        /// Base64 image payload.
        source: ImageSource,
    },
    /// Thinking output surfaced by reasoning-capable upstream models.
    #[serde(rename = "thinking")]
    Thinking {
        /// Reasoning text emitted by the model.
        thinking: String,
        /// Opaque signature carried by Anthropic thinking blocks.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// Anthropic usage fields for Messages responses.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseUsage {
    /// Estimated or reported prompt token count.
    pub input_tokens: u32,
    /// Number of prompt tokens written into cache on this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// Number of prompt tokens read from cache on this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Estimated or reported output token count.
    pub output_tokens: u32,
    /// Optional hosted server-tool usage summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<Value>,
}

/// Anthropic token counting response body.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CountTokensResponse {
    /// Estimated input token count for the submitted request.
    pub input_tokens: u32,
}

/// Anthropic-compatible models list response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelsResponse {
    /// Listed models.
    pub data: Vec<ModelInfo>,
    /// First model identifier when available.
    pub first_id: Option<String>,
    /// Whether more models remain after this page.
    pub has_more: bool,
    /// Last model identifier when available.
    pub last_id: Option<String>,
}

/// Anthropic-compatible model object.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model creation timestamp in RFC 3339 form.
    pub created_at: String,
    /// Human-readable model name.
    pub display_name: String,
    /// Model identifier.
    pub id: String,
    /// Object kind, always `model`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Anthropic message batch creation request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageBatchCreateRequest {
    /// Individual message creation requests to process in the batch.
    pub requests: Vec<MessageBatchRequest>,
}

/// Single request inside an Anthropic message batch.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageBatchRequest {
    /// Caller-defined identifier used to match results back to inputs.
    pub custom_id: String,
    /// Parameters for the embedded message creation request.
    pub params: MessagesRequest,
}

/// Anthropic-compatible message batch object.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatch {
    /// Time at which the batch is archived, if ever.
    pub archived_at: Option<String>,
    /// Time at which cancellation was initiated, if ever.
    pub cancel_initiated_at: Option<String>,
    /// Time at which the batch was created.
    pub created_at: String,
    /// Time at which batch processing ended.
    pub ended_at: Option<String>,
    /// Time at which the batch will expire.
    pub expires_at: String,
    /// Batch identifier.
    pub id: String,
    /// Current processing status.
    pub processing_status: &'static str,
    /// Counts grouped by terminal request state.
    pub request_counts: MessageBatchRequestCounts,
    /// URL where JSONL batch results can be fetched.
    pub results_url: Option<String>,
    /// Object kind, always `message_batch`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Anthropic-compatible list response for message batches.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatchListResponse {
    /// Listed message batches, ordered newest first.
    pub data: Vec<MessageBatch>,
    /// First batch identifier when available.
    pub first_id: Option<String>,
    /// Whether more batches remain after this page.
    pub has_more: bool,
    /// Last batch identifier when available.
    pub last_id: Option<String>,
}

/// Anthropic-compatible request state counters for a batch.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatchRequestCounts {
    /// Number of canceled requests.
    pub canceled: u32,
    /// Number of errored requests.
    pub errored: u32,
    /// Number of expired requests.
    pub expired: u32,
    /// Number of requests still processing.
    pub processing: u32,
    /// Number of succeeded requests.
    pub succeeded: u32,
}

/// Single line emitted by the Anthropic message batch results endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct MessageBatchResult {
    /// Caller-defined identifier copied from the request.
    pub custom_id: String,
    /// Terminal result payload for the request.
    pub result: MessageBatchResultType,
}

/// Terminal result variants for Anthropic message batch items.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum MessageBatchResultType {
    /// Successful message creation result.
    #[serde(rename = "succeeded")]
    Succeeded {
        /// Completed message object.
        message: MessageResponse,
    },
    /// Failed message creation result.
    #[serde(rename = "errored")]
    Errored {
        /// Error object returned for the failed request.
        error: Value,
    },
    /// Request canceled before execution completed.
    #[serde(rename = "canceled")]
    Canceled,
}

/// Anthropic-compatible deletion confirmation for a message batch.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatchDeleted {
    /// Identifier of the deleted message batch.
    pub id: String,
    /// Object kind, always `message_batch_deleted`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// SSE payload for `message_start`.
#[derive(Debug, Clone, Serialize)]
pub struct MessageStartEvent {
    /// Event payload type, always `message_start`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Initial partial message object for the stream.
    pub message: StreamingMessage,
}

/// Partial message object used by stream events.
#[derive(Debug, Clone, Serialize)]
pub struct StreamingMessage {
    /// Anthropic message identifier shared across the stream.
    pub id: String,
    /// Object kind, always `message`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Message role, always `assistant`.
    pub role: &'static str,
    /// Content blocks accumulated so far.
    pub content: Vec<Value>,
    /// Model identifier producing the stream.
    pub model: String,
    /// Final stop reason, omitted until the stream ends.
    pub stop_reason: Option<String>,
    /// Final stop sequence, if any.
    pub stop_sequence: Option<String>,
    /// Incremental token usage metadata.
    pub usage: ResponseUsage,
}
