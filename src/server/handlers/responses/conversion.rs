use crate::{
    anthropic::{
        ContentBlock, Message, MessageContent, MessagesRequest, SystemPrompt, ToolResultContent,
        normalize_anthropic_effort, to_openai_request,
    },
    error::Result,
    openai::types::{
        ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool, FunctionCall,
        ImageGenerationRequest, ImageUrl, ResponseInput, ResponseInputContent, ResponseInputItem,
        ResponseMessageInputItem, ResponsesRequest, ToolCall,
    },
    server::AppState,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Map;
use serde_json::{Value, json};

pub(in crate::server::handlers) async fn load_previous_response(
    state: &AppState,
    response_id: Option<&str>,
) -> Result<Option<crate::server::store::StoredResponse>> {
    match response_id {
        Some(id) => state
            .responses
            .get(id)
            .await
            .ok_or_else(|| crate::Error::config(format!("response `{id}` was not found")))
            .map(Some),
        None => Ok(None),
    }
}

pub(in crate::server::handlers) fn responses_to_chat_request(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> Result<(ChatCompletionRequest, Vec<Value>)> {
    let mut messages = previous.map(stored_response_messages).unwrap_or_default();
    let input_items = collect_response_input_items(request, previous)?;
    messages.extend(response_input_items_to_chat_messages(&input_items));
    maybe_prepend_instructions(&mut messages, request.instructions.as_deref());

    Ok((
        ChatCompletionRequest {
            model: request.model.clone(),
            messages,
            stream: request.stream,
            temperature: request.temperature,
            top_p: request.top_p,
            tools: request.tools.clone(),
            tool_choice: request.tool_choice.clone(),
            service_tier: request.service_tier.clone(),
            reasoning_effort: request
                .reasoning
                .as_ref()
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            max_completion_tokens: request.max_output_tokens,
            max_tokens: request.max_output_tokens,
            parallel_tool_calls: request.parallel_tool_calls,
            stop: None,
            extra: request.extra.clone(),
        },
        input_items,
    ))
}

pub(in crate::server::handlers) fn response_request_requires_raw_mode(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_image_generation_tool))
        || previous_stores_generated_images(previous)
        || response_input_requires_raw_mode(request.input.as_ref())
        || previous_stores_raw_response_items(previous)
}

fn previous_stores_generated_images(
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    previous.is_some_and(|stored| {
        stored
            .input_items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("image_generation_call"))
    })
}

fn response_input_requires_raw_mode(input: Option<&ResponseInput>) -> bool {
    match input {
        Some(ResponseInput::Items(items)) => items.iter().any(response_item_requires_raw_mode),
        Some(ResponseInput::Text(_)) | None => false,
    }
}

const fn response_item_requires_raw_mode(item: &ResponseInputItem) -> bool {
    matches!(
        item,
        ResponseInputItem::FunctionCall(_)
            | ResponseInputItem::FunctionCallOutput(_)
            | ResponseInputItem::Reasoning(_)
            | ResponseInputItem::Raw(_)
    )
}

fn previous_stores_raw_response_items(
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    previous.is_some_and(|stored| {
        stored
            .input_items
            .iter()
            .any(is_native_response_replay_item)
    })
}

fn is_native_response_replay_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "function_call_output" | "reasoning")
    )
}

fn is_image_generation_tool(tool: &ChatTool) -> bool {
    tool.kind == "image_generation"
}

pub(in crate::server::handlers) fn anthropic_responses_request(
    request: &MessagesRequest,
) -> Result<ResponsesRequest> {
    let openai = to_openai_request(request)?;
    let raw_input_items = anthropic_raw_input_items(request);
    let mut extra = request.extra.clone();
    extra.remove("rotom_anthropic_beta");
    extra.remove("rotom_anthropic_version");
    if let Some(stop_sequences) = request
        .stop_sequences
        .as_ref()
        .filter(|items| !items.is_empty())
    {
        extra.insert(
            "stop".to_owned(),
            Value::Array(stop_sequences.iter().cloned().map(Value::String).collect()),
        );
    }
    extra.insert(
        "rotom_raw_input_items".to_owned(),
        Value::Array(raw_input_items),
    );

    Ok(ResponsesRequest {
        model: openai.model,
        input: None,
        instructions: Some(String::new()),
        stream: openai.stream,
        temperature: openai.temperature,
        top_p: openai.top_p,
        tools: openai.tools,
        tool_choice: openai.tool_choice,
        service_tier: request.upstream_service_tier(),
        reasoning: anthropic_reasoning(request),
        max_output_tokens: request.max_tokens,
        parallel_tool_calls: openai.parallel_tool_calls,
        store: Some(false),
        previous_response_id: None,
        metadata: None,
        extra,
    })
}

pub(in crate::server::handlers) fn image_generation_responses_request(
    request: &ImageGenerationRequest,
) -> ResponsesRequest {
    let mut tool = ChatTool {
        kind: "image_generation".to_owned(),
        function: None,
        extra: serde_json::Map::new(),
    };
    if let Some(size) = &request.size {
        tool.extra
            .insert("size".to_owned(), Value::String(size.clone()));
    }
    if let Some(quality) = &request.quality {
        tool.extra
            .insert("quality".to_owned(), Value::String(quality.clone()));
    }
    if let Some(background) = &request.background {
        tool.extra
            .insert("background".to_owned(), Value::String(background.clone()));
    }
    if let Some(output_format) = &request.output_format {
        tool.extra.insert(
            "output_format".to_owned(),
            Value::String(output_format.clone()),
        );
    }
    if let Some(n) = request.n {
        tool.extra.insert("n".to_owned(), Value::from(n));
    }

    ResponsesRequest {
        model: request.model.clone(),
        input: Some(ResponseInput::Text(request.prompt.clone())),
        instructions: None,
        stream: Some(false),
        temperature: None,
        top_p: None,
        tools: Some(vec![tool]),
        tool_choice: Some(json!({"type": "image_generation"})),
        service_tier: None,
        reasoning: None,
        max_output_tokens: None,
        parallel_tool_calls: Some(false),
        store: Some(false),
        previous_response_id: None,
        metadata: None,
        extra: request.extra.clone(),
    }
}

fn maybe_prepend_instructions(messages: &mut Vec<ChatMessage>, instructions: Option<&str>) {
    if let Some(instructions) = instructions {
        messages.insert(
            0,
            ChatMessage {
                role: "system".to_owned(),
                content: Some(ChatContent::Text(instructions.to_owned())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
    }
}

pub(in crate::server::handlers) fn collect_response_input_items(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> Result<Vec<Value>> {
    let mut input_items = previous
        .map(|stored| stored.input_items.clone())
        .unwrap_or_default();

    if let Some(raw_items) = request
        .extra
        .get("rotom_raw_input_items")
        .and_then(Value::as_array)
    {
        input_items.extend(raw_items.iter().cloned());
        return Ok(input_items);
    }

    match request.input.as_ref() {
        Some(ResponseInput::Text(text)) => {
            input_items.push(serde_json::to_value(ResponseInputItem::Message(
                ResponseMessageInputItem {
                    kind: Some("message".to_owned()),
                    role: "user".to_owned(),
                    content: ResponseInputContent::Parts(vec![json_text_input_part(text)]),
                    id: None,
                    name: None,
                    tool_call_id: None,
                    status: None,
                    phase: None,
                    extra: Map::new(),
                },
            ))?);
        }
        Some(ResponseInput::Items(items)) => {
            for item in items {
                input_items.push(serde_json::to_value(item)?);
            }
        }
        None => {}
    }

    Ok(input_items)
}

fn response_input_items_to_chat_messages(input_items: &[Value]) -> Vec<ChatMessage> {
    input_items
        .iter()
        .filter_map(|item| serde_json::from_value::<ResponseInputItem>(item.clone()).ok())
        .filter_map(|item| response_input_item_to_chat_message(&item))
        .collect()
}

fn response_input_item_to_chat_message(item: &ResponseInputItem) -> Option<ChatMessage> {
    match item {
        ResponseInputItem::Message(message) => Some(ChatMessage {
            role: message.role.clone(),
            content: Some(response_input_content_to_chat(&message.content)),
            name: message.name.clone(),
            tool_call_id: message.tool_call_id.clone(),
            tool_calls: None,
        }),
        ResponseInputItem::FunctionCall(call) => Some(ChatMessage {
            role: "assistant".to_owned(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: call.call_id.clone(),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name: call.name.clone(),
                    arguments: response_arguments_to_string(&call.arguments),
                },
            }]),
        }),
        ResponseInputItem::FunctionCallOutput(output) => Some(ChatMessage {
            role: "tool".to_owned(),
            content: Some(ChatContent::Text(response_output_to_string(&output.output))),
            name: None,
            tool_call_id: Some(output.call_id.clone()),
            tool_calls: None,
        }),
        ResponseInputItem::Reasoning(_) | ResponseInputItem::Raw(_) => None,
        ResponseInputItem::Compaction(compaction) => {
            decode_compaction_summary(&compaction.encrypted_content).map(|summary| ChatMessage {
                role: "developer".to_owned(),
                content: Some(ChatContent::Text(summary)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
        }
    }
}

fn response_arguments_to_string(arguments: &Value) -> String {
    match arguments {
        Value::String(value) if !value.trim().is_empty() => value.clone(),
        Value::Null => "{}".to_owned(),
        other => other.to_string(),
    }
}

fn response_output_to_string(output: &Value) -> String {
    match output {
        Value::String(value) => value.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn response_input_content_to_chat(content: &ResponseInputContent) -> ChatContent {
    match content {
        ResponseInputContent::Text(text) => ChatContent::Text(text.clone()),
        ResponseInputContent::Parts(parts) => ChatContent::Parts(
            parts
                .iter()
                .filter_map(|part| match part.kind.as_str() {
                    "text" | "input_text" | "output_text" => Some(ChatContentPart {
                        kind: "text".to_owned(),
                        text: part.text.clone(),
                        image_url: None,
                    }),
                    "input_image" | "image_url" => {
                        part.image_url.as_ref().map(|image_url| ChatContentPart {
                            kind: "image_url".to_owned(),
                            text: None,
                            image_url: Some(ImageUrl {
                                url: image_url.clone(),
                                detail: part.detail.clone(),
                            }),
                        })
                    }
                    _ => None,
                })
                .collect(),
        ),
    }
}

fn stored_response_messages(stored: &crate::server::store::StoredResponse) -> Vec<ChatMessage> {
    response_input_items_to_chat_messages(&stored.input_items)
}

pub(in crate::server::handlers) fn estimate_response_input_tokens(
    request: &ResponsesRequest,
    input_items: &[Value],
) -> u32 {
    let mut text = String::new();
    if let Some(instructions) = request.instructions.as_deref() {
        text.push_str(instructions);
    }
    if let Some(reasoning) = request.reasoning.as_ref() {
        text.push_str(&reasoning.to_string());
    }
    if let Some(tools) = request.tools.as_ref() {
        for tool in tools {
            if let Ok(value) = serde_json::to_string(tool) {
                text.push_str(&value);
            }
        }
    }
    for item in input_items {
        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            if let Some(content) = item.get("encrypted_content").and_then(Value::as_str) {
                if let Some(summary) = decode_compaction_summary(content) {
                    text.push_str(&summary);
                } else {
                    text.push_str(content);
                }
            }
        } else {
            if let Some(role) = item.get("role").and_then(Value::as_str) {
                text.push_str(role);
            }
            if let Some(content) = item.get("content") {
                match content {
                    Value::String(value) => text.push_str(value),
                    Value::Array(parts) => {
                        for part in parts {
                            if let Some(value) = part.get("text").and_then(Value::as_str) {
                                text.push_str(value);
                            }
                            if let Some(value) = part.get("image_url").and_then(Value::as_str) {
                                text.push_str("[image:");
                                text.push_str(value);
                                text.push(']');
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let estimated = text.chars().count().saturating_div(4).max(1);
    u32::try_from(estimated).unwrap_or(u32::MAX)
}

fn json_text_input_part(text: &str) -> crate::openai::types::ResponseInputContentPart {
    crate::openai::types::ResponseInputContentPart {
        kind: "input_text".to_owned(),
        text: Some(text.to_owned()),
        image_url: None,
        detail: None,
        extra: Map::new(),
    }
}

fn anthropic_reasoning(request: &MessagesRequest) -> Option<Value> {
    let effort = request
        .output_effort()
        .or_else(|| anthropic_thinking_effort(request.thinking.as_ref()))?;
    let summary = if anthropic_thinking_enabled(request.thinking.as_ref()) {
        "detailed"
    } else {
        "auto"
    };

    Some(json!({
        "effort": effort,
        "summary": summary
    }))
}

fn anthropic_thinking_enabled(thinking: Option<&Value>) -> bool {
    thinking.is_some_and(|thinking| {
        !thinking
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("disabled"))
    })
}

fn anthropic_thinking_effort(thinking: Option<&Value>) -> Option<String> {
    let thinking = thinking?;
    if thinking
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("disabled"))
    {
        return None;
    }
    if let Some(effort) = thinking
        .get("effort")
        .and_then(Value::as_str)
        .and_then(normalize_anthropic_effort)
    {
        return Some(effort);
    }

    let budget_tokens = thinking.get("budget_tokens").and_then(Value::as_u64);
    match budget_tokens {
        Some(tokens) if tokens >= 8_192 => Some("high".to_owned()),
        Some(tokens) if tokens >= 2_048 => Some("medium".to_owned()),
        Some(_) => Some("low".to_owned()),
        None if thinking
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("adaptive")) =>
        {
            Some("high".to_owned())
        }
        None => Some("medium".to_owned()),
    }
}

fn anthropic_raw_input_items(request: &MessagesRequest) -> Vec<Value> {
    let mut items = request
        .system
        .as_ref()
        .map_or_else(Vec::new, anthropic_system_items);
    for message in &request.messages {
        items.extend(anthropic_message_items(message));
    }
    items
}

fn anthropic_system_items(system: &SystemPrompt) -> Vec<Value> {
    let parts = match system {
        SystemPrompt::Text(text) => crate::anthropic::sanitize_system_text(text)
            .map(|text| vec![json!({"type": "input_text", "text": text})])
            .unwrap_or_default(),
        SystemPrompt::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .text
                    .as_deref()
                    .and_then(crate::anthropic::sanitize_system_text)
                    .map(|text| {
                        let mut value = json!({"type": "input_text", "text": text});
                        if let Some(cache_control) = &block.cache_control {
                            value["cache_control"] = cache_control.clone();
                        }
                        value
                    })
            })
            .collect(),
    };

    if parts.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "type": "message",
            "role": "developer",
            "content": parts
        })]
    }
}

fn anthropic_message_items(message: &Message) -> Vec<Value> {
    match (&message.role[..], &message.content) {
        ("user", MessageContent::Text(text)) => {
            let content = vec![json!({"type": "input_text", "text": text})];
            vec![raw_message_item("user", &content)]
        }
        ("assistant", MessageContent::Text(text)) => vec![json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
            "status": "completed"
        })],
        ("user", MessageContent::Blocks(blocks)) => anthropic_user_block_items(blocks),
        ("assistant", MessageContent::Blocks(blocks)) => anthropic_assistant_block_items(blocks),
        _ => Vec::new(),
    }
}

fn anthropic_user_block_items(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut items = Vec::new();
    let mut parts = Vec::new();
    for block in blocks {
        if block.kind == "tool_result" {
            flush_raw_message_item(&mut items, "user", &mut parts);
            // Responses has no is_error field, so carry the failure in tool-visible text.
            let output = anthropic_tool_result_text(block.content.as_ref());
            let output = if block.is_error == Some(true) {
                format!("Error: {output}")
            } else {
                output
            };
            let mut item = json!({
                "type": "function_call_output",
                "call_id": block.tool_use_id.clone().unwrap_or_default(),
                "output": output,
            });
            if let Some(cache_control) = &block.cache_control {
                item["cache_control"] = cache_control.clone();
            }
            items.push(item);
        } else if let Some(part) = anthropic_user_content_part(block) {
            parts.push(part);
        }
    }
    flush_raw_message_item(&mut items, "user", &mut parts);
    items
}

fn anthropic_assistant_block_items(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut items = Vec::new();
    let mut text_parts = Vec::new();
    for block in blocks {
        if block.kind == "tool_use" {
            flush_raw_assistant_text_item(&mut items, &mut text_parts);
            let mut item = json!({
                "type": "function_call",
                "call_id": block.id.clone().unwrap_or_default(),
                "name": block.name.clone().unwrap_or_default(),
                "arguments": block.input.clone().unwrap_or_else(|| json!({})).to_string(),
            });
            if let Some(cache_control) = &block.cache_control {
                item["cache_control"] = cache_control.clone();
            }
            items.push(item);
        } else if let Some(part) = anthropic_assistant_content_part(block) {
            text_parts.push(part);
        }
    }
    flush_raw_assistant_text_item(&mut items, &mut text_parts);
    items
}

fn anthropic_user_content_part(block: &ContentBlock) -> Option<Value> {
    match block.kind.as_str() {
        "text" | "input_text" => Some(cacheable_value(
            json!({"type": "input_text", "text": anthropic_text_like_block_text(block)?}),
            block.cache_control.as_ref(),
        )),
        "image" => anthropic_image_data_url(block.source.as_ref()).map(|image_url| {
            cacheable_value(
                json!({"type": "input_image", "image_url": image_url, "detail": "auto"}),
                block.cache_control.as_ref(),
            )
        }),
        "document" => Some(cacheable_value(
            json!({"type": "input_text", "text": anthropic_document_text(block)?}),
            block.cache_control.as_ref(),
        )),
        "thinking" => Some(cacheable_value(
            json!({"type": "input_text", "text": block.thinking.clone()?}),
            block.cache_control.as_ref(),
        )),
        _ => None,
    }
}

fn anthropic_assistant_content_part(block: &ContentBlock) -> Option<Value> {
    match block.kind.as_str() {
        "text" | "input_text" | "document" | "thinking" => {
            let text = match block.kind.as_str() {
                "document" => anthropic_document_text(block)?,
                "thinking" => block.thinking.clone()?,
                _ => anthropic_text_like_block_text(block)?,
            };
            Some(cacheable_value(
                json!({"type": "output_text", "text": text, "annotations": []}),
                block.cache_control.as_ref(),
            ))
        }
        _ => None,
    }
}

fn cacheable_value(mut value: Value, cache_control: Option<&Value>) -> Value {
    if let Some(cache_control) = cache_control {
        value["cache_control"] = cache_control.clone();
    }
    value
}

fn raw_message_item(role: &str, content: &[Value]) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": content
    })
}

fn flush_raw_message_item(items: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    let content = std::mem::take(parts);
    items.push(raw_message_item(role, &content));
}

fn flush_raw_assistant_text_item(items: &mut Vec<Value>, parts: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    items.push(json!({
        "type": "message",
        "role": "assistant",
        "content": std::mem::take(parts),
        "status": "completed"
    }));
}

fn anthropic_text_like_block_text(block: &ContentBlock) -> Option<String> {
    block
        .text
        .clone()
        .or_else(|| block.thinking.clone())
        .or_else(|| block.source.as_ref().and_then(|source| source.text.clone()))
}

fn anthropic_image_data_url(source: Option<&crate::anthropic::ImageSource>) -> Option<String> {
    let source = source?;
    let media_type = source.media_type.as_deref().unwrap_or("image/png");
    let data = source.data.as_deref()?;
    Some(format!("data:{media_type};base64,{data}"))
}

fn anthropic_document_text(block: &ContentBlock) -> Option<String> {
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
            if let Some(text) = anthropic_text_like_block_text(nested) {
                fragments.push(text);
            }
        }
    }
    let combined = fragments.join("\n");
    (!combined.is_empty()).then_some(combined)
}

fn anthropic_tool_result_text(content: Option<&ToolResultContent>) -> String {
    match content {
        Some(ToolResultContent::Text(text)) => text.clone(),
        Some(ToolResultContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(anthropic_text_like_block_text)
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

pub(in crate::server::handlers) fn compact_response_items(input_items: &[Value]) -> Vec<Value> {
    let mut output = input_items
        .iter()
        .filter(|item| is_compactable_message(item))
        .cloned()
        .collect::<Vec<_>>();
    output.push(json!({
        "type": "compaction",
        "encrypted_content": local_compaction_payload(&output),
    }));
    output
}

fn is_compactable_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && matches!(
            item.get("role").and_then(Value::as_str),
            Some("user" | "developer")
        )
}

fn local_compaction_payload(items: &[Value]) -> String {
    let summary = items
        .iter()
        .filter_map(compaction_text_for_item)
        .collect::<Vec<_>>()
        .join("\n");
    let summary = truncate_summary(&summary, 1_024);
    let payload = json!({
        "provider": "rotom",
        "version": 1,
        "summary": summary,
    });
    STANDARD.encode(payload.to_string())
}

fn compaction_text_for_item(item: &Value) -> Option<String> {
    let role = item.get("role").and_then(Value::as_str)?;
    let content = item.get("content")?;
    let text = match content {
        Value::String(value) => value.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    Some(format!("{role}: {text}"))
}

fn truncate_summary(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_owned();
    }

    text.chars().take(max_len).collect()
}

fn decode_compaction_summary(payload: &str) -> Option<String> {
    let decoded = STANDARD.decode(payload).ok()?;
    let value = serde_json::from_slice::<Value>(&decoded).ok()?;
    value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn anthropic_messages_map_system_to_instructions_and_thinking_to_reasoning() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "system": "be terse",
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "stop_sequences": ["DONE"],
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let converted = anthropic_responses_request(&request).unwrap();

        assert_eq!(converted.instructions.as_deref(), Some(""));
        assert_eq!(
            converted
                .reasoning
                .as_ref()
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str),
            Some("medium")
        );
        let items = converted
            .extra
            .get("rotom_raw_input_items")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["role"], "developer");
        assert_eq!(converted.extra["stop"], json!(["DONE"]));
    }

    #[test]
    fn anthropic_messages_map_output_config_effort_and_fast_speed() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "output_config": {"effort": "max"},
            "service_tier": "standard_only",
            "speed": "fast",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let converted = anthropic_responses_request(&request).unwrap();

        assert_eq!(converted.service_tier.as_deref(), Some("priority"));
        assert_eq!(
            converted
                .reasoning
                .as_ref()
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str),
            Some("xhigh")
        );
        assert_eq!(
            converted
                .reasoning
                .as_ref()
                .and_then(|value| value.get("summary"))
                .and_then(Value::as_str),
            Some("auto")
        );
        assert!(!converted.extra.contains_key("output_config"));
        assert!(!converted.extra.contains_key("speed"));
    }

    #[test]
    fn anthropic_messages_map_service_tier_auto_to_codex_priority() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "service_tier": "auto",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let converted = anthropic_responses_request(&request).unwrap();

        assert_eq!(converted.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn anthropic_messages_skip_standard_service_tier_and_disabled_thinking() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "service_tier": "standard_only",
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let converted = anthropic_responses_request(&request).unwrap();

        assert!(converted.service_tier.is_none());
        assert!(converted.reasoning.is_none());
    }

    #[test]
    fn anthropic_messages_preserve_assistant_block_order_in_raw_items() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"q": "x"}},
                    {"type": "text", "text": "after"}
                ]
            }]
        }))
        .unwrap();

        let converted = anthropic_responses_request(&request).unwrap();
        let items = converted
            .extra
            .get("rotom_raw_input_items")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "before");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[2]["type"], "message");
        assert_eq!(items[2]["content"][0]["text"], "after");
    }

    #[test]
    fn responses_function_history_requires_raw_mode() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {"role": "user", "content": "hi"},
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{\"q\":\"x\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "done"
                }
            ]
        }))
        .unwrap();

        assert!(response_request_requires_raw_mode(&request, None));
        let items = collect_response_input_items(&request, None).unwrap();
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[2]["type"], "function_call_output");
    }
}

#[cfg(test)]
mod tool_flow_tests;
