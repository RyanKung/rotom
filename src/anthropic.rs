//! Anthropic-compatible request, response, and streaming adapters.
//!
//! This module exposes a minimal Messages API surface that matches the parts
//! Claude Code and Anthropic SDKs commonly rely on: `/v1/messages` and
//! `/v1/messages/count_tokens`.

use crate::{
    Error, Result,
    openai::{
        response::{
            ChatCompletionResponse, ResponseObject, Usage, generated_images_from_output,
            generated_images_from_response_items,
        },
        types::{
            ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool,
            FunctionTool, ImageUrl, ToolCall,
        },
    },
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

mod events;
mod types;

pub use events::{
    content_block_stop, error_body, image_block_start, message_delta_event, message_start_event,
    message_stop_event, signature_delta, text_block_start, text_delta, thinking_block_start,
    thinking_delta, tool_block_start, tool_json_delta,
};
pub(crate) use types::normalize_anthropic_effort;
pub use types::{
    ContentBlock, CountTokensResponse, ImageSource, Message, MessageBatch,
    MessageBatchCreateRequest, MessageBatchDeleted, MessageBatchListResponse, MessageBatchRequest,
    MessageBatchRequestCounts, MessageBatchResult, MessageBatchResultType, MessageContent,
    MessageResponse, MessagesRequest, ModelInfo, ModelsResponse, ResponseContentBlock,
    ResponseUsage, StreamingMessage, SystemBlock, SystemPrompt, ToolDefinition, ToolResultContent,
};

const CLAUDE_CODE_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// Converts an Anthropic Messages request into the existing OpenAI-like request
/// type used by the Codex upstream adapter.
///
/// # Errors
///
/// Returns an error when the request contains unsupported Anthropic roles or
/// malformed tool blocks that cannot be mapped into the OpenAI-compatible
/// request shape.
pub fn to_openai_request(request: &MessagesRequest) -> Result<ChatCompletionRequest> {
    let mut messages = Vec::new();
    if let Some(system) = system_prompt_text(request.system.as_ref()) {
        messages.push(ChatMessage {
            role: "system".to_owned(),
            content: Some(ChatContent::Text(system)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }

    for message in &request.messages {
        append_message(&mut messages, message)?;
    }

    Ok(ChatCompletionRequest {
        model: request.model.clone(),
        messages,
        stream: request.stream,
        temperature: request.temperature,
        top_p: request.top_p,
        tools: request.tools.as_ref().map(|tools| convert_tools(tools)),
        tool_choice: request.tool_choice.clone().map(convert_tool_choice),
        service_tier: request.upstream_service_tier(),
        reasoning_effort: request.output_effort(),
        max_completion_tokens: request.max_tokens,
        max_tokens: request.max_tokens,
        parallel_tool_calls: Some(parallel_tool_calls_enabled(request.tool_choice.as_ref())),
        stop: request.stop_sequences.clone(),
        extra: request.extra.clone(),
    })
}

/// Maps the OpenAI-compatible chat response into an Anthropic Messages response.
pub fn from_openai_response(response: ChatCompletionResponse) -> MessageResponse {
    let choice = response
        .choices
        .into_iter()
        .next()
        .unwrap_or_else(empty_choice);
    let mut content = Vec::new();

    if !choice.message.content.is_empty() {
        content.push(ResponseContentBlock::Text {
            text: choice.message.content,
        });
    }

    for tool_call in choice.message.tool_calls.into_iter().flatten() {
        content.push(ResponseContentBlock::ToolUse {
            id: tool_call.id,
            name: tool_call.function.name,
            input: parse_arguments(&tool_call.function.arguments),
        });
    }

    for image in choice.message.images.into_iter().flatten() {
        content.push(ResponseContentBlock::Image {
            source: ImageSource {
                kind: "base64".to_owned(),
                media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                data: Some(image.b64_json),
                text: None,
                url: None,
                file_id: None,
            },
        });
    }

    let usage = response.usage.unwrap_or(Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });

    MessageResponse {
        id: response.id.replace("chatcmpl", "msg"),
        kind: "message",
        role: "assistant",
        model: response.model,
        content,
        stop_reason: map_stop_reason(&choice.finish_reason),
        stop_sequence: None,
        usage: ResponseUsage {
            input_tokens: usage.prompt_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: usage.completion_tokens,
            server_tool_use: None,
        },
    }
}

/// Maps an OpenAI-compatible Responses object into an Anthropic Messages response.
#[must_use]
pub fn from_openai_response_object(response: ResponseObject) -> MessageResponse {
    let mut content = Vec::new();

    for item in &response.output {
        match item.kind.as_str() {
            "message" => {
                let text = item
                    .content
                    .iter()
                    .map(|part| part.text.clone())
                    .collect::<String>();
                if !text.is_empty() {
                    content.push(ResponseContentBlock::Text { text });
                }
            }
            "function_call" => {
                content.push(ResponseContentBlock::ToolUse {
                    id: item.call_id.clone().unwrap_or_else(|| item.id.clone()),
                    name: item.name.clone().unwrap_or_default(),
                    input: parse_arguments(item.arguments.as_deref().unwrap_or("{}")),
                });
            }
            "reasoning" => {
                let thinking = item
                    .summary
                    .as_ref()
                    .and_then(Value::as_array)
                    .map_or_else(String::new, |parts| joined_reasoning_text(parts));
                if !thinking.is_empty() {
                    content.push(ResponseContentBlock::Thinking {
                        thinking,
                        signature: item.encrypted_content.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    for image in generated_images_from_response_items(&response.output) {
        content.push(ResponseContentBlock::Image {
            source: ImageSource {
                kind: "base64".to_owned(),
                media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                data: Some(image.b64_json),
                text: None,
                url: None,
                file_id: None,
            },
        });
    }

    let stop_reason = response_stop_reason(&response);
    let usage = response.usage.unwrap_or(Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });

    MessageResponse {
        id: response.id.replace("resp", "msg"),
        kind: "message",
        role: "assistant",
        model: response.model,
        content,
        stop_reason,
        stop_sequence: None,
        usage: ResponseUsage {
            input_tokens: usage.prompt_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: usage.completion_tokens,
            server_tool_use: None,
        },
    }
}

/// Maps a raw OpenAI/Codex Responses payload into an Anthropic Messages response.
#[must_use]
pub fn from_openai_response_value(response: &Value, fallback_model: &str) -> MessageResponse {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .map_or(&[] as &[Value], Vec::as_slice);
    let mut content = output
        .iter()
        .flat_map(response_content_blocks_from_value)
        .collect::<Vec<_>>();
    content.extend(
        generated_images_from_output(output)
            .into_iter()
            .map(|image| ResponseContentBlock::Image {
                source: ImageSource {
                    kind: "base64".to_owned(),
                    media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                    data: Some(image.b64_json),
                    text: None,
                    url: None,
                    file_id: None,
                },
            }),
    );

    let usage = response
        .get("usage")
        .and_then(parse_anthropic_usage_value)
        .unwrap_or_else(|| default_response_usage(0, 0));

    MessageResponse {
        id: response
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| "msg_local".to_owned(), |id| id.replace("resp", "msg")),
        kind: "message",
        role: "assistant",
        model: response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
            .to_owned(),
        content,
        stop_reason: response_stop_reason_value(response),
        stop_sequence: None,
        usage,
    }
}

fn response_content_blocks_from_value(item: &Value) -> Vec<ResponseContentBlock> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => response_message_blocks_from_value(item),
        Some("function_call") => vec![ResponseContentBlock::ToolUse {
            id: item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            input: parse_arguments(
                item.get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}"),
            ),
        }],
        Some("reasoning") => response_reasoning_blocks_from_value(item),
        _ => Vec::new(),
    }
}

fn response_message_blocks_from_value(item: &Value) -> Vec<ResponseContentBlock> {
    let text = item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ResponseContentBlock::Text { text }]
    }
}

fn response_reasoning_blocks_from_value(item: &Value) -> Vec<ResponseContentBlock> {
    let thinking = item
        .get("summary")
        .and_then(Value::as_array)
        .map_or_else(String::new, |parts| joined_reasoning_text(parts));
    let thinking = if thinking.is_empty() {
        item.get("content")
            .and_then(Value::as_array)
            .map_or_else(String::new, |parts| joined_reasoning_text(parts))
    } else {
        thinking
    };

    if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ResponseContentBlock::Thinking {
            thinking,
            signature: item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }]
    }
}

fn joined_reasoning_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

/// Produces a compact token estimate for `/v1/messages/count_tokens`.
#[must_use]
pub fn estimate_input_tokens(request: &MessagesRequest) -> u32 {
    let mut text = String::new();
    if let Some(system) = system_prompt_text(request.system.as_ref()) {
        text.push_str(&system);
    }

    for message in &request.messages {
        text.push_str(&message.role);
        match &message.content {
            MessageContent::Text(value) => text.push_str(value),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    append_block_text(&mut text, block);
                }
            }
        }
    }

    if let Some(tools) = request.tools.as_ref() {
        for tool in tools {
            text.push_str(&tool.name);
            if let Some(description) = &tool.description {
                text.push_str(description);
            }
            if let Some(schema) = &tool.input_schema {
                text.push_str(&schema.to_string());
            }
        }
    }

    if let Some(thinking) = request.thinking.as_ref() {
        text.push_str(&thinking.to_string());
    }
    if let Some(output_config) = request.output_config.as_ref() {
        text.push_str(&output_config.to_string());
    }
    if let Some(service_tier) = request.service_tier.as_deref() {
        text.push_str(service_tier);
    }
    if let Some(speed) = request.speed.as_deref() {
        text.push_str(speed);
    }

    estimate_tokens_from_text(&text)
}

/// Builds an Anthropic-compatible models response from the configured model IDs.
#[must_use]
pub fn models_response(
    models: &[crate::openai::response::ModelObject],
    aliases: &BTreeMap<String, String>,
) -> ModelsResponse {
    let ids = discovery_model_ids(models, aliases);
    let data = ids
        .into_iter()
        .map(|id| ModelInfo {
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            description: "Provided by Rotom".to_owned(),
            display_name: anthropic_display_name(&id),
            id,
            kind: "model",
        })
        .collect::<Vec<_>>();
    let first_id = data.first().map(|model| model.id.clone());
    let last_id = data.last().map(|model| model.id.clone());

    ModelsResponse {
        object: "list",
        data,
        first_id,
        has_more: false,
        last_id,
    }
}

/// Adds configured aliases to an OpenAI-compatible model list without
/// removing the native names.
#[must_use]
pub fn model_list_with_aliases(
    models: &crate::openai::response::ModelList,
    aliases: &BTreeMap<String, String>,
) -> crate::openai::response::ModelList {
    let mut data = models.data.clone();
    for model in &models.data {
        for alias in aliases
            .iter()
            .filter(|(_, source)| source.as_str() == model.id)
            .map(|(alias, _)| alias)
        {
            data.push(crate::openai::response::ModelObject {
                id: alias.clone(),
                object: model.object,
                owned_by: model.owned_by,
            });
        }
    }
    crate::openai::response::ModelList {
        object: models.object,
        data,
    }
}

fn discovery_model_ids(
    models: &[crate::openai::response::ModelObject],
    aliases: &BTreeMap<String, String>,
) -> Vec<String> {
    models
        .iter()
        .flat_map(|model| {
            let configured = aliases
                .iter()
                .filter(|(_, source)| source.as_str() == model.id)
                .map(|(alias, _)| alias.clone())
                .collect::<Vec<_>>();
            std::iter::once(model.id.clone())
                .chain(configured)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Builds an Anthropic-compatible list response from stored message batches.
#[must_use]
pub fn message_batch_list_response(batches: Vec<MessageBatch>) -> MessageBatchListResponse {
    let first_id = batches.first().map(|batch| batch.id.clone());
    let last_id = batches.last().map(|batch| batch.id.clone());

    MessageBatchListResponse {
        data: batches,
        first_id,
        has_more: false,
        last_id,
    }
}

fn anthropic_display_name(id: &str) -> String {
    id.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn append_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<()> {
    match message.role.as_str() {
        "system" => {
            append_system_message(messages, message);
            Ok(())
        }
        "user" => {
            append_user_message(messages, message);
            Ok(())
        }
        "assistant" => append_assistant_message(messages, message),
        role => Err(Error::config(format!("unsupported Anthropic role: {role}"))),
    }
}

fn append_system_message(messages: &mut Vec<ChatMessage>, message: &Message) {
    let text = message_content_text(&message.content);
    if let Some(text) = sanitize_system_text(&text) {
        messages.push(ChatMessage {
            role: "system".to_owned(),
            content: Some(ChatContent::Text(text)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }
}

fn append_user_message(messages: &mut Vec<ChatMessage>, message: &Message) {
    match &message.content {
        MessageContent::Text(text) => messages.push(ChatMessage {
            role: "user".to_owned(),
            content: Some(ChatContent::Text(text.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block.kind.as_str() {
                    "text" | "input_text" => parts.push(ChatContentPart {
                        kind: "text".to_owned(),
                        text: text_like_block_text(block),
                        image_url: None,
                    }),
                    "image" => {
                        if let Some(url) = image_data_url(block.source.as_ref()) {
                            parts.push(ChatContentPart {
                                kind: "image_url".to_owned(),
                                text: None,
                                image_url: Some(ImageUrl { url, detail: None }),
                            });
                        }
                    }
                    "document" => {
                        if let Some(text) = document_text(block) {
                            parts.push(ChatContentPart {
                                kind: "text".to_owned(),
                                text: Some(text),
                                image_url: None,
                            });
                        }
                    }
                    "tool_result" => {
                        flush_user_parts(messages, &mut parts);
                        messages.push(ChatMessage {
                            role: "tool".to_owned(),
                            content: Some(ChatContent::Text(tool_result_text(
                                block.content.as_ref(),
                            ))),
                            name: None,
                            tool_call_id: block.tool_use_id.clone(),
                            tool_calls: None,
                        });
                    }
                    _ => {}
                }
            }

            flush_user_parts(messages, &mut parts);
        }
    }
}

fn append_assistant_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<()> {
    match &message.content {
        MessageContent::Text(text) => messages.push(ChatMessage {
            role: "assistant".to_owned(),
            content: Some(ChatContent::Text(text.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }),
        MessageContent::Blocks(blocks) => {
            // Anthropic can interleave text and tool_use blocks in one assistant
            // turn; flatten text into one assistant message and preserve tool
            // calls in the dedicated OpenAI-compatible field.
            let text = blocks
                .iter()
                .filter_map(|block| match block.kind.as_str() {
                    "text" | "input_text" => text_like_block_text(block),
                    "document" => document_text(block),
                    "thinking" => block.thinking.clone(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let tool_calls = blocks
                .iter()
                .filter(|block| block.kind == "tool_use")
                .map(tool_call_from_block)
                .collect::<Result<Vec<_>>>()?;

            messages.push(ChatMessage {
                role: "assistant".to_owned(),
                content: (!text.is_empty()).then_some(ChatContent::Text(text)),
                name: None,
                tool_call_id: None,
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            });
        }
    }
    Ok(())
}

fn message_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                append_block_text(&mut text, block);
            }
            text
        }
    }
}

fn tool_call_from_block(block: &ContentBlock) -> Result<ToolCall> {
    Ok(ToolCall {
        id: block
            .id
            .clone()
            .ok_or_else(|| Error::config("tool_use block missing id"))?,
        kind: "function".to_owned(),
        function: crate::openai::types::FunctionCall {
            name: block
                .name
                .clone()
                .ok_or_else(|| Error::config("tool_use block missing name"))?,
            // Preserve the JSON object as a string because the OpenAI-style
            // request shape stores function arguments as encoded JSON text.
            arguments: block.input.clone().unwrap_or_else(|| json!({})).to_string(),
        },
    })
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<ChatTool> {
    tools
        .iter()
        .map(|tool| ChatTool {
            kind: if tool.name == "image_generation" {
                "image_generation".to_owned()
            } else {
                "function".to_owned()
            },
            function: (tool.name != "image_generation").then_some(FunctionTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
                strict: None,
            }),
            extra: tool
                .cache_control
                .as_ref()
                .map_or_else(Map::new, |cache_control| {
                    let mut extra = Map::new();
                    extra.insert("cache_control".to_owned(), cache_control.clone());
                    extra
                }),
        })
        .collect()
}

/// Maps Anthropic tool selection to the OpenAI-compatible request shape.
fn convert_tool_choice(value: Value) -> Value {
    match value {
        Value::String(string) => match string.as_str() {
            "any" => json!("required"),
            "auto" => json!("auto"),
            "none" => json!("none"),
            _ => Value::String(string),
        },
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => {
                let name = object.get("name").cloned().unwrap_or(Value::Null);
                json!({
                    "type": "function",
                    "function": {"name": name}
                })
            }
            _ => Value::Object(object),
        },
        other => other,
    }
}

/// Honors Anthropic's opt-out flag independently of the selected tool mode.
fn parallel_tool_calls_enabled(tool_choice: Option<&Value>) -> bool {
    match tool_choice {
        Some(Value::String(choice)) => choice != "none",
        Some(Value::Object(object)) => {
            object.get("type").and_then(Value::as_str) != Some("none")
                && object.get("disable_parallel_tool_use") != Some(&Value::Bool(true))
        }
        None | Some(_) => true,
    }
}

pub(crate) fn system_prompt_text(system: Option<&SystemPrompt>) -> Option<String> {
    match system? {
        SystemPrompt::Text(text) => sanitize_system_text(text),
        SystemPrompt::Blocks(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text.as_deref())
                .filter_map(sanitize_system_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
    }
}

pub(crate) fn sanitize_system_text(text: &str) -> Option<String> {
    let stripped = strip_claude_code_billing_header_line(text)
        .trim()
        .to_owned();
    (!stripped.is_empty()).then_some(stripped)
}

fn strip_claude_code_billing_header_line(text: &str) -> &str {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix(CLAUDE_CODE_BILLING_HEADER_PREFIX) else {
        return text;
    };

    rest.find('\n').map_or("", |index| {
        rest[index + 1..].trim_start_matches(['\r', '\n'])
    })
}

fn image_data_url(source: Option<&ImageSource>) -> Option<String> {
    let source = source?;
    let media_type = source.media_type.as_deref().unwrap_or("image/png");
    let data = source.data.as_deref()?;
    Some(format!("data:{media_type};base64,{data}"))
}

fn tool_result_text(content: Option<&ToolResultContent>) -> String {
    match content {
        Some(ToolResultContent::Text(text)) => text.clone(),
        Some(ToolResultContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| block.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn flush_user_parts(messages: &mut Vec<ChatMessage>, parts: &mut Vec<ChatContentPart>) {
    if parts.is_empty() {
        return;
    }

    messages.push(ChatMessage {
        role: "user".to_owned(),
        content: Some(ChatContent::Parts(std::mem::take(parts))),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });
}

fn text_like_block_text(block: &ContentBlock) -> Option<String> {
    block
        .text
        .clone()
        .or_else(|| block.thinking.clone())
        .or_else(|| block.source.as_ref().and_then(|source| source.text.clone()))
}

fn document_text(block: &ContentBlock) -> Option<String> {
    let mut fragments = Vec::new();
    if let Some(text) = block.text.as_deref() {
        fragments.push(text.to_owned());
    }
    if let Some(source) = block.source.as_ref() {
        if let Some(text) = source.text.as_deref() {
            fragments.push(text.to_owned());
        }
        if let Some(data) = source.data.as_deref() {
            fragments.push(data.to_owned());
        }
        if let Some(url) = source.url.as_deref() {
            fragments.push(format!("[document:{url}]"));
        }
        if let Some(file_id) = source.file_id.as_deref() {
            fragments.push(format!("[document_file:{file_id}]"));
        }
    }
    if let Some(content) = block.document_content.as_ref() {
        for nested in content {
            let mut text = String::new();
            append_block_text(&mut text, nested);
            if !text.is_empty() {
                fragments.push(text);
            }
        }
    }

    let combined = fragments.join("\n");
    (!combined.is_empty()).then_some(combined)
}

fn parse_arguments(arguments: &str) -> Value {
    // Anthropic expects parsed JSON input for tool_use blocks; keep malformed
    // arguments accessible instead of failing the entire response conversion.
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "_raw": arguments }))
}

fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" => "tool_use",
        "length" | "max_output_tokens" | "max_tokens" => "max_tokens",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

fn response_stop_reason(response: &ResponseObject) -> &'static str {
    if response
        .output
        .iter()
        .any(|item| item.kind == "function_call")
    {
        return "tool_use";
    }

    if response.status == "incomplete" {
        return response
            .incomplete_details
            .as_ref()
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
            .map_or("max_tokens", map_stop_reason);
    }

    "end_turn"
}

fn response_stop_reason_value(response: &Value) -> &'static str {
    if response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        })
    {
        return "tool_use";
    }

    if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        return response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
            .map_or("max_tokens", map_stop_reason);
    }

    response
        .get("stop_reason")
        .or_else(|| response.get("finish_reason"))
        .and_then(Value::as_str)
        .map_or("end_turn", map_stop_reason)
}

const fn default_response_usage(input_tokens: u32, output_tokens: u32) -> ResponseUsage {
    ResponseUsage {
        input_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        output_tokens,
        server_tool_use: None,
    }
}

fn parse_anthropic_usage_value(value: &Value) -> Option<ResponseUsage> {
    let prompt_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())?;
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);

    Some(ResponseUsage {
        input_tokens: prompt_tokens,
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        cache_read_input_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        output_tokens: completion_tokens,
        server_tool_use: value
            .get("server_tool_use")
            .filter(|value| !value.is_null())
            .cloned(),
    })
}

fn estimate_tokens_from_text(text: &str) -> u32 {
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

fn append_block_text(text: &mut String, block: &ContentBlock) {
    match block.kind.as_str() {
        "text" | "input_text" => {
            if let Some(value) = text_like_block_text(block) {
                text.push_str(&value);
            }
        }
        "document" => {
            if let Some(value) = document_text(block) {
                text.push_str(&value);
            }
        }
        "tool_use" => {
            if let Some(name) = &block.name {
                text.push_str(name);
            }
            if let Some(input) = &block.input {
                text.push_str(&input.to_string());
            }
        }
        "tool_result" => text.push_str(&tool_result_text(block.content.as_ref())),
        "thinking" => {
            if let Some(value) = &block.thinking {
                text.push_str(value);
            }
        }
        _ => {}
    }
}

fn empty_choice() -> crate::openai::response::ChatChoice {
    crate::openai::response::ChatChoice {
        index: 0,
        message: crate::openai::response::AssistantMessage {
            role: "assistant",
            content: String::new(),
            tool_calls: None,
            images: None,
        },
        finish_reason: "stop".to_owned(),
    }
}

#[cfg(test)]
mod tests;
