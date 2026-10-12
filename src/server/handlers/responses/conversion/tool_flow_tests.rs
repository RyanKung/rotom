//! Regression tests for Anthropic tool selection and tool result conversion.

use super::*;
use serde_json::json;

#[test]
fn anthropic_tool_choice_objects_map_to_codex_modes_and_parallel_policy() {
    let cases = [
        (json!({"type": "auto"}), json!("auto"), true),
        (
            json!({"type": "auto", "disable_parallel_tool_use": true}),
            json!("auto"),
            false,
        ),
        (json!({"type": "any"}), json!("required"), true),
        (
            json!({"type": "any", "disable_parallel_tool_use": true}),
            json!("required"),
            false,
        ),
        (json!({"type": "none"}), json!("none"), false),
        (
            json!({"type": "tool", "name": "lookup"}),
            json!({"type": "function", "name": "lookup"}),
            true,
        ),
        (
            json!({"type": "tool", "name": "lookup", "disable_parallel_tool_use": true}),
            json!({"type": "function", "name": "lookup"}),
            false,
        ),
    ];

    for (choice, expected_choice, expected_parallel) in cases {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            "tool_choice": choice
        }))
        .unwrap();
        let converted = anthropic_responses_request(&request).unwrap();
        let input_items = collect_response_input_items(&converted, None).unwrap();
        let body = crate::codex::convert::responses_to_upstream_request(
            crate::config::Provider::Codex,
            &converted,
            &input_items,
        )
        .unwrap();

        assert_eq!(body["tool_choice"], expected_choice);
        assert_eq!(body["parallel_tool_calls"], expected_parallel);
    }
}

#[test]
fn anthropic_error_tool_results_use_supported_codex_output_shape() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5.5",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {}},
                {"type": "tool_use", "id": "call_2", "name": "lookup", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "No such file", "is_error": true},
                {"type": "tool_result", "tool_use_id": "call_2", "content": "found", "is_error": false}
            ]}
        ]
    }))
    .unwrap();
    let converted = anthropic_responses_request(&request).unwrap();
    let input_items = collect_response_input_items(&converted, None).unwrap();
    let body = crate::codex::convert::responses_to_upstream_request(
        crate::config::Provider::Codex,
        &converted,
        &input_items,
    )
    .unwrap();

    assert_eq!(body["input"][2]["output"], "Error: No such file");
    assert_eq!(body["input"][3]["output"], "found");
    assert!(body["input"][2].get("is_error").is_none());
    assert!(body["input"][3].get("is_error").is_none());
}
