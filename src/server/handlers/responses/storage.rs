use crate::{
    config::Provider,
    openai::response::{
        GeneratedImage, ResponseObject, Usage, response_function_call_item,
        response_image_generation_item, response_message_item, response_reasoning_item,
    },
    openai::types::ResponsesRequest,
    server::AppState,
};
use serde_json::{Value, json};

pub(in crate::server::handlers) fn response_object_from_chat(
    request: &ResponsesRequest,
    response: crate::openai::response::ChatCompletionResponse,
) -> ResponseObject {
    let choice = response.choices.into_iter().next().unwrap_or_else(|| {
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
    });
    let output = build_responses_output_items(
        &response.id,
        &choice.message.content,
        choice.message.tool_calls.unwrap_or_default(),
        choice.message.images.unwrap_or_default(),
    );

    ResponseObject {
        id: response.id.replace("chatcmpl", "resp"),
        object: "response",
        created_at: response.created,
        status: "completed".to_owned(),
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: response.model,
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        top_p: request.top_p,
        text: response_text_config(request),
        tool_choice: request.tool_choice.clone(),
        tools: request
            .tools
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tool| serde_json::to_value(tool).ok())
            .collect(),
        truncation: response_truncation(request),
        usage: response.usage,
        user: response_user(request),
        metadata: request.metadata.clone(),
        reasoning: response_reasoning(request),
        previous_response_id: request.previous_response_id.clone(),
    }
}

pub(in crate::server::handlers) fn response_object_from_upstream(
    request: &ResponsesRequest,
    response: &Value,
) -> ResponseObject {
    let created_at = response
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(crate::config::now_unix);
    let output = response_output_items_from_upstream(
        response
            .get("output")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice),
    );

    ResponseObject {
        id: response
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(build_response_id, str::to_owned),
        object: "response",
        created_at,
        status: response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_owned(),
        error: response
            .get("error")
            .filter(|value| !value.is_null())
            .cloned(),
        incomplete_details: response
            .get("incomplete_details")
            .filter(|value| !value.is_null())
            .cloned(),
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .to_owned(),
        output,
        parallel_tool_calls: response
            .get("parallel_tool_calls")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| request.parallel_tool_calls()),
        store: response
            .get("store")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| request.should_store()),
        temperature: response
            .get("temperature")
            .and_then(Value::as_f64)
            .or(request.temperature),
        top_p: response
            .get("top_p")
            .and_then(Value::as_f64)
            .or(request.top_p),
        text: response
            .get("text")
            .cloned()
            .unwrap_or_else(|| response_text_config(request)),
        tool_choice: response
            .get("tool_choice")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| request.tool_choice.clone()),
        tools: response
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| response_tool_values(request.tools.clone())),
        truncation: response
            .get("truncation")
            .and_then(Value::as_str)
            .map_or_else(|| response_truncation(request), str::to_owned),
        usage: parse_upstream_usage(response.get("usage")),
        user: response
            .get("user")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| response_user(request)),
        metadata: response
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| request.metadata.clone()),
        reasoning: response
            .get("reasoning")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| response_reasoning(request)),
        previous_response_id: request.previous_response_id.clone(),
    }
}

pub(in crate::server::handlers) async fn maybe_store_response(
    state: &AppState,
    request: &ResponsesRequest,
    response: ResponseObject,
    input_items: Vec<Value>,
    provider: Provider,
    upstream_resource: bool,
) {
    if request.should_store() {
        state
            .responses
            .insert(crate::server::store::StoredResponse {
                input_items: stored_response_input_items(input_items, &response),
                response,
                provider,
                upstream_resource,
            })
            .await;
    }
}

pub(in crate::server::handlers) fn stored_response_input_items(
    mut input_items: Vec<Value>,
    response: &ResponseObject,
) -> Vec<Value> {
    input_items.extend(response_output_to_input_items(&response.output));
    input_items
}

pub(in crate::server::handlers) fn build_response_id() -> String {
    format!(
        "resp_{}_{:08x}",
        crate::config::now_unix(),
        rand::random::<u32>()
    )
}

pub(in crate::server::handlers) fn build_responses_output_items(
    response_id: &str,
    output_text: &str,
    tool_calls: Vec<crate::openai::types::ToolCall>,
    images: Vec<GeneratedImage>,
) -> Vec<crate::openai::response::ResponseOutputItem> {
    let has_non_message_output = !tool_calls.is_empty() || !images.is_empty();
    let mut output = Vec::new();
    if !output_text.is_empty() || !has_non_message_output {
        output.push(response_message_item(
            format!("msg_{response_id}"),
            Some(output_text.to_owned()),
        ));
    }
    for (index, tool_call) in tool_calls.into_iter().enumerate() {
        output.push(response_function_call_item(
            format!("fc_{response_id}_{index}"),
            tool_call,
        ));
    }
    for (index, image) in images.into_iter().enumerate() {
        output.push(response_image_generation_item(
            format!("ig_{response_id}_{index}"),
            image.b64_json,
            image.revised_prompt,
        ));
    }
    output
}

fn response_tool_values(tools: Option<Vec<crate::openai::types::ChatTool>>) -> Vec<Value> {
    tools
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tool| serde_json::to_value(tool).ok())
        .collect()
}

fn response_output_items_from_upstream(
    items: &[Value],
) -> Vec<crate::openai::response::ResponseOutputItem> {
    let mut output = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
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
                output.push(response_message_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("msg_{index}"), str::to_owned),
                    Some(text),
                ));
            }
            Some("function_call") => {
                let tool_call = crate::openai::types::ToolCall {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    kind: "function".to_owned(),
                    function: crate::openai::types::FunctionCall {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned(),
                    },
                };
                output.push(response_function_call_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("fc_{index}"), str::to_owned),
                    tool_call,
                ));
            }
            Some("image_generation_call") => {
                if let Some(image) = crate::openai::response::generated_image_from_item(item) {
                    output.push(response_image_generation_item(
                        item.get("id")
                            .and_then(Value::as_str)
                            .map_or_else(|| format!("ig_{index}"), str::to_owned),
                        image.b64_json,
                        image.revised_prompt,
                    ));
                }
            }
            Some("reasoning") => {
                output.push(response_reasoning_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("rs_{index}"), str::to_owned),
                    item.get("summary").cloned(),
                    item.get("encrypted_content")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ));
            }
            _ => {}
        }
    }
    output
}

fn response_output_to_input_items(
    items: &[crate::openai::response::ResponseOutputItem],
) -> Vec<Value> {
    let mut output = Vec::new();
    for item in items {
        match item.kind.as_str() {
            "message" => {
                let content = item
                    .content
                    .iter()
                    .map(|part| {
                        json!({
                            "type": "output_text",
                            "text": part.text,
                            "annotations": part.annotations,
                        })
                    })
                    .collect::<Vec<_>>();
                output.push(json!({
                    "type": "message",
                    "role": item.role.unwrap_or("assistant"),
                    "status": item.status,
                    "id": item.id,
                    "content": content,
                }));
            }
            "function_call" => output.push(json!({
                "type": "function_call",
                "id": item.id,
                "call_id": item.call_id,
                "name": item.name,
                "arguments": item.arguments,
            })),
            "image_generation_call" => output.push(json!({
                "type": "image_generation_call",
                "id": item.id,
                "result": item.result,
                "revised_prompt": item.revised_prompt,
            })),
            "reasoning" => {
                if let Some(encrypted_content) = item.encrypted_content.as_ref() {
                    let mut reasoning = json!({
                        "type": "reasoning",
                        "id": item.id,
                        "encrypted_content": encrypted_content,
                    });
                    if let Some(summary) = item.summary.as_ref() {
                        reasoning["summary"] = summary.clone();
                    }
                    output.push(reasoning);
                }
            }
            _ => {}
        }
    }
    output
}

pub(in crate::server::handlers) fn parse_upstream_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let prompt_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let cached_tokens = value
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| value.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());

    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        prompt_tokens_details: cached_tokens
            .map(|cached_tokens| crate::openai::response::PromptTokensDetails { cached_tokens }),
    })
}

pub(in crate::server::handlers) fn response_object(
    request: &ResponsesRequest,
    response_id: String,
    created_at: i64,
    status: &str,
    output: Vec<crate::openai::response::ResponseOutputItem>,
    usage: Option<Usage>,
) -> ResponseObject {
    ResponseObject {
        id: response_id,
        object: "response",
        created_at,
        status: status.to_owned(),
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: request.model.clone(),
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        top_p: request.top_p,
        text: response_text_config(request),
        tool_choice: request.tool_choice.clone(),
        tools: response_tool_values(request.tools.clone()),
        truncation: response_truncation(request),
        usage,
        user: response_user(request),
        metadata: request.metadata.clone(),
        reasoning: response_reasoning(request),
        previous_response_id: request.previous_response_id.clone(),
    }
}

fn response_text_config(request: &ResponsesRequest) -> Value {
    request
        .extra
        .get("text")
        .cloned()
        .unwrap_or_else(|| json!({"format": {"type": "text"}}))
}

fn response_truncation(request: &ResponsesRequest) -> String {
    request
        .extra
        .get("truncation")
        .and_then(Value::as_str)
        .unwrap_or("disabled")
        .to_owned()
}

fn response_user(request: &ResponsesRequest) -> Option<String> {
    request
        .extra
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn response_reasoning(request: &ResponsesRequest) -> Option<Value> {
    request
        .reasoning
        .clone()
        .or_else(|| Some(json!({"effort": null, "summary": null})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_object_from_upstream_preserves_status_and_incomplete_details() {
        let request = ResponsesRequest {
            model: "gpt-5.5".to_owned(),
            input: None,
            instructions: Some("Be terse.".to_owned()),
            stream: Some(false),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            service_tier: None,
            reasoning: None,
            max_output_tokens: Some(128),
            parallel_tool_calls: Some(false),
            store: Some(false),
            previous_response_id: None,
            metadata: None,
            extra: serde_json::Map::new(),
        };
        let response = json!({
            "id": "resp_incomplete",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "model": "gpt-5.5",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "partial"}]
            }]
        });

        let mapped = response_object_from_upstream(&request, &response);

        assert_eq!(mapped.status, "incomplete");
        assert_eq!(
            mapped
                .incomplete_details
                .as_ref()
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str),
            Some("max_output_tokens")
        );
        assert_eq!(mapped.output[0].kind, "message");
    }

    #[test]
    fn response_object_from_upstream_preserves_reasoning_items() {
        let request = ResponsesRequest {
            model: "gpt-5.5".to_owned(),
            input: None,
            instructions: None,
            stream: Some(false),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            service_tier: None,
            reasoning: None,
            max_output_tokens: None,
            parallel_tool_calls: None,
            store: Some(false),
            previous_response_id: None,
            metadata: None,
            extra: serde_json::Map::new(),
        };
        let response = json!({
            "id": "resp_reasoning",
            "status": "completed",
            "model": "gpt-5.5",
            "parallel_tool_calls": false,
            "store": false,
            "temperature": 0.5,
            "top_p": 0.9,
            "text": {"format": {"type": "json_object"}},
            "tool_choice": "required",
            "tools": [{"type": "function", "name": "lookup"}],
            "truncation": "auto",
            "user": "user_1",
            "metadata": {"trace": "abc"},
            "output": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "work"}],
                "encrypted_content": "sig"
            }]
        });

        let mapped = response_object_from_upstream(&request, &response);

        assert!(!mapped.parallel_tool_calls);
        assert!(!mapped.store);
        assert_eq!(mapped.temperature, Some(0.5));
        assert_eq!(mapped.top_p, Some(0.9));
        assert_eq!(mapped.text, json!({"format": {"type": "json_object"}}));
        assert_eq!(mapped.tool_choice, Some(json!("required")));
        assert_eq!(
            mapped.tools,
            vec![json!({"type": "function", "name": "lookup"})]
        );
        assert_eq!(mapped.truncation, "auto");
        assert_eq!(mapped.user.as_deref(), Some("user_1"));
        assert_eq!(mapped.metadata.as_ref().unwrap()["trace"], "abc");
        assert_eq!(mapped.output[0].kind, "reasoning");
        assert_eq!(
            mapped.output[0].summary.as_ref().unwrap(),
            &json!([{"type": "summary_text", "text": "work"}])
        );
        assert_eq!(mapped.output[0].encrypted_content.as_deref(), Some("sig"));
    }

    #[test]
    fn stored_response_items_replay_reasoning_only_with_encrypted_content() {
        let response = ResponseObject {
            id: "resp_reasoning".to_owned(),
            object: "response",
            created_at: 1,
            status: "completed".to_owned(),
            error: None,
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            model: "gpt-5.5".to_owned(),
            output: vec![response_reasoning_item(
                "rs_1".to_owned(),
                Some(json!([{"type": "summary_text", "text": "work"}])),
                Some("sig".to_owned()),
            )],
            parallel_tool_calls: true,
            store: true,
            temperature: None,
            top_p: None,
            text: json!({"format": {"type": "text"}}),
            tool_choice: None,
            tools: Vec::new(),
            truncation: "disabled".to_owned(),
            usage: None,
            user: None,
            metadata: None,
            reasoning: Some(json!({"effort": null, "summary": null})),
            previous_response_id: None,
        };

        let stored = stored_response_input_items(Vec::new(), &response);

        assert_eq!(stored[0]["type"], "reasoning");
        assert_eq!(stored[0]["id"], "rs_1");
        assert_eq!(stored[0]["encrypted_content"], "sig");
        assert_eq!(
            stored[0]["summary"],
            json!([{"type": "summary_text", "text": "work"}])
        );
    }
}
