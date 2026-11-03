//! Lossless adaptation to Antigravity's protobuf Schema dialect.
use serde_json::{Map, Value, json};

#[derive(Debug, thiserror::Error)]
pub(super) enum SchemaError {
    #[error("Gemini schema must be an object")]
    NotObject,
    #[error("Gemini requires a resolvable local schema reference without sibling constraints")]
    InvalidReference,
    #[error("Gemini cannot represent recursive or excessively nested schemas")]
    Recursive,
    #[error("Gemini schema type must be a string or an array of strings")]
    InvalidType,
    #[error("Antigravity response schemas support string enums only")]
    InvalidEnum,
}

pub(super) fn convert(schema: &Value) -> Result<Value, SchemaError> {
    expand(schema, schema, 0)
}

// External schema retrieval is disabled in Cargo features: validation stays local.
pub(super) fn validator(body: &Value) -> crate::Result<Option<jsonschema::Validator>> {
    let Some(format) = body.pointer("/text/format") else {
        return Ok(None);
    };
    if format.get("type").and_then(Value::as_str) == Some("json_object") {
        return jsonschema::validator_for(&json!({"type":"object"}))
            .map(Some)
            .map_err(|_| crate::Error::config("invalid Gemini JSON object validator"));
    }
    if format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return Ok(None);
    }
    let schema = format
        .get("schema")
        .ok_or_else(|| crate::Error::config("missing Gemini output schema"))?;
    let validator = jsonschema::validator_for(schema).map_err(|_| {
        crate::Error::config("invalid or externally referenced Gemini output schema")
    })?;
    Ok((format.get("strict").and_then(Value::as_bool) != Some(false)).then_some(validator))
}

pub(super) fn validate_response(
    validator: &jsonschema::Validator,
    response: &Value,
) -> crate::Result<()> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| crate::Error::upstream("Gemini structured response has no output"))?;
    // Tool turns are not final structured answers; preserve normal tool execution.
    if output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
    {
        return Ok(());
    }
    let text: String = output
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    let valid = serde_json::from_str::<Value>(&text).is_ok_and(|value| validator.is_valid(&value));
    if response.get("status").and_then(Value::as_str) != Some("completed") || !valid {
        return Err(crate::Error::upstream(
            "Gemini output failed strict JSON Schema validation",
        ));
    }
    Ok(())
}

fn expand(value: &Value, root: &Value, depth: usize) -> Result<Value, SchemaError> {
    if depth > 64 {
        return Err(SchemaError::Recursive);
    }
    let object = value.as_object().ok_or(SchemaError::NotObject)?;
    if let Some(reference) = object.get("$ref") {
        if object.len() != 1 {
            return Err(SchemaError::InvalidReference);
        }
        let target = reference
            .as_str()
            .and_then(|r| r.strip_prefix('#'))
            .filter(|r| r.starts_with('/'))
            .and_then(|r| root.pointer(r))
            .ok_or(SchemaError::InvalidReference)?;
        return expand(target, root, depth + 1);
    }
    let mut result = Map::new();
    for (key, value) in object {
        let value = match key.as_str() {
            "$defs" | "definitions" => continue,
            "properties" => {
                let properties = value.as_object().ok_or(SchemaError::NotObject)?;
                Value::Object(
                    properties
                        .iter()
                        .map(|(name, schema)| Ok((name.clone(), expand(schema, root, depth + 1)?)))
                        .collect::<Result<Map<_, _>, SchemaError>>()?,
                )
            }
            "items" => expand(value, root, depth + 1)?,
            "additionalProperties" if value.is_object() => expand(value, root, depth + 1)?,
            "anyOf" => Value::Array(
                value
                    .as_array()
                    .ok_or(SchemaError::NotObject)?
                    .iter()
                    .map(|v| expand(v, root, depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            "type" if value.is_array() => {
                let types = value.as_array().ok_or(SchemaError::InvalidType)?;
                if types.len() == 2 && types.iter().any(|v| v == "null") {
                    result.insert("nullable".into(), json!(true));
                    types
                        .iter()
                        .find(|v| *v != "null")
                        .filter(|v| v.is_string())
                        .ok_or(SchemaError::InvalidType)?
                        .clone()
                } else {
                    return Err(SchemaError::InvalidType);
                }
            }
            "enum" => {
                if !value
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string))
                {
                    return Err(SchemaError::InvalidEnum);
                }
                value.clone()
            }
            _ => value.clone(),
        };
        result.insert(key.clone(), value);
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_mode_validates_arbitrary_objects_without_constraining_properties() {
        let validator = validator(&json!({"text":{"format":{"type":"json_object"}}}))
            .unwrap()
            .unwrap();
        assert!(validator.is_valid(&json!({"ok":true,"nested":[1,2]})));
        assert!(!validator.is_valid(&json!([])));
        assert!(!validator.is_valid(&json!("text")));
    }

    #[test]
    fn local_references_expand_and_nullable_types_preserve_constraints() {
        let schema = json!({"type":"object","properties":{"value":{"$ref":"#/$defs/value"}},
            "required":["value"],"additionalProperties":false,
            "$defs":{"value":{"type":["string","null"],"description":"optional value"}}});
        assert_eq!(
            convert(&schema).unwrap(),
            json!({"type":"object",
            "properties":{"value":{"type":"string","nullable":true,"description":"optional value"}},
            "required":["value"],"additionalProperties":false})
        );
    }

    #[test]
    fn unrepresentable_schemas_fail_without_weakening_the_output_contract() {
        for schema in [
            json!(true),
            json!({"$ref":"https://example.com/schema"}),
            json!({"$ref":"#/$defs/absent"}),
            json!({"$defs":{"loop":{"$ref":"#/$defs/loop"}},"properties":{"x":{"$ref":"#/$defs/loop"}}}),
            json!({"$defs":{"x":{}},"properties":{"x":{"$ref":"#/$defs/x","enum":["only"]}}}),
            json!({"type":"integer","enum":[7]}),
            json!({"type":["string","integer"]}),
        ] {
            assert!(convert(&schema).is_err(), "{schema}");
        }
    }
}
