use super::*;

fn credentials() -> Credentials {
    Credentials {
        provider: crate::config::Provider::Kiro,
        access_token: "access".to_owned(),
        refresh_token: serde_json::to_string(&json!({
            "kind": "desktop",
            "refresh_token": "refresh",
            "region": "us-east-1",
            "user_agent": "KiroIDE",
            "profile_arn": "arn:aws:codewhisperer:us-east-1:123:profile/test"
        }))
        .unwrap(),
        expires_at: 1,
        account_id: "kiro-desktop:us-east-1".to_owned(),
    }
}

#[test]
fn builds_kiro_payload_from_responses_body() {
    let request = json!({
        "model": "kiro/claude-sonnet-4.5",
        "instructions": "be terse",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }]
    });

    let payload = to_kiro_payload(&request, &credentials()).unwrap();

    assert_eq!(
        payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "claude-sonnet-4.5"
    );
    assert_eq!(
        payload["conversationState"]["currentMessage"]["userInputMessage"]["content"],
        "be terse\n\nhello"
    );
    assert_eq!(
        payload["profileArn"],
        "arn:aws:codewhisperer:us-east-1:123:profile/test"
    );
}

#[test]
fn forwards_native_kiro_model_ids_after_stripping_local_prefixes() {
    for (model, native_id) in [
        ("kiro/gpt-5.6-sol", "gpt-5.6-sol"),
        ("kiro/kimi-k3", "kimi-k3"),
        ("claude-haiku-5.5", "claude-haiku-5.5"),
        ("claude-fable-5.1", "claude-fable-5.1"),
    ] {
        let request = json!({"model": model, "input": "hello"});

        let payload = to_kiro_payload(&request, &credentials()).unwrap();

        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            native_id,
            "model {model} did not retain its native upstream id"
        );
    }
}

#[test]
fn builds_kiro_payload_from_openai_responses_string_input() {
    let request = json!({
        "model": "auto",
        "input": "hello from responses"
    });

    let payload = to_kiro_payload(&request, &credentials()).unwrap();

    assert_eq!(
        payload["conversationState"]["currentMessage"]["userInputMessage"]["content"],
        "hello from responses"
    );
    assert_eq!(
        payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "auto"
    );
}

#[test]
fn builds_kiro_payload_from_openai_chat_history_and_tools() {
    let request = json!({
        "model": "claude-sonnet-4.5",
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "fetch data",
            "parameters": {
                "type": "object",
                "properties": {"q": {"type": "string"}},
                "required": [],
                "additionalProperties": false
            }
        }],
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "find x"}]
            },
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
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "summarize"}]
            }
        ]
    });

    let payload = to_kiro_payload(&request, &credentials()).unwrap();
    let state = &payload["conversationState"];

    assert_eq!(state["history"][0]["userInputMessage"]["content"], "find x");
    assert_eq!(
        state["history"][1]["assistantResponseMessage"]["toolUses"][0]["name"],
        "lookup"
    );
    assert_eq!(
        state["history"][1]["assistantResponseMessage"]["toolUses"][0]["input"],
        json!({"q": "x"})
    );
    assert_eq!(
        state["history"][2]["userInputMessage"]["userInputMessageContext"]["toolResults"][0]["toolUseId"],
        "call_1"
    );
    assert_eq!(
        state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"][0]["toolSpecification"]
            ["inputSchema"]["json"],
        json!({
            "type": "object",
            "properties": {"q": {"type": "string"}}
        })
    );
}

#[test]
fn kiro_payload_uses_only_supported_runtime_fields() {
    let request = json!({
        "model": "claude-sonnet-4.5",
        "temperature": 0.2,
        "top_p": 0.8,
        "max_output_tokens": 128,
        "stop": ["DONE"],
        "reasoning": {"effort": "high"},
        "tool_choice": "none",
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "fetch data",
            "parameters": {"type": "object"}
        }],
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }]
    });

    let payload = to_kiro_payload(&request, &credentials()).unwrap();
    let serialized = serde_json::to_string(&payload).unwrap();

    assert_eq!(
        payload
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["conversationState", "profileArn"]
    );
    assert!(serialized.contains("conversationState"));
    assert!(!serialized.contains("temperature"));
    assert!(!serialized.contains("top_p"));
    assert!(!serialized.contains("max_output_tokens"));
    assert!(!serialized.contains("\"stop\""));
    assert!(!serialized.contains("reasoning"));
    assert!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"]
                .is_null()
        );
}

#[test]
fn maps_inline_images_and_documents_to_kiro_blocks() {
    let request = json!({
        "model": "claude-sonnet-4.5",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "inspect"},
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                {
                    "type": "input_file",
                    "filename": "notes.md",
                    "file_data": "data:text/markdown;base64,IyBOb3Rlcw=="
                }
            ]
        }]
    });

    let payload = to_kiro_payload(&request, &credentials()).unwrap();
    let message = &payload["conversationState"]["currentMessage"]["userInputMessage"];

    assert_eq!(message["images"][0]["format"], "png");
    assert_eq!(message["images"][0]["source"]["bytes"], "aGVsbG8=");
    assert_eq!(message["documents"][0]["name"], "notes");
    assert_eq!(message["documents"][0]["format"], "md");
    assert_eq!(message["documents"][0]["source"]["bytes"], "IyBOb3Rlcw==");
}

#[test]
fn rejects_remote_images_for_kiro_payload() {
    let request = json!({
        "model": "claude-sonnet-4.5",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "inspect"},
                {"type": "input_image", "image_url": "https://example.com/a.png"}
            ]
        }]
    });

    let error = to_kiro_payload(&request, &credentials()).unwrap_err();

    assert!(error.to_string().contains("inline data URLs"));
}

#[test]
fn builds_kiro_payload_from_anthropic_converted_blocks() {
    let request = json!({
        "model": "claude-sonnet-4.5",
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hello "},
                    {"type": "input_text", "text": "claude"}
                ]
            },
            {
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            },
            {
                "role": "user",
                "content": [{"type": "input_text", "text": "continue"}]
            }
        ]
    });

    let payload = to_kiro_payload(&request, &credentials()).unwrap();
    let state = &payload["conversationState"];

    assert_eq!(
        state["history"][0]["userInputMessage"]["content"],
        "hello claude"
    );
    assert_eq!(
        state["history"][1]["assistantResponseMessage"]["content"],
        "hi"
    );
    assert_eq!(
        state["currentMessage"]["userInputMessage"]["content"],
        "continue"
    );
}

#[test]
fn maps_kiro_tool_events_to_responses_function_call() {
    let request = json!({"model": "claude-sonnet-4.5", "input": "use lookup"});
    let mut state = KiroStreamState::new(request);

    assert_eq!(
        state.apply_payload(&json!({"name": "lookup", "input": {}, "toolUseId": "call_1"})),
        Vec::<JsonSseEvent>::new()
    );
    assert_eq!(
        state.apply_payload(&json!({"input": "{\"q\":\"x\"}"})),
        Vec::<JsonSseEvent>::new()
    );
    let events = state.apply_payload(&json!({"stop": true}));

    let done = events
        .iter()
        .find(|event| {
            event.value.get("type").and_then(Value::as_str) == Some("response.output_item.done")
        })
        .unwrap();
    assert_eq!(done.value["item"]["type"], "function_call");
    assert_eq!(done.value["item"]["call_id"], "call_1");
    assert_eq!(done.value["item"]["name"], "lookup");
    assert_eq!(done.value["item"]["arguments"], "{\"q\":\"x\"}");
}

#[test]
fn maps_wrapped_kiro_reasoning_events() {
    let request = json!({"model": "claude-sonnet-4.5", "input": "think"});
    let mut state = KiroStreamState::new(request);

    let events = state.apply_payload(&json!({
        "reasoningContentEvent": {"text": "step", "signature": "sig"}
    }));
    let final_events = state.finish_events();

    assert!(events.iter().any(|event| {
        event.value.get("type").and_then(Value::as_str) == Some("response.reasoning_text.delta")
            && event.value.get("delta").and_then(Value::as_str) == Some("step")
    }));
    let completed = final_events
        .iter()
        .find(|event| event.value.get("type").and_then(Value::as_str) == Some("response.completed"))
        .unwrap();
    assert_eq!(
        completed.value["response"]["output"][0]["type"],
        "reasoning"
    );
    assert_eq!(
        completed.value["response"]["output"][0]["encrypted_content"],
        "sig"
    );
}

#[test]
fn decodes_aws_event_stream_frame() {
    let payload = br#"{"content":"OK","modelId":"auto"}"#;
    let total_len = 12 + payload.len() + 4;
    let mut frame = Vec::new();
    frame.extend_from_slice(&u32::try_from(total_len).unwrap().to_be_bytes());
    frame.extend_from_slice(&0_u32.to_be_bytes());
    frame.extend_from_slice(&0_u32.to_be_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&0_u32.to_be_bytes());
    let mut decoder = AwsEventStreamDecoder::default();

    let events = decoder.push_chunk(&Bytes::from(frame)).unwrap();

    assert_eq!(events, vec![json!({"content": "OK", "modelId": "auto"})]);
}
