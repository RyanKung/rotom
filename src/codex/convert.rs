use super::responses_input;
use crate::{
    Error, Result,
    config::Provider,
    openai::types::{
        ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool,
        ResponsesRequest,
    },
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const MAX_CACHE_KEY_LEN: usize = 64;

/// Normalizes model IDs by removing the `openai-codex/` prefix when present.
#[must_use]
pub fn normalize_model(model: &str) -> String {
    model
        .strip_prefix("openai-codex/")
        .unwrap_or(model)
        .to_owned()
}

/// Converts a chat completions request into the JSON body expected by Codex.
///
/// # Errors
///
/// Returns an error when a function tool omits its function metadata.
pub fn to_codex_request(request: &ChatCompletionRequest) -> Result<Value> {
    let (instructions, input) = split_messages(&request.messages);
    let cache_key = resolve_prompt_cache_key(&request.extra, None, None, &instructions, &input);
    let mut body = json!({
        "model": normalize_model(&request.model),
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "text": text_config(request),
        "include": ["reasoning.encrypted_content"],
        "tool_choice": convert_tool_choice(request.tool_choice.as_ref()).unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": request.parallel_tool_calls.unwrap_or(true)
    });
    insert_optional(&mut body, "prompt_cache_key", cache_key.map(Value::String));

    insert_optional(
        &mut body,
        "service_tier",
        request.service_tier.clone().map(Value::from),
    );
    insert_optional(
        &mut body,
        "stop",
        request
            .stop
            .as_ref()
            .filter(|stop| !stop.is_empty())
            .cloned()
            .map(|stop| json!(stop)),
    );
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body["tools"] = Value::Array(tools.iter().map(convert_tool).collect::<Result<Vec<_>>>()?);
    }

    if let Some(effort) = request.reasoning_effort.as_deref() {
        body["reasoning"] = json!({
            "effort": clamp_reasoning_effort(&request.model, effort),
            "summary": "auto"
        });
    }

    let inferred_provider = crate::models::provider_for_model(&request.model);
    if inferred_provider == Some(Provider::Gemini) {
        insert_optional(
            &mut body,
            "temperature",
            request.temperature.map(Value::from),
        );
        insert_optional(&mut body, "top_p", request.top_p.map(Value::from));
        insert_optional(
            &mut body,
            "max_output_tokens",
            request
                .max_completion_tokens
                .or(request.max_tokens)
                .map(Value::from),
        );
    }
    if inferred_provider == Some(Provider::Vercel) {
        insert_optional(
            &mut body,
            "prompt_cache_retention",
            request.extra.get("prompt_cache_retention").cloned(),
        );
    }

    strip_unsupported_keys(&mut body);

    Ok(body)
}

fn split_messages(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    messages
        .iter()
        .enumerate()
        .fold((Vec::new(), Vec::new()), |mut acc, (index, message)| {
            match message.role.as_str() {
                "system" | "developer" => {
                    if acc.1.is_empty() {
                        if let Some(text) = message_text(message) {
                            acc.0.push(text);
                        }
                    } else {
                        acc.1.push(json!({
                            "role": "developer",
                            "content": content_to_input_parts(message.content.as_ref())
                        }));
                    }
                }
                "user" => acc.1.push(json!({
                    "role": "user",
                    "content": content_to_input_parts(message.content.as_ref())
                })),
                "assistant" => append_assistant_message(&mut acc.1, message, index),
                "tool" => {
                    if let Some(call_id) = message.tool_call_id.as_deref() {
                        acc.1.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": message_text(message).unwrap_or_default()
                        }));
                    }
                }
                _ => {}
            }
            acc
        })
        .map_first(|parts| parts.join("\n\n"))
}

fn append_assistant_message(input: &mut Vec<Value>, message: &ChatMessage, index: usize) {
    if let Some(text) = message_text(message).filter(|text| !text.is_empty()) {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
            "status": "completed",
            "id": format!("msg_{index}")
        }));
    }

    for tool_call in message.tool_calls.iter().flatten() {
        input.push(json!({
            "type": "function_call",
            "call_id": &tool_call.id,
            "name": &tool_call.function.name,
            "arguments": &tool_call.function.arguments
        }));
    }
}

fn content_to_input_parts(content: Option<&ChatContent>) -> Vec<Value> {
    match content {
        Some(ChatContent::Text(text)) => vec![json!({ "type": "input_text", "text": text })],
        Some(ChatContent::Parts(parts)) => parts.iter().filter_map(convert_content_part).collect(),
        None => Vec::new(),
    }
}

fn convert_content_part(part: &ChatContentPart) -> Option<Value> {
    match part.kind.as_str() {
        "text" => part
            .text
            .as_ref()
            .map(|text| json!({ "type": "input_text", "text": text })),
        "image_url" => part.image_url.as_ref().map(|image| {
            json!({
                "type": "input_image",
                "detail": image.detail.as_deref().unwrap_or("auto"),
                "image_url": image.url
            })
        }),
        _ => None,
    }
}

/// Converts one compatibility-layer tool definition into the upstream Codex shape.
///
/// # Errors
///
/// Returns an error when a function tool omits its function metadata.
pub fn convert_tool(tool: &ChatTool) -> Result<Value> {
    if tool.kind == "function" {
        let mut extra = tool.extra.clone();
        let name = tool
            .function
            .as_ref()
            .map(|function| function.name.clone())
            .or_else(|| {
                extra
                    .remove("name")
                    .and_then(|value| value.as_str().map(str::to_owned))
            })
            .ok_or_else(|| Error::config("function tool is missing function metadata"))?;
        let description = tool
            .function
            .as_ref()
            .and_then(|function| function.description.clone())
            .or_else(|| {
                extra
                    .remove("description")
                    .and_then(|value| value.as_str().map(str::to_owned))
            })
            .unwrap_or_default();
        let parameters = tool
            .function
            .as_ref()
            .and_then(|function| function.parameters.clone())
            .or_else(|| extra.remove("parameters"))
            .unwrap_or_else(|| json!({ "type": "object" }));
        let strict = tool
            .function
            .as_ref()
            .and_then(|function| function.strict)
            .or_else(|| extra.remove("strict").and_then(|value| value.as_bool()));

        for key in ["name", "description", "parameters", "strict"] {
            extra.remove(key);
        }

        let mut value = json!({
            "type": "function",
            "name": name,
            "description": description,
            "parameters": parameters,
            "strict": strict
        });
        if let Some(object) = value.as_object_mut() {
            object.extend(extra);
        }
        Ok(value)
    } else {
        let mut value = Value::Object(tool.extra.clone());
        value["type"] = Value::String(tool.kind.clone());
        Ok(value)
    }
}

/// Normalizes compatibility-layer tool choice values into the upstream Codex shape.
pub fn convert_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
    let choice = tool_choice?;
    if choice.is_string() {
        return Some(choice.clone());
    }

    let kind = choice.get("type").and_then(Value::as_str)?;
    if kind != "function" {
        return Some(choice.clone());
    }

    let name = choice
        .get("name")
        .or_else(|| choice.pointer("/function/name"))?
        .as_str()?;

    Some(json!({
        "type": "function",
        "name": name
    }))
}

/// Converts a Responses request plus normalized input items into the Codex body.
///
/// # Errors
///
/// Returns an error when a function tool omits its function metadata.
pub fn responses_to_codex_request(request: &ResponsesRequest, input: &[Value]) -> Result<Value> {
    responses_to_upstream_request(Provider::Codex, request, input)
}

/// Converts a Responses request plus normalized input items into a provider request body.
///
/// # Errors
///
/// Returns an error when a function tool omits its function metadata.
pub fn responses_to_upstream_request(
    provider: Provider,
    request: &ResponsesRequest,
    input: &[Value],
) -> Result<Value> {
    let normalized = responses_input::normalize(input);
    let input = normalized.items;
    let instructions = responses_input::combined_instructions(
        request.instructions.as_deref(),
        &normalized.instructions,
    );
    let cache_key = resolve_prompt_cache_key(
        &request.extra,
        request.metadata.as_ref(),
        request.previous_response_id.as_deref(),
        &instructions,
        &input,
    );
    let stream = upstream_stream_value(provider, request);
    let store = upstream_store_value(provider, request, &input);
    let text = request.extra.get("text").cloned().unwrap_or_else(|| {
        json!({ "verbosity": request.extra.get("text_verbosity").and_then(Value::as_str).unwrap_or("medium") })
    });
    let include = request
        .extra
        .get("include")
        .cloned()
        .unwrap_or_else(|| json!(["reasoning.encrypted_content"]));
    let mut body = json!({
        "model": normalize_model(&request.model),
        "store": store,
        "stream": stream,
        "input": input,
        "text": text,
        "include": include,
        "tool_choice": convert_tool_choice(request.tool_choice.as_ref()).unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": request.parallel_tool_calls()
    });
    if should_include_instructions(provider, request, &instructions) {
        body["instructions"] = Value::String(instructions);
    }
    insert_optional(&mut body, "prompt_cache_key", cache_key.map(Value::String));

    insert_optional(
        &mut body,
        "temperature",
        request.temperature.map(Value::from),
    );
    insert_optional(&mut body, "top_p", request.top_p.map(Value::from));
    insert_optional(
        &mut body,
        "max_output_tokens",
        request.max_output_tokens.map(Value::from),
    );
    insert_optional(&mut body, "stop", request.extra.get("stop").cloned());
    insert_optional(
        &mut body,
        "service_tier",
        request.service_tier.clone().map(Value::from),
    );
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body["tools"] = Value::Array(tools.iter().map(convert_tool).collect::<Result<Vec<_>>>()?);
    }
    if let Some(reasoning) = reasoning_for(provider, &request.model, request.reasoning.as_ref()) {
        body["reasoning"] = reasoning;
    }
    if provider == Provider::Grok {
        insert_optional(
            &mut body,
            "previous_response_id",
            request.previous_response_id.clone().map(Value::from),
        );
    }
    if provider == Provider::Vercel {
        insert_optional(
            &mut body,
            "prompt_cache_retention",
            request.extra.get("prompt_cache_retention").cloned(),
        );
    }

    strip_unsupported_keys(&mut body);

    Ok(body)
}

/// Resolves or deterministically derives a conversation-stable cache/session key
/// from a raw JSON request body.
#[must_use]
pub fn cache_key_from_request_value(body: &Value) -> Option<String> {
    let object = body.as_object()?;
    let metadata = object.get("metadata").and_then(Value::as_object);
    let previous_response_id = object.get("previous_response_id").and_then(Value::as_str);
    let instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = match object.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })],
        _ => Vec::new(),
    };
    resolve_prompt_cache_key(object, metadata, previous_response_id, instructions, &input)
}

/// Resolves a conversation-stable prompt cache key from explicit request metadata
/// or deterministically hashes the conversation's opening context.
#[must_use]
pub fn resolve_prompt_cache_key(
    extra: &Map<String, Value>,
    metadata: Option<&Map<String, Value>>,
    previous_response_id: Option<&str>,
    instructions: &str,
    input: &[Value],
) -> Option<String> {
    if let Some(explicit) = explicit_cache_key(extra, metadata) {
        let key = normalize_cache_key(explicit);
        record_continuity_cache_key(instructions, input, &key);
        return Some(key);
    }
    if let Some(derived) = continuity_or_deterministic_cache_key(instructions, input) {
        return Some(derived);
    }
    previous_response_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_cache_key)
}

fn explicit_cache_key<'a>(
    extra: &'a Map<String, Value>,
    metadata: Option<&'a Map<String, Value>>,
) -> Option<&'a str> {
    for key in [
        "rotom_session_id",
        "prompt_cache_key",
        "conversation_id",
        "session_id",
    ] {
        if let Some(value) = non_empty_str(extra.get(key)) {
            return Some(value);
        }
    }

    let meta = metadata.or_else(|| extra.get("metadata").and_then(Value::as_object));
    if let Some(meta) = meta {
        for key in ["session_id", "conversation_id", "prompt_cache_key"] {
            if let Some(value) = non_empty_str(meta.get(key)) {
                return Some(value);
            }
        }
        if let Some(user_id) = non_empty_str(meta.get("user_id")) {
            if let Some((_, session)) = user_id.rsplit_once("_session_") {
                let session = session.trim();
                if !session.is_empty() {
                    return Some(session);
                }
            }
            return Some(user_id);
        }
    }

    non_empty_str(extra.get("user"))
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn normalize_cache_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if (1..=MAX_CACHE_KEY_LEN).contains(&trimmed.len())
        && trimmed.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        trimmed.to_owned()
    } else {
        uuid_from_seed(trimmed.as_bytes())
    }
}

const MAX_CONTINUITY_ENTRIES: usize = 4096;
const INSTRUCTION_SEED_PREFIX_CHARS: usize = 512;

struct ContinuityStore {
    order: std::collections::VecDeque<u64>,
    map: std::collections::HashMap<u64, String>,
}

impl ContinuityStore {
    fn new() -> Self {
        Self {
            order: std::collections::VecDeque::new(),
            map: std::collections::HashMap::new(),
        }
    }

    fn get(&self, fingerprint: u64) -> Option<String> {
        self.map.get(&fingerprint).cloned()
    }

    fn insert(&mut self, fingerprint: u64, key: &str) {
        if let Some(existing) = self.map.get_mut(&fingerprint) {
            existing.clear();
            existing.push_str(key);
            return;
        }
        if self.order.len() >= MAX_CONTINUITY_ENTRIES
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        self.order.push_back(fingerprint);
        self.map.insert(fingerprint, key.to_owned());
    }
}

fn continuity_store() -> &'static std::sync::Mutex<ContinuityStore> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<ContinuityStore>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(ContinuityStore::new()))
}

fn continuity_or_deterministic_cache_key(instructions: &str, input: &[Value]) -> Option<String> {
    let seed_prefix = instruction_seed_prefix(instructions, input);
    let dialogue_hashes = dialogue_item_hashes(input);
    let opening_fp = dialogue_hashes
        .first()
        .map(|first| opening_turn_fingerprint(&seed_prefix, *first));

    if let Ok(store) = continuity_store().lock() {
        if let Some(fp) = opening_fp
            && let Some(existing) = store.get(fp)
        {
            drop(store);
            record_fingerprints(&seed_prefix, opening_fp, &dialogue_hashes, &existing);
            return Some(existing);
        }
        if dialogue_hashes.len() >= 2 {
            for window in dialogue_hashes.windows(2) {
                let bigram = pair_fingerprint(&seed_prefix, window[0], window[1]);
                if let Some(existing) = store.get(bigram) {
                    drop(store);
                    record_fingerprints(&seed_prefix, opening_fp, &dialogue_hashes, &existing);
                    return Some(existing);
                }
            }
        }
    }

    let derived = deterministic_conversation_cache_key(instructions, input)?;
    record_fingerprints(&seed_prefix, opening_fp, &dialogue_hashes, &derived);
    Some(derived)
}

fn record_continuity_cache_key(instructions: &str, input: &[Value], key: &str) {
    let seed_prefix = instruction_seed_prefix(instructions, input);
    let dialogue_hashes = dialogue_item_hashes(input);
    let opening_fp = dialogue_hashes
        .first()
        .map(|first| opening_turn_fingerprint(&seed_prefix, *first));
    record_fingerprints(&seed_prefix, opening_fp, &dialogue_hashes, key);
}

fn record_fingerprints(
    seed_prefix: &str,
    opening_fp: Option<u64>,
    dialogue_hashes: &[u64],
    key: &str,
) {
    let Ok(mut store) = continuity_store().lock() else {
        return;
    };
    if let Some(fp) = opening_fp {
        store.insert(fp, key);
    }
    for window in dialogue_hashes.windows(2) {
        store.insert(pair_fingerprint(seed_prefix, window[0], window[1]), key);
    }
}

fn dialogue_item_hashes(input: &[Value]) -> Vec<u64> {
    input
        .iter()
        .filter(|item| {
            matches!(
                item.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .map(|item| {
            let mut clean = item.clone();
            strip_unsupported_keys(&mut clean);
            if let Some(object) = clean.as_object_mut() {
                object.remove("id");
                object.remove("status");
            }
            hash_u64(clean.to_string().as_bytes())
        })
        .collect()
}

fn opening_turn_fingerprint(seed_prefix: &str, first_dialogue_hash: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"opening:");
    hasher.update(seed_prefix.as_bytes());
    hasher.update(b":");
    hasher.update(first_dialogue_hash.to_be_bytes());
    digest_u64(&hasher.finalize())
}

fn pair_fingerprint(seed_prefix: &str, first: u64, second: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"pair:");
    hasher.update(seed_prefix.as_bytes());
    hasher.update(b":");
    hasher.update(first.to_be_bytes());
    hasher.update(second.to_be_bytes());
    digest_u64(&hasher.finalize())
}

fn hash_u64(bytes: &[u8]) -> u64 {
    digest_u64(&Sha256::digest(bytes))
}

fn digest_u64(digest: &[u8]) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

fn instruction_seed_prefix(instructions: &str, input: &[Value]) -> String {
    let trimmed = instructions.trim();
    if !trimmed.is_empty() {
        return trimmed
            .chars()
            .take(INSTRUCTION_SEED_PREFIX_CHARS)
            .collect();
    }
    if let Some(first_dev) = input.first().filter(|item| {
        matches!(
            item.get("role").and_then(Value::as_str),
            Some("developer" | "system")
        )
    }) {
        let mut clean = first_dev.clone();
        strip_unsupported_keys(&mut clean);
        return clean
            .to_string()
            .chars()
            .take(INSTRUCTION_SEED_PREFIX_CHARS)
            .collect();
    }
    String::new()
}

fn deterministic_conversation_cache_key(instructions: &str, input: &[Value]) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut has_seed = false;

    let trimmed_instructions = instructions.trim();
    if !trimmed_instructions.is_empty() {
        let prefix: String = trimmed_instructions
            .chars()
            .take(INSTRUCTION_SEED_PREFIX_CHARS)
            .collect();
        hasher.update(b"instructions:");
        hasher.update(prefix.as_bytes());
        hasher.update(b"\n");
        has_seed = true;
    } else if let Some(first_dev) = input.first().filter(|item| {
        matches!(
            item.get("role").and_then(Value::as_str),
            Some("developer" | "system")
        )
    }) {
        let mut clean = first_dev.clone();
        strip_unsupported_keys(&mut clean);
        let prefix: String = clean
            .to_string()
            .chars()
            .take(INSTRUCTION_SEED_PREFIX_CHARS)
            .collect();
        hasher.update(b"dev:");
        hasher.update(prefix.as_bytes());
        hasher.update(b"\n");
        has_seed = true;
    }

    if let Some(first_dialogue) = input
        .iter()
        .find(|item| {
            matches!(
                item.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .or_else(|| input.first())
    {
        let mut clean = first_dialogue.clone();
        strip_unsupported_keys(&mut clean);
        if let Some(object) = clean.as_object_mut() {
            object.remove("id");
            object.remove("status");
        }
        hasher.update(b"first:");
        hasher.update(clean.to_string().as_bytes());
        has_seed = true;
    }

    has_seed.then(|| format_digest_as_uuid(&hasher.finalize()))
}

fn uuid_from_seed(seed: &[u8]) -> String {
    let digest = Sha256::digest(seed);
    format_digest_as_uuid(&digest)
}

fn format_digest_as_uuid(digest: &[u8]) -> String {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Set RFC 4122 version 4 and variant bits for strict UUID validators.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn upstream_stream_value(provider: Provider, request: &ResponsesRequest) -> bool {
    match provider {
        // Codex currently expects SSE upstream even when the downstream API
        // requested a one-shot JSON response.
        Provider::Codex => true,
        Provider::Grok | Provider::Kiro | Provider::Vercel | Provider::Gemini => {
            request.wants_stream()
        }
    }
}

fn upstream_store_value(provider: Provider, request: &ResponsesRequest, input: &[Value]) -> bool {
    match provider {
        Provider::Codex => request.should_store() && !input_requires_stateless_replay(input),
        Provider::Grok | Provider::Vercel => request.should_store(),
        Provider::Kiro | Provider::Gemini => false,
    }
}

const fn should_include_instructions(
    provider: Provider,
    request: &ResponsesRequest,
    instructions: &str,
) -> bool {
    match provider {
        Provider::Codex => true,
        Provider::Grok => !instructions.is_empty() && request.previous_response_id.is_none(),
        Provider::Kiro | Provider::Vercel | Provider::Gemini => !instructions.is_empty(),
    }
}

fn input_requires_stateless_replay(input: &[Value]) -> bool {
    input.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "function_call_output" | "reasoning" | "image_generation_call")
        )
    })
}

fn message_text(message: &ChatMessage) -> Option<String> {
    match message.content.as_ref()? {
        ChatContent::Text(text) => Some(text.clone()),
        ChatContent::Parts(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| match part.kind.as_str() {
                    "text" => part.text.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(text)
        }
    }
}

fn text_verbosity(request: &ChatCompletionRequest) -> String {
    request
        .extra
        .get("text_verbosity")
        .and_then(Value::as_str)
        .unwrap_or("medium")
        .to_owned()
}

/// Builds the Responses API `text` object: verbosity plus, when the chat
/// request carries a `response_format`, the equivalent `text.format`.
fn text_config(request: &ChatCompletionRequest) -> Value {
    let mut text = json!({ "verbosity": text_verbosity(request) });
    if let Some(format) = convert_response_format(request.extra.get("response_format")) {
        text["format"] = format;
    }
    text
}

/// Maps a Chat Completions `response_format` to a Responses API `text.format`.
///
/// - `{"type": "json_schema", "json_schema": {name, schema, strict?, description?}}`
///   becomes `{"type": "json_schema", name, schema, strict, description?}`.
/// - `{"type": "json_object"}` and `{"type": "text"}` pass through unchanged.
/// - Anything else is forwarded as-is so newer formats are not silently dropped.
pub fn convert_response_format(response_format: Option<&Value>) -> Option<Value> {
    let format = response_format?.as_object()?;
    let kind = format.get("type").and_then(Value::as_str)?;
    match kind {
        "json_schema" => {
            let spec = format.get("json_schema").and_then(Value::as_object);
            let mut out = json!({ "type": "json_schema" });
            if let Some(spec) = spec {
                for key in ["name", "schema", "strict", "description"] {
                    if let Some(value) = spec.get(key) {
                        out[key] = value.clone();
                    }
                }
            }
            if out.get("name").is_none() {
                out["name"] = Value::String("response".to_owned());
            }
            if out.get("strict").is_none() {
                out["strict"] = Value::Bool(true);
            }
            Some(out)
        }
        _ => Some(Value::Object(format.clone())),
    }
}

fn clamp_reasoning_effort(model: &str, effort: &str) -> String {
    let id = normalize_model(model);
    if (id.starts_with("gpt-5.2")
        || id.starts_with("gpt-5.3")
        || id.starts_with("gpt-5.4")
        || id.starts_with("gpt-5.5")
        || id.starts_with("gpt-5.6")
        || id.starts_with("gpt-6"))
        && effort == "minimal"
    {
        "low".to_owned()
    } else if id == "gpt-5.1" && effort == "xhigh" {
        "high".to_owned()
    } else {
        effort.to_owned()
    }
}

fn reasoning_for(provider: Provider, model: &str, value: Option<&Value>) -> Option<Value> {
    let mut reasoning = value?.clone();
    if provider == Provider::Codex {
        if let Some(reasoning) = reasoning.as_object_mut() {
            let effort = reasoning
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(effort) = effort {
                reasoning.insert(
                    "effort".to_owned(),
                    Value::String(clamp_reasoning_effort(model, &effort)),
                );
            }
        }
    }
    Some(reasoning)
}

fn insert_optional(body: &mut Value, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        body[key] = value;
    }
}

fn strip_unsupported_keys(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("cache_control");
            object.remove("rotom_session_id");
            for nested in object.values_mut() {
                strip_unsupported_keys(nested);
            }
        }
        Value::Array(array) => {
            for nested in array {
                strip_unsupported_keys(nested);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

trait TupleMapFirst<A, B> {
    fn map_first<C>(self, map: impl FnOnce(A) -> C) -> (C, B);
}

impl<A, B> TupleMapFirst<A, B> for (A, B) {
    fn map_first<C>(self, map: impl FnOnce(A) -> C) -> (C, B) {
        (map(self.0), self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::types::ChatCompletionRequest;

    fn request(value: Value) -> ChatCompletionRequest {
        serde_json::from_value(value).unwrap()
    }

    fn assert_key_absent_recursive(value: &Value, key: &str) {
        match value {
            Value::Object(object) => {
                assert!(
                    !object.contains_key(key),
                    "unexpected key `{key}` in object: {value}"
                );
                for nested in object.values() {
                    assert_key_absent_recursive(nested, key);
                }
            }
            Value::Array(array) => {
                for nested in array {
                    assert_key_absent_recursive(nested, key);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    #[test]
    fn strips_openai_codex_prefix() {
        assert_eq!(normalize_model("openai-codex/gpt-5.4"), "gpt-5.4");
        assert_eq!(normalize_model("gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn converts_chat_messages_to_responses_body() {
        let body = to_codex_request(&request(json!({
            "model": "openai-codex/gpt-5.4",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hello"}
            ],
            "temperature": 0.2
        })))
        .unwrap();

        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["instructions"], "be terse");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn omits_text_format_without_response_format() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "hello"}]
        })))
        .unwrap();

        assert_eq!(body["text"]["verbosity"], "medium");
        assert!(body["text"].get("format").is_none());
    }

    #[test]
    fn maps_json_schema_response_format_to_text_format() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "hello"}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "patch",
                    "strict": true,
                    "description": "a patch",
                    "schema": {"type": "object", "properties": {"id": {"type": "string"}},
                               "required": ["id"], "additionalProperties": false}
                }
            }
        })))
        .unwrap();

        let format = &body["text"]["format"];
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["name"], "patch");
        assert_eq!(format["strict"], true);
        assert_eq!(format["description"], "a patch");
        assert_eq!(format["schema"]["required"], json!(["id"]));
        assert!(
            format.get("json_schema").is_none(),
            "chat wrapper key must not leak upstream"
        );
        assert_eq!(body["text"]["verbosity"], "medium");
    }

    #[test]
    fn json_schema_response_format_defaults_name_and_strict() {
        let format = convert_response_format(Some(&json!({
            "type": "json_schema",
            "json_schema": { "schema": {"type": "object"} }
        })))
        .unwrap();

        assert_eq!(format["name"], "response");
        assert_eq!(format["strict"], true);
    }

    #[test]
    fn passes_json_object_response_format_through() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "hello"}],
            "response_format": {"type": "json_object"}
        })))
        .unwrap();

        assert_eq!(body["text"]["format"], json!({"type": "json_object"}));
    }

    #[test]
    fn ignores_malformed_response_format() {
        assert!(convert_response_format(Some(&json!("json_object"))).is_none());
        assert!(convert_response_format(Some(&json!({"no_type": 1}))).is_none());
        assert!(convert_response_format(None).is_none());
    }

    #[test]
    fn does_not_forward_unsupported_sampling_controls() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-6-astra",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 0.2,
            "top_p": 0.7,
            "parallel_tool_calls": false,
            "stop": ["DONE"]
        })))
        .unwrap();

        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["stop"], json!(["DONE"]));
    }

    #[test]
    fn converts_assistant_tool_calls_and_tool_results() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [
                {"role": "assistant", "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "done"}
            ]
        })))
        .unwrap();

        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][1]["type"], "function_call_output");
    }

    #[test]
    fn clamps_minimal_reasoning_for_models_without_minimal_effort() {
        for model in ["gpt-5.5", "gpt-6-astra"] {
            let body = to_codex_request(&request(json!({
                "model": model,
                "messages": [],
                "reasoning_effort": "minimal"
            })))
            .unwrap();

            assert_eq!(body["reasoning"]["effort"], "low");
        }
    }

    #[test]
    fn clamps_gpt_6_responses_minimal_reasoning_to_low() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-6-astra",
            "input": "hello",
            "reasoning": {"effort": "minimal"}
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn does_not_forward_unsupported_chat_completion_token_limit() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "max_completion_tokens": 42
        })))
        .unwrap();

        assert_key_absent_recursive(&body, "max_output_tokens");
    }

    #[test]
    fn maps_chat_tool_choice_to_responses_tool_choice() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "lookup"}}
        })))
        .unwrap();

        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "lookup"})
        );
    }

    #[test]
    fn preserves_string_tool_choice() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "tool_choice": "required"
        })))
        .unwrap();

        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn accepts_flat_responses_function_tools() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "look things up",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "q": {"type": "string"}
                    },
                    "required": ["q"]
                },
                "strict": false
            }]
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][0]["description"], "look things up");
        assert_eq!(body["tools"][0]["parameters"]["required"], json!(["q"]));
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn accepts_nested_chat_function_tools() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "look things up",
                    "parameters": {"type": "object"},
                    "strict": true
                }
            }]
        })))
        .unwrap();

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][0]["description"], "look things up");
        assert_eq!(body["tools"][0]["parameters"], json!({"type": "object"}));
        assert_eq!(body["tools"][0]["strict"], true);
    }

    #[test]
    fn normalizes_raw_responses_message_items_for_codex() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": []
        }))
        .unwrap();
        let input = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}]
            }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"q\":\"x\"}"
            }),
        ];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(body["input"][2]["type"], "function_call");
    }

    #[test]
    fn normalizes_raw_responses_tool_payloads_for_codex() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": []
        }))
        .unwrap();
        let input = vec![
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": {"q": "x"}
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [
                    {"type": "text", "text": "one"},
                    {"type": "output_text", "text": "two"}
                ]
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_2",
                "output": {"ok": true}
            }),
        ];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["input"][0]["arguments"], "{\"q\":\"x\"}");
        assert_eq!(body["input"][1]["output"], "one\ntwo");
        assert_eq!(body["input"][2]["output"], "{\"ok\":true}");
    }

    #[test]
    fn lifts_raw_responses_system_items_into_instructions() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "instructions": "base"
        }))
        .unwrap();
        let input = vec![
            json!({
                "type": "message",
                "role": "system",
                "content": [{"type": "input_text", "text": "system prompt"}]
            }),
            json!({
                "role": "developer",
                "content": "developer prompt"
            }),
            json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "done"
            }),
        ];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["instructions"], "base\n\nsystem prompt");
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "developer prompt");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert!(
            !body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| matches!(item.get("role").and_then(Value::as_str), Some("system")))
        );
    }

    #[test]
    fn responses_request_always_includes_instructions() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": "hello"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["instructions"], "");
    }

    #[test]
    fn codex_responses_requests_force_upstream_streaming() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "stream": false,
            "input": "hello"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["stream"], true);
    }

    #[test]
    fn grok_responses_requests_preserve_downstream_streaming() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "grok-4.3",
            "stream": false,
            "input": "hello"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_upstream_request(Provider::Grok, &request, &input).unwrap();

        assert_eq!(body["stream"], false);
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn vercel_responses_requests_preserve_gateway_controls() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "openai/gpt-6-astra",
            "stream": false,
            "store": true,
            "instructions": "be terse",
            "temperature": 0.2,
            "input": "hello"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_upstream_request(Provider::Vercel, &request, &input).unwrap();

        assert_eq!(body["model"], "openai/gpt-6-astra");
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], true);
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["temperature"], 0.2);
    }

    #[test]
    fn grok_responses_requests_forward_non_empty_instructions_without_previous_response_id() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "grok-4.3",
            "instructions": "be terse",
            "input": "hello"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_upstream_request(Provider::Grok, &request, &input).unwrap();

        assert_eq!(body["instructions"], "be terse");
    }

    #[test]
    fn codex_responses_native_replay_forces_upstream_store_false() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": []
        }))
        .unwrap();
        let input = vec![
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "work"}],
                "encrypted_content": "sig"
            }),
            json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }),
        ];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["store"], false);
    }

    #[test]
    fn grok_responses_native_replay_preserves_store_and_previous_response_id() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "grok-4.3",
            "store": true,
            "previous_response_id": "resp_upstream",
            "input": []
        }))
        .unwrap();
        let input = vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "done"
        })];

        let body = responses_to_upstream_request(Provider::Grok, &request, &input).unwrap();

        assert_eq!(body["store"], true);
        assert_eq!(body["previous_response_id"], "resp_upstream");
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn forwards_responses_controls_for_provider_filtering() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "temperature": 0.2,
            "top_p": 0.8,
            "max_output_tokens": 128,
            "stop": ["DONE"],
            "text": {"verbosity": "low"},
            "include": ["reasoning.encrypted_content"],
            "input": "hello"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["stop"], json!(["DONE"]));
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn strips_cache_control_recursively_from_chat_requests() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [{
                "role": "user",
                "content": "hello"
            }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "parameters": {"type": "object"}
                },
                "cache_control": {"type": "ephemeral"}
            }]
        })))
        .unwrap();

        assert_key_absent_recursive(&body, "cache_control");
    }

    #[test]
    fn strips_cache_control_recursively_from_responses_requests() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "parameters": {"type": "object"}
                },
                "cache_control": {"type": "ephemeral"}
            }]
        }))
        .unwrap();
        let input = vec![json!({
            "role": "developer",
            "content": [{
                "type": "input_text",
                "text": "be terse",
                "cache_control": {"type": "ephemeral"}
            }]
        })];

        let body = responses_to_codex_request(&request, &input).unwrap();

        assert_key_absent_recursive(&body, "cache_control");
    }

    #[test]
    fn rejects_function_tool_without_metadata() {
        let error = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "tools": [{"type": "function"}]
        })))
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "configuration error: function tool is missing function metadata"
        );
    }

    #[test]
    fn derives_stable_prompt_cache_key_across_conversation_turns() {
        let turn1 = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "first question"}
            ]
        })))
        .unwrap();
        let turn2 = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "first answer"},
                {"role": "user", "content": "second question"}
            ]
        })))
        .unwrap();

        let key1 = turn1["prompt_cache_key"].as_str().unwrap();
        let key2 = turn2["prompt_cache_key"].as_str().unwrap();
        assert_ne!(key1, "");
        assert_eq!(key1, key2);
    }

    #[test]
    fn extracts_claude_code_session_from_metadata_user_id() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "metadata": {
                "user_id": "user_abc123_account_11111111-2222-3333-4444-555555555555_session_99999999-aaaa-bbbb-cccc-dddddddddddd"
            },
            "messages": [{"role": "user", "content": "hello"}]
        })))
        .unwrap();

        assert_eq!(
            body["prompt_cache_key"],
            "99999999-aaaa-bbbb-cccc-dddddddddddd"
        );
    }

    #[test]
    fn forwards_prompt_cache_retention_only_for_vercel() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "openai/gpt-5",
            "prompt_cache_key": "conv-123",
            "prompt_cache_retention": "24h"
        }))
        .unwrap();
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        })];

        let vercel = responses_to_upstream_request(Provider::Vercel, &req, &input).unwrap();
        let grok = responses_to_upstream_request(Provider::Grok, &req, &input).unwrap();
        let codex = responses_to_upstream_request(Provider::Codex, &req, &input).unwrap();

        assert_eq!(vercel["prompt_cache_key"], "conv-123");
        assert_eq!(vercel["prompt_cache_retention"], "24h");
        assert_eq!(grok["prompt_cache_key"], "conv-123");
        assert!(grok.get("prompt_cache_retention").is_none());
        assert_eq!(codex["prompt_cache_key"], "conv-123");
        assert!(codex.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn keeps_trailing_depth_system_messages_in_input_to_preserve_prefix_cache() {
        let turn1 = to_codex_request(&request(json!({
            "model": "grok-4.3",
            "messages": [
                {"role": "system", "content": "main system prompt"},
                {"role": "system", "content": "character card"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi there"},
                {"role": "system", "content": "[Depth 2 World Info: tavern]"},
                {"role": "user", "content": "look around"},
                {"role": "system", "content": "[Post-History Instructions: stay in character]"}
            ]
        })))
        .unwrap();

        assert_eq!(
            turn1["instructions"],
            "main system prompt\n\ncharacter card"
        );
        let input = turn1["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["role"], "developer");
        assert_eq!(
            input[2]["content"][0]["text"],
            "[Depth 2 World Info: tavern]"
        );
        assert_eq!(input[3]["role"], "user");
        assert_eq!(input[4]["role"], "developer");
        assert_eq!(
            input[4]["content"][0]["text"],
            "[Post-History Instructions: stay in character]"
        );
    }

    #[test]
    fn preserves_prompt_cache_key_across_sliding_window_truncation() {
        let turn1 = to_codex_request(&request(json!({
            "model": "grok-4.3",
            "messages": [
                {"role": "system", "content": "stable character card prefix"},
                {"role": "user", "content": "turn 1 user"},
                {"role": "assistant", "content": "turn 1 assistant"},
                {"role": "user", "content": "turn 2 user"},
                {"role": "assistant", "content": "turn 2 assistant"},
                {"role": "user", "content": "turn 3 user"}
            ]
        })))
        .unwrap();

        // Turn 2 simulates SillyTavern sliding window dropping turn 1 (oldest messages)
        // when the 71.9K context limit is reached.
        let turn2 = to_codex_request(&request(json!({
            "model": "grok-4.3",
            "messages": [
                {"role": "system", "content": "stable character card prefix"},
                {"role": "user", "content": "turn 2 user"},
                {"role": "assistant", "content": "turn 2 assistant"},
                {"role": "user", "content": "turn 3 user"},
                {"role": "assistant", "content": "turn 3 assistant"},
                {"role": "user", "content": "turn 4 user"}
            ]
        })))
        .unwrap();

        let key1 = turn1["prompt_cache_key"].as_str().unwrap();
        let key2 = turn2["prompt_cache_key"].as_str().unwrap();
        assert_ne!(key1, "");
        assert_eq!(key1, key2);
    }
}
