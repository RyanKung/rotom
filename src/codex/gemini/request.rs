use super::ReplayCache;
use crate::{Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(super) async fn payload(body: &Value, project: &str, replay: &ReplayCache) -> Result<Value> {
    let model = required_string(body, "model")?;
    let model = model.strip_prefix("gemini/").unwrap_or(model);
    let input = match body.get("input") {
        Some(Value::String(text)) => vec![json!({"role":"user", "content":text})],
        Some(Value::Array(items)) => items.clone(),
        _ => return Err(Error::config("Gemini requires input messages")),
    };
    let mut contents = Vec::new();
    let mut names = HashMap::new();
    let mut system = Vec::new();
    if let Some(text) = body
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        system.push(json!({"text":text}));
    }
    for item in &input {
        convert_item(
            item,
            project,
            replay,
            &mut names,
            &mut contents,
            &mut system,
        )
        .await?;
    }
    if contents.is_empty() {
        return Err(Error::config("Gemini requires nonempty conversation input"));
    }
    let session = session_id(body, &contents);
    let mut request = json!({"contents":contents,"sessionId":session});
    if let Some(schema) = body.pointer("/text/format/schema") {
        system.push(json!({"text":format!("When returning a final answer, return only JSON satisfying this JSON Schema, including all numeric and array bounds: {schema}")}));
    } else if body.pointer("/text/format/type").and_then(Value::as_str) == Some("json_object") {
        system.push(json!({"text":"Return only a valid JSON object answering the user's request, without Markdown fences or surrounding commentary."}));
    }
    if !system.is_empty() {
        request["systemInstruction"] = json!({"parts":system});
    }
    add_config(body, &mut request)?;
    Ok(json!({"model":model,"project":project,"request":request,
        "userAgent":"antigravity","requestType":"agent","requestId":format!("agent-{:032x}",rand::random::<u128>())}))
}

// Stable across turns, including image-only conversations: honor explicit session/cache keys
// when supplied, or hash the first user content.
fn session_id(body: &Value, contents: &[Value]) -> String {
    let explicit = [
        "rotom_session_id",
        "conversation_id",
        "session_id",
        "prompt_cache_key",
    ]
    .into_iter()
    .find_map(|key| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    });
    let seed = explicit.map_or_else(
        || {
            let first = contents
                .iter()
                .find(|item| item.get("role").and_then(Value::as_str) == Some("user"));
            first.map_or_else(String::new, Value::to_string)
        },
        str::to_owned,
    );
    let digest = Sha256::digest(seed.as_bytes());
    let n = digest
        .iter()
        .take(8)
        .fold(0_u64, |n, byte| (n << 8) | u64::from(*byte));
    format!("-{}", n & 0x7fff_ffff_ffff_ffff)
}

async fn convert_item(
    item: &Value,
    project: &str,
    replay: &ReplayCache,
    names: &mut HashMap<String, String>,
    contents: &mut Vec<Value>,
    system: &mut Vec<Value>,
) -> Result<()> {
    match item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "function_call" => {
            let id = required_string(item, "call_id")?;
            let name = required_string(item, "name")?;
            let args: Value = serde_json::from_str(
                item.get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}"),
            )?;
            if !args.is_object() {
                return Err(Error::config(
                    "Gemini function arguments must be a JSON object",
                ));
            }
            names.insert(id.into(), name.into());
            let part = match replay.get(project, id).await {
                Some(part) => {
                    if part.pointer("/functionCall/name").and_then(Value::as_str) != Some(name)
                        || part.pointer("/functionCall/args") != Some(&args)
                    {
                        return Err(Error::config("Gemini signed tool call was modified"));
                    }
                    part
                }
                None if id.starts_with("gemini_") => {
                    return Err(Error::config(
                        "Gemini tool replay expired or belongs to another project; start a new conversation",
                    ));
                }
                None => json!({"functionCall":{"name":name,"args":args}}),
            };
            push_parts(contents, "model", vec![part]);
        }
        "function_call_output" => {
            let id = required_string(item, "call_id")?;
            let name = names.get(id).ok_or_else(|| {
                Error::config("Gemini tool output has no preceding function call")
            })?;
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            push_parts(
                contents,
                "user",
                vec![json!({"functionResponse":{"name":name,"response":{"output":output}}})],
            );
        }
        "message" => {
            let role = required_string(item, "role")?;
            let parts = content_parts(item.get("content"))?;
            match role {
                "system" | "developer" if contents.is_empty() => system.extend(parts),
                "system" | "developer" | "user" => push_parts(contents, "user", parts),
                "assistant" => push_parts(contents, "model", parts),
                _ => return Err(Error::config("unsupported Gemini message role")),
            }
        }
        // Local Responses reasoning summaries are not executable conversation content.
        "reasoning" => {}
        _ => return Err(Error::config("unsupported Gemini input item")),
    }
    Ok(())
}

fn push_parts(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut) {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({"role":role,"parts":parts}));
}

fn content_parts(content: Option<&Value>) -> Result<Vec<Value>> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({"text":text})]),
        Some(Value::Array(parts)) => parts.iter().map(|part| {
            match part.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => Ok(json!({"text":required_string(part,"text")?})),
                Some("input_image") => {
                    let image = required_string(part,"image_url")?;
                    let (mime, data) = image.strip_prefix("data:").and_then(|s| s.split_once(";base64,"))
                        .ok_or_else(|| Error::config("Gemini supports inline base64 images; remote image URLs must be inlined by the caller"))?;
                    Ok(json!({"inlineData":{"mimeType":mime,"data":data}}))
                }
                _ => Err(Error::config("unsupported Gemini content part")),
            }
        }).collect(),
        _ => Err(Error::config("invalid Gemini message content")),
    }
}

fn add_config(body: &Value, request: &mut Value) -> Result<()> {
    let mut config = serde_json::Map::new();
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_output_tokens", "maxOutputTokens"),
        ("stop", "stopSequences"),
    ] {
        if let Some(value) = body.get(source).filter(|v| !v.is_null()) {
            config.insert(target.into(), value.clone());
        }
    }
    add_response_format(body, &mut config)?;
    if !config.is_empty() {
        request["generationConfig"] = Value::Object(config);
    }
    if let Some(tools) = body
        .get("tools")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
    {
        let declarations = tools.iter().map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(Error::config("Gemini subscription adapter supports function tools only"));
            }
            let mut declaration = json!({"name":required_string(tool,"name")?,
                "parametersJsonSchema":tool.get("parameters").cloned().unwrap_or_else(||json!({"type":"object","properties":{}}))});
            if let Some(description) = tool.get("description") { declaration["description"] = description.clone(); }
            Ok(declaration)
        }).collect::<Result<Vec<_>>>()?;
        request["tools"] = json!([{"functionDeclarations":declarations}]);
        let choice = body.get("tool_choice");
        let mode = match choice.and_then(Value::as_str) {
            Some("none") => "NONE",
            Some("required") => "ANY",
            _ => "AUTO",
        };
        let calling = choice
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .map_or_else(
                || json!({"mode":mode}),
                |name| json!({"mode":"ANY","allowedFunctionNames":[name]}),
            );
        request["toolConfig"] = json!({"functionCallingConfig":calling});
    }
    Ok(())
}

// Chat response_format is normalized to Responses text.format before this boundary.
// Adapt schema syntax without dropping constraints; unsupported constraints fail upstream.
fn add_response_format(body: &Value, config: &mut serde_json::Map<String, Value>) -> Result<()> {
    let Some(format) = body
        .pointer("/text/format")
        .filter(|value| !value.is_null())
    else {
        return Ok(());
    };
    match format.get("type").and_then(Value::as_str) {
        Some("text") => {}
        Some("json_object") => {
            config.insert("responseMimeType".into(), json!("application/json"));
        }
        Some("json_schema") => {
            let schema = format
                .get("schema")
                .filter(|value| value.is_object())
                .ok_or_else(|| Error::config("Gemini json_schema requires an object schema"))?;
            if format
                .get("strict")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(Error::config("Gemini json_schema strict must be a boolean"));
            }
            super::schema::validator(body)?;
            // Antigravity constrains generation with responseSchema; it has no
            // separate strict flag. OpenAI's name/description are envelope metadata.
            config.insert("responseMimeType".into(), json!("application/json"));
            config.insert(
                "responseSchema".into(),
                super::schema::convert(schema).map_err(|error| Error::config(error.to_string()))?,
            );
        }
        _ => return Err(Error::config("unsupported Gemini response format")),
    }
    Ok(())
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::config(format!("Gemini input requires {key}")))
}
