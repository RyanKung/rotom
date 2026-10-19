fn model_from_request(request: &Value) -> &str {
    request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("auto")
}

fn kiro_model_id(model: &str) -> String {
    model
        .strip_prefix("kiro/")
        .or_else(|| model.strip_prefix("openai-codex/kiro/"))
        .unwrap_or(model)
        .to_owned()
}

fn messages_from_request(request: &Value) -> Result<Vec<KiroMessage>> {
    match request.get("input") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| message_from_input_item(item).transpose())
            .collect(),
        Some(Value::String(text)) => Ok(vec![KiroMessage {
            role: KiroRole::User,
            content: text.clone(),
            ..KiroMessage::default()
        }]),
        _ => Ok(Vec::new()),
    }
}

fn message_from_input_item(item: &Value) -> Result<Option<KiroMessage>> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Ok(Some(KiroMessage {
            role: KiroRole::Assistant,
            content: EMPTY_PLACEHOLDER.to_owned(),
            tool_uses: vec![KiroToolUse {
                id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call")
                    .to_owned(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned(),
                input: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str(arguments).ok())
                    .unwrap_or_else(|| json!({})),
            }],
            ..KiroMessage::default()
        })),
        Some("function_call_output") => Ok(Some(KiroMessage {
            role: KiroRole::User,
            content: tool_output_text(item),
            tool_results: vec![tool_result_from_item(item)],
            ..KiroMessage::default()
        })),
        Some("message") | None => {
            let role = match item.get("role").and_then(Value::as_str) {
                Some("assistant") => KiroRole::Assistant,
                _ => KiroRole::User,
            };
            let content = item.get("content");
            Ok(Some(KiroMessage {
                role,
                content: content_text(content),
                images: if role == KiroRole::User {
                    image_blocks(content)?
                } else {
                    Vec::new()
                },
                documents: if role == KiroRole::User {
                    document_blocks(content)?
                } else {
                    Vec::new()
                },
                ..KiroMessage::default()
            }))
        }
        Some("reasoning" | "image_generation_call") => Ok(None),
        Some(_) => item
            .get("content")
            .map(|content| {
                Ok(KiroMessage {
                    role: KiroRole::User,
                    content: content_text(Some(content)),
                    images: image_blocks(Some(content))?,
                    documents: document_blocks(Some(content))?,
                    ..KiroMessage::default()
                })
            })
            .transpose(),
    }
}

fn tool_output_text(item: &Value) -> String {
    item.get("output")
        .and_then(Value::as_str)
        .or_else(|| item.get("content").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .unwrap_or(EMPTY_PLACEHOLDER)
        .to_owned()
}

fn tool_result_from_item(item: &Value) -> Value {
    json!({
        "content": [{"text": tool_output_text(item)}],
        "status": "success",
        "toolUseId": item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    })
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(content_part_text).collect(),
        Some(Value::Object(_)) => content_part_text(value.unwrap())
            .unwrap_or_default()
            .to_owned(),
        Some(value) => value.as_str().unwrap_or_default().to_owned(),
        None => String::new(),
    }
}

fn content_part_text(part: &Value) -> Option<&str> {
    part.get("text")
        .or_else(|| part.get("input_text"))
        .or_else(|| part.get("output_text"))
        .or_else(|| part.get("refusal"))
        .and_then(Value::as_str)
}

fn image_blocks(value: Option<&Value>) -> Result<Vec<Value>> {
    let mut images = Vec::new();
    for part in content_parts(value) {
        let kind = part.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("input_image" | "image_url" | "image")) {
            continue;
        }
        let Some(url) = image_url_from_part(part) else {
            continue;
        };
        let (mime_type, data) = data_url_payload(url, "image")?;
        let format = match mime_type.as_str() {
            "image/png" => "png",
            "image/jpeg" | "image/jpg" => "jpeg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => {
                return Err(Error::config(format!(
                    "Kiro image input does not support MIME type {mime_type}"
                )));
            }
        };
        images.push(json!({
            "format": format,
            "source": { "bytes": data }
        }));
    }
    Ok(images)
}

fn document_blocks(value: Option<&Value>) -> Result<Vec<Value>> {
    let mut documents = Vec::new();
    for part in content_parts(value) {
        let kind = part.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("input_file" | "document_url" | "document")) {
            continue;
        }
        if references_external_document(part) {
            return Err(Error::config(
                "Kiro document input supports only inline base64 document data; remote URLs and file IDs are not fetched",
            ));
        }
        let Some((name, mime_type, data)) = document_data_from_part(part)? else {
            continue;
        };
        let format = match mime_type.as_str() {
            "application/pdf" => "pdf",
            "text/csv" => "csv",
            "application/msword" => "doc",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
            "application/vnd.ms-excel" => "xls",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
            "text/html" => "html",
            "text/plain" => "txt",
            "text/markdown" => "md",
            _ => {
                return Err(Error::config(format!(
                    "Kiro document input does not support MIME type {mime_type}"
                )));
            }
        };
        documents.push(json!({
            "name": sanitize_document_name(&name),
            "format": format,
            "source": { "bytes": data }
        }));
    }
    Ok(documents)
}

fn content_parts(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(parts)) => parts.iter().collect(),
        Some(Value::Object(_)) => value.into_iter().collect(),
        _ => Vec::new(),
    }
}

fn image_url_from_part(part: &Value) -> Option<&str> {
    part.get("image_url")
        .and_then(string_or_url_object)
        .or_else(|| part.pointer("/source/url").and_then(Value::as_str))
}

fn string_or_url_object(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("url").and_then(Value::as_str))
}

fn document_data_from_part(part: &Value) -> Result<Option<(String, String, String)>> {
    if let Some(file_data) = part.get("file_data").and_then(Value::as_str) {
        let (mime_type, data) = data_url_payload(file_data, "document")?;
        return Ok(Some((document_name(part), mime_type, data)));
    }

    if let Some(source) = part.get("source") {
        if let Some(data) = source.get("data").and_then(Value::as_str) {
            let mime_type = source
                .get("media_type")
                .or_else(|| source.get("mimeType"))
                .or_else(|| source.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("text/plain")
                .to_ascii_lowercase();
            return Ok(Some((document_name(part), mime_type, data.to_owned())));
        }
    }

    if let Some(document_url) = part.get("document_url") {
        if let Some(data) = document_url.get("data").and_then(Value::as_str) {
            let mime_type = document_url
                .get("mimeType")
                .or_else(|| document_url.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("text/plain")
                .to_ascii_lowercase();
            let name = document_url
                .get("name")
                .or_else(|| document_url.get("filename"))
                .and_then(Value::as_str)
                .map_or_else(|| document_name(part), str::to_owned);
            return Ok(Some((name, mime_type, data.to_owned())));
        }
    }

    Ok(None)
}

fn references_external_document(part: &Value) -> bool {
    part.get("file_id").and_then(Value::as_str).is_some()
        || part.get("document_url").and_then(Value::as_str).is_some()
        || part
            .get("document_url")
            .and_then(|value| value.get("url"))
            .and_then(Value::as_str)
            .is_some()
        || part
            .get("source")
            .and_then(|value| value.get("url").or_else(|| value.get("file_id")))
            .and_then(Value::as_str)
            .is_some()
}

fn document_name(part: &Value) -> String {
    part.get("filename")
        .or_else(|| part.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("document")
        .to_owned()
}

fn sanitize_document_name(name: &str) -> String {
    let sanitized = name
        .rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '-' | '_' | '(' | ')' | '[' | ']')
            {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let sanitized = sanitized.trim_matches('-').trim();
    if sanitized.is_empty() {
        "document".to_owned()
    } else {
        sanitized.chars().take(200).collect()
    }
}

fn data_url_payload(url: &str, kind: &str) -> Result<(String, String)> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(Error::config(format!(
            "Kiro {kind} input supports only inline data URLs"
        )));
    };
    let Some((metadata, data)) = rest.split_once(',') else {
        return Err(Error::config(format!(
            "Kiro {kind} data URL is missing data"
        )));
    };
    if !metadata
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Err(Error::config(format!(
            "Kiro {kind} data URL must be base64 encoded"
        )));
    }
    if data.trim().is_empty() {
        return Err(Error::config(format!("Kiro {kind} data URL is empty")));
    }
    let mime_type = metadata
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("text/plain")
        .to_ascii_lowercase();
    Ok((mime_type, data.to_owned()))
}

fn prepend_instructions(messages: &mut [KiroMessage], instructions: &str) {
    if let Some(message) = messages
        .iter_mut()
        .find(|message| message.role == KiroRole::User)
    {
        message.content = if message.content.trim().is_empty() {
            instructions.to_owned()
        } else {
            format!("{instructions}\n\n{}", message.content)
        };
    }
}

fn normalize_messages(messages: Vec<KiroMessage>) -> Vec<KiroMessage> {
    let mut merged = Vec::<KiroMessage>::new();
    for mut message in messages {
        if message.content.trim().is_empty()
            && message.images.is_empty()
            && message.documents.is_empty()
            && message.tool_uses.is_empty()
            && message.tool_results.is_empty()
        {
            EMPTY_PLACEHOLDER.clone_into(&mut message.content);
        }
        if let Some(index) = merged.len().checked_sub(1)
            && merged[index].role == message.role
        {
            if can_merge_adjacent(&merged[index], &message) {
                merge_message(&mut merged[index], message);
            } else {
                let role = merged[index].role.opposite();
                merged.push(KiroMessage {
                    role,
                    content: EMPTY_PLACEHOLDER.to_owned(),
                    ..KiroMessage::default()
                });
                merged.push(message);
            }
            continue;
        }
        merged.push(message);
    }

    if merged
        .first()
        .is_some_and(|message| message.role != KiroRole::User)
    {
        merged.insert(
            0,
            KiroMessage {
                role: KiroRole::User,
                content: EMPTY_PLACEHOLDER.to_owned(),
                ..KiroMessage::default()
            },
        );
    }
    merged
}

impl KiroRole {
    const fn opposite(self) -> Self {
        match self {
            Self::User => Self::Assistant,
            Self::Assistant => Self::User,
        }
    }
}

// Parallel tool calls arrive as one `function_call` item per call, and their
// outputs as one `function_call_output` item each. Bedrock requires every
// toolUse of an assistant turn to be answered in the next user turn, so the
// calls must share one assistant message and the results one user message.
fn can_merge_adjacent(previous: &KiroMessage, message: &KiroMessage) -> bool {
    match message.role {
        KiroRole::Assistant => true,
        KiroRole::User => previous.tool_results.is_empty() == message.tool_results.is_empty(),
    }
}

fn merge_message(previous: &mut KiroMessage, message: KiroMessage) {
    if !message.content.trim().is_empty() && message.content != EMPTY_PLACEHOLDER {
        if previous.content.trim().is_empty() || previous.content == EMPTY_PLACEHOLDER {
            previous.content = message.content;
        } else {
            previous.content.push_str("\n\n");
            previous.content.push_str(&message.content);
        }
    }
    previous.images.extend(message.images);
    previous.documents.extend(message.documents);
    previous.tool_uses.extend(message.tool_uses);
    previous.tool_results.extend(message.tool_results);
}

fn history_item(message: &KiroMessage, model: &str) -> Value {
    match message.role {
        KiroRole::User => {
            json!({ "userInputMessage": user_input_message(message, model, &json!({})) })
        }
        KiroRole::Assistant => {
            let mut assistant = json!({
                "content": non_empty_content(&message.content),
            });
            if !message.tool_uses.is_empty() {
                assistant["toolUses"] = Value::Array(
                    message
                        .tool_uses
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "input": tool.input,
                                "toolUseId": tool.id,
                            })
                        })
                        .collect(),
                );
            }
            json!({ "assistantResponseMessage": assistant })
        }
    }
}

fn user_input_message(message: &KiroMessage, model: &str, request: &Value) -> Value {
    let mut user_input = json!({
        "content": non_empty_content(&message.content),
        "modelId": model,
        "origin": "AI_EDITOR",
    });
    if !message.images.is_empty() {
        user_input["images"] = Value::Array(message.images.clone());
    }
    if !message.documents.is_empty() {
        user_input["documents"] = Value::Array(message.documents.clone());
    }
    let mut context = Map::new();
    let tools = tools_for_request(request);
    if !tools.is_empty() {
        context.insert("tools".to_owned(), Value::Array(tools));
    }
    if !message.tool_results.is_empty() {
        context.insert(
            "toolResults".to_owned(),
            Value::Array(message.tool_results.clone()),
        );
    }
    if !context.is_empty() {
        user_input["userInputMessageContext"] = Value::Object(context);
    }
    user_input
}

fn tools_for_request(request: &Value) -> Vec<Value> {
    if tool_choice_disables_tools(request.get("tool_choice")) {
        return Vec::new();
    }
    let selected_tool = selected_tool_name(request.get("tool_choice"));
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| {
            selected_tool.is_none_or(|name| {
                tool.get("name")
                    .or_else(|| tool.pointer("/function/name"))
                    .and_then(Value::as_str)
                    == Some(name)
            })
        })
        .filter_map(convert_tool_to_kiro)
        .collect()
}

fn tool_choice_disables_tools(choice: Option<&Value>) -> bool {
    choice.and_then(Value::as_str) == Some("none")
}

fn selected_tool_name(choice: Option<&Value>) -> Option<&str> {
    let choice = choice?.as_object()?;
    if choice.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    choice
        .get("name")
        .or_else(|| {
            choice
                .get("function")
                .and_then(|function| function.get("name"))
        })
        .and_then(Value::as_str)
}

fn convert_tool_to_kiro(tool: &Value) -> Option<Value> {
    let name = tool
        .get("name")
        .or_else(|| tool.pointer("/function/name"))?
        .as_str()?;
    let description = tool
        .get("description")
        .or_else(|| tool.pointer("/function/description"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Tool");
    let parameters = tool
        .get("parameters")
        .or_else(|| tool.pointer("/function/parameters"))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    Some(json!({
        "toolSpecification": {
            "name": name,
            "description": description,
            "inputSchema": {
                "json": sanitize_schema(parameters)
            }
        }
    }))
}

fn sanitize_schema(value: Value) -> Value {
    match value {
        Value::Object(mut object) => {
            object.remove("additionalProperties");
            if object
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                object.remove("required");
            }
            let sanitized = object
                .into_iter()
                .map(|(key, value)| (key, sanitize_schema(value)))
                .collect();
            Value::Object(sanitized)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_schema).collect()),
        other => other,
    }
}

fn non_empty_content(content: &str) -> String {
    if content.trim().is_empty() {
        EMPTY_PLACEHOLDER.to_owned()
    } else {
        content.to_owned()
    }
}

fn conversation_id_from_request(request: &Value) -> String {
    request
        .get("conversation_id")
        .or_else(|| request.get("previous_response_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(random_conversation_id, str::to_owned)
}

fn random_conversation_id() -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        rand::random::<u32>(),
        rand::random::<u16>(),
        rand::random::<u16>(),
        rand::random::<u16>(),
        rand::random::<u64>() & 0x0000_ffff_ffff_ffff
    )
}

fn input_fragment(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(object) if object.is_empty() => String::new(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn normalize_tool_arguments(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return "{}".to_owned();
    }
    serde_json::from_str::<Value>(arguments)
        .map_or_else(|_| "{}".to_owned(), |value| value.to_string())
}

fn output_text_part(text: &str) -> Value {
    json!({
        "type": "output_text",
        "text": text,
        "annotations": []
    })
}

fn json_event(event: &str, value: Value) -> JsonSseEvent {
    JsonSseEvent {
        event: Some(event.to_owned()),
        value,
    }
}

fn kiro_metadata(credit_usage: Option<f64>, context_usage_percentage: Option<f64>) -> Value {
    let mut metadata = Map::new();
    if let Some(usage) = credit_usage {
        metadata.insert("kiro_credit_usage".to_owned(), json!(usage));
    }
    if let Some(percentage) = context_usage_percentage {
        metadata.insert(
            "kiro_context_usage_percentage".to_owned(),
            json!(percentage),
        );
    }
    Value::Object(metadata)
}

fn copy_optional_response_field(response: &mut Value, request: &Value, key: &str) {
    if let Some(value) = request.get(key)
        && let Some(object) = response.as_object_mut()
    {
        object.insert(key.to_owned(), value.clone());
    }
}

fn kiro_error_message(value: &Value) -> Option<String> {
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("__type"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn machine_fingerprint(credentials: &Credentials) -> String {
    let hostname = env::var("HOSTNAME").unwrap_or_default();
    let username = env::var("USER").unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(hostname.as_bytes());
    hasher.update(b":");
    hasher.update(username.as_bytes());
    hasher.update(b":");
    hasher.update(credentials.account_id.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

fn invocation_id() -> String {
    random_conversation_id()
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}
