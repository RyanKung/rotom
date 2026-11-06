use serde_json::{Value, json};

pub(super) struct NormalizedResponsesInput {
    pub(super) items: Vec<Value>,
    pub(super) instructions: Vec<String>,
}

pub(super) fn normalize(input: &[Value]) -> NormalizedResponsesInput {
    let mut normalized = NormalizedResponsesInput {
        items: Vec::new(),
        instructions: Vec::new(),
    };

    for item in input {
        if normalized.items.is_empty()
            && let Some(instruction) = instruction_from_input_item(item)
        {
            normalized.instructions.push(instruction);
        } else {
            normalized.items.push(normalize_input_item(item));
        }
    }

    normalized
}

pub(super) fn combined_instructions(
    request_instructions: Option<&str>,
    input_instructions: &[String],
) -> String {
    let mut parts = Vec::new();
    if let Some(instructions) = request_instructions.filter(|value| !value.trim().is_empty()) {
        parts.push(instructions);
    }
    parts.extend(
        input_instructions
            .iter()
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty()),
    );
    parts.join("\n\n")
}

fn instruction_from_input_item(item: &Value) -> Option<String> {
    let object = item.as_object()?;
    let kind = object.get("type").and_then(Value::as_str);
    let type_is_message = kind.is_none_or(str::is_empty) || kind == Some("message");
    if !type_is_message {
        return None;
    }
    if object.get("role").and_then(Value::as_str) != Some("system") {
        return None;
    }

    Some(
        object
            .get("content")
            .and_then(instruction_content_to_string)
            .unwrap_or_default(),
    )
}

fn instruction_content_to_string(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => text_parts_to_string(parts),
        _ => None,
    }
}

fn normalize_input_item(item: &Value) -> Value {
    let mut item = item.clone();
    let Some(object) = item.as_object_mut() else {
        return item;
    };

    let type_is_missing = object
        .get("type")
        .is_none_or(|kind| kind.is_null() || kind.as_str() == Some(""));
    let is_message_like =
        object.contains_key("role") && object.contains_key("content") && type_is_missing;
    if is_message_like {
        object.insert("type".to_owned(), Value::String("message".to_owned()));
    }

    if object.get("type").and_then(Value::as_str) == Some("message") {
        let mut role = object
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_owned();
        if role == "system" {
            "developer".clone_into(&mut role);
            object.insert("role".to_owned(), Value::String(role.clone()));
        }
        if let Some(content) = object.get_mut("content") {
            normalize_message_content(content, &role);
        }
    }
    if object.get("type").and_then(Value::as_str) == Some("function_call")
        && let Some(arguments) = object.get_mut("arguments")
    {
        *arguments = Value::String(arguments_to_string(arguments));
    }
    if object.get("type").and_then(Value::as_str) == Some("function_call_output")
        && let Some(output) = object.get_mut("output")
    {
        *output = Value::String(tool_output_to_string(output));
    }

    item
}

fn arguments_to_string(arguments: &Value) -> String {
    match arguments {
        Value::String(value) if !value.trim().is_empty() => value.clone(),
        Value::String(_) | Value::Null => "{}".to_owned(),
        other => other.to_string(),
    }
}

fn tool_output_to_string(output: &Value) -> String {
    match output {
        Value::String(value) => value.clone(),
        Value::Null => String::new(),
        Value::Array(parts) => text_parts_to_string(parts).unwrap_or_else(|| output.to_string()),
        other => other.to_string(),
    }
}

fn text_parts_to_string(parts: &[Value]) -> Option<String> {
    let mut text = Vec::new();
    for part in parts {
        let kind = part.get("type").and_then(Value::as_str);
        match kind {
            Some("text" | "input_text" | "output_text") => {
                text.push(part.get("text").and_then(Value::as_str).unwrap_or_default());
            }
            _ => return None,
        }
    }
    Some(text.join("\n"))
}

fn normalize_message_content(content: &mut Value, role: &str) {
    match content {
        Value::String(text) => {
            *content = Value::Array(vec![json!({
                "type": text_part_type(role),
                "text": text.clone()
            })]);
        }
        Value::Array(parts) => {
            for part in parts {
                let Some(object) = part.as_object_mut() else {
                    continue;
                };
                if object.get("type").and_then(Value::as_str) == Some("text") {
                    object.insert(
                        "type".to_owned(),
                        Value::String(text_part_type(role).to_owned()),
                    );
                }
            }
        }
        _ => {}
    }
}

fn text_part_type(role: &str) -> &'static str {
    if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lifts_system_messages_without_lifting_developer_messages() {
        let normalized = normalize(&[
            json!({"type": "message", "role": "system", "content": "system prompt"}),
            json!({"role": "developer", "content": "developer prompt"}),
            json!({"role": "user", "content": "hello"}),
        ]);

        assert_eq!(normalized.instructions, vec!["system prompt"]);
        assert_eq!(normalized.items.len(), 2);
        assert_eq!(normalized.items[0]["role"], "developer");
        assert_eq!(normalized.items[1]["role"], "user");
    }

    #[test]
    fn normalizes_message_and_tool_payload_shapes() {
        let normalized = normalize(&[
            json!({"role": "user", "content": "hello"}),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": {"q": "x"}
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [{"type": "text", "text": "done"}]
            }),
        ]);

        assert_eq!(normalized.items[0]["type"], "message");
        assert_eq!(normalized.items[0]["content"][0]["type"], "input_text");
        assert_eq!(normalized.items[1]["arguments"], "{\"q\":\"x\"}");
        assert_eq!(normalized.items[2]["output"], "done");
    }

    #[test]
    fn combines_request_and_input_instructions_without_empty_parts() {
        let input_instructions = vec![String::new(), "system prompt".to_owned()];

        assert_eq!(
            combined_instructions(Some("base"), &input_instructions),
            "base\n\nsystem prompt"
        );
        assert_eq!(
            combined_instructions(None, &input_instructions),
            "system prompt"
        );
    }
}
