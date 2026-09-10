use super::*;
use serde_json::json;

#[test]
fn converts_anthropic_request_to_openai_shape() {
    let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "system": "be terse",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}
                ]}
            ],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            "tool_choice": "any"
        }))
        .unwrap();

    let converted = to_openai_request(&request).unwrap();
    assert_eq!(converted.messages[0].role, "system");
    assert_eq!(converted.messages[1].role, "user");
    assert_eq!(
        converted.tools.as_ref().unwrap()[0]
            .function
            .as_ref()
            .unwrap()
            .name,
        "lookup"
    );
    assert_eq!(converted.tool_choice, Some(json!("required")));
    assert_eq!(converted.parallel_tool_calls, Some(false));
}

#[test]
fn accepts_system_role_messages_from_tolerant_clients() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "claude-opus-4.8",
        "messages": [
            {
                "role": "system",
                "content": "x-anthropic-billing-header: cc_version=2.1.156;\n\nbe terse"
            },
            {"role": "user", "content": "hello"}
        ]
    }))
    .unwrap();

    let converted = to_openai_request(&request).unwrap();
    assert_eq!(converted.messages[0].role, "system");
    assert_eq!(
        converted.messages[0].content,
        Some(ChatContent::Text("be terse".to_owned()))
    );
    assert_eq!(converted.messages[1].role, "user");
}

#[test]
fn explicit_tool_choice_disables_parallel_tool_calls() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "hello"}],
        "tool_choice": {"type": "tool", "name": "lookup"}
    }))
    .unwrap();

    let converted = to_openai_request(&request).unwrap();
    assert_eq!(converted.parallel_tool_calls, Some(false));
    assert_eq!(
        converted.tool_choice,
        Some(json!({"type": "function", "function": {"name": "lookup"}}))
    );
}

#[test]
fn converts_tool_result_blocks_to_tool_messages() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [{
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "done"}]
        }]
    }))
    .unwrap();

    let converted = to_openai_request(&request).unwrap();
    assert_eq!(converted.messages[0].role, "tool");
    assert_eq!(
        converted.messages[0].tool_call_id.as_deref(),
        Some("call_1")
    );
}

#[test]
fn preserves_user_content_around_tool_results() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "prefix"},
                {"type": "tool_result", "tool_use_id": "call_1", "content": "done"},
                {"type": "text", "text": "suffix"}
            ]
        }]
    }))
    .unwrap();

    let converted = to_openai_request(&request).unwrap();
    assert_eq!(converted.messages.len(), 3);
    assert_eq!(converted.messages[0].role, "user");
    assert_eq!(converted.messages[1].role, "tool");
    assert_eq!(converted.messages[2].role, "user");
}

#[test]
fn supports_input_text_and_document_blocks() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "hello"},
                {"type": "document", "source": {"type": "text", "text": "doc body"}}
            ]
        }]
    }))
    .unwrap();

    let converted = to_openai_request(&request).unwrap();
    let parts = match converted.messages[0].content.as_ref().unwrap() {
        ChatContent::Parts(parts) => parts,
        ChatContent::Text(_) => panic!("expected multimodal parts"),
    };
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].text.as_deref(), Some("hello"));
    assert_eq!(parts[1].text.as_deref(), Some("doc body"));
}

#[test]
fn maps_raw_reasoning_responses_to_thinking_blocks() {
    let response = json!({
        "id": "resp_reasoning",
        "model": "gpt-5.5",
        "status": "completed",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "work"}],
                "encrypted_content": "sig"
            },
            {
                "type": "message",
                "content": [{"type": "output_text", "text": "OK"}]
            }
        ],
        "usage": {"input_tokens": 12, "output_tokens": 5, "total_tokens": 17}
    });

    let mapped = from_openai_response_value(&response, "gpt-5.5");
    assert_eq!(mapped.content.len(), 2);
    assert_eq!(
        serde_json::to_value(&mapped.content[0]).unwrap()["type"],
        "thinking"
    );
    assert_eq!(
        serde_json::to_value(&mapped.content[0]).unwrap()["thinking"],
        "work"
    );
}

#[test]
fn converts_openai_response_to_anthropic_message() {
    let response = ChatCompletionResponse {
        id: "chatcmpl-1".into(),
        object: "chat.completion",
        created: 1,
        model: "gpt-5.5".into(),
        choices: vec![crate::openai::response::ChatChoice {
            index: 0,
            message: crate::openai::response::AssistantMessage {
                role: "assistant",
                content: "hello".into(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: crate::openai::types::FunctionCall {
                        name: "lookup".into(),
                        arguments: "{\"q\":\"x\"}".into(),
                    },
                }]),
                images: None,
            },
            finish_reason: "tool_calls".into(),
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
        }),
    };

    let message = from_openai_response(response);
    assert_eq!(message.stop_reason, "tool_use");
    assert_eq!(message.usage.input_tokens, 10);
    assert_eq!(message.content.len(), 2);
}

#[test]
fn estimates_input_tokens_from_blocks() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}]}]
    }))
    .unwrap();

    assert!(estimate_input_tokens(&request) > 0);
}

#[test]
fn estimates_input_tokens_include_documents_tools_and_thinking() {
    let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "tools": [{"name": "lookup", "description": "fetch docs", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}],
            "messages": [{
                "role": "user",
                "content": [{"type": "document", "source": {"type": "text", "text": "document body"}}]
            }]
        }))
        .unwrap();

    assert!(estimate_input_tokens(&request) > 0);
}

#[test]
fn strips_claude_code_billing_header_from_system_text() {
    let system = SystemPrompt::Text(
        "x-anthropic-billing-header: cc_version=2.1.38; cc_entrypoint=cli; cch=4873d;\n\nbe terse"
            .into(),
    );

    assert_eq!(
        system_prompt_text(Some(&system)).as_deref(),
        Some("be terse")
    );
}

#[test]
fn strips_claude_code_billing_header_blocks_from_system_prompt() {
    let system = SystemPrompt::Blocks(vec![
        SystemBlock {
            kind: "text".into(),
            text: Some(
                "x-anthropic-billing-header: cc_version=2.1.38; cc_entrypoint=cli; cch=4873d;"
                    .into(),
            ),
            cache_control: None,
        },
        SystemBlock {
            kind: "text".into(),
            text: Some("be terse".into()),
            cache_control: None,
        },
    ]);

    assert_eq!(
        system_prompt_text(Some(&system)).as_deref(),
        Some("be terse")
    );
}
