//! Regression coverage for interleaved partial calls and Responses item identity.

use super::*;
use crate::{
    config::Provider,
    openai::response::{chunk_finished, chunk_with_content, chunk_with_tool_call},
    server::{handlers::sse::openai_responses_sse, store::ResponseStore},
};
use axum::{body::to_bytes, response::IntoResponse};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

/// Builds one chat tool fragment with optional metadata represented by empty fields.
fn tool_fragment(
    index: u32,
    id: &str,
    name: &str,
    arguments: &str,
) -> crate::openai::response::ChatCompletionChunk {
    chunk_with_tool_call(
        "chat_1",
        1,
        "test-model",
        index,
        ToolCall {
            id: id.to_owned(),
            kind: "function".to_owned(),
            function: FunctionCall {
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
        },
    )
}

/// Extracts all JSON event payloads from an axum SSE body.
fn event_values(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

/// Interleaved calls and text retain identical lifecycle and stored output identities.
#[tokio::test]
async fn interleaved_partial_calls_keep_one_lifecycle_and_stored_identity() {
    let chunks = vec![
        tool_fragment(7, "call_first", "write_file", ""),
        tool_fragment(7, "", "", "{\"x\":"),
        chunk_with_content("chat_1", 1, "test-model", "working".to_owned()),
        tool_fragment(3, "call_second", "lookup", "{\"q\":"),
        tool_fragment(7, "", "", "1}"),
        tool_fragment(3, "", "", "2}"),
        chunk_finished("chat_1", 1, "test-model", "tool_calls"),
    ];
    let store = ResponseStore::default();
    let response = openai_responses_sse(
        Box::pin(stream::iter(chunks.into_iter().map(Ok))),
        "resp_partial".to_owned(),
        serde_json::from_value(json!({
            "model": "test-model", "input": "go", "stream": true, "store": true
        }))
        .unwrap(),
        Vec::new(),
        store.clone(),
        Provider::Codex,
    )
    .into_response();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let events = event_values(&body);
    let added: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.added")
        .collect();
    let done: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .collect();
    let delta_count = events
        .iter()
        .filter(|event| event["type"] == "response.function_call_arguments.delta")
        .count();
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let output = completed["response"]["output"].as_array().unwrap();

    assert_eq!(added.len(), 3);
    assert_eq!(done.len(), 3);
    assert_eq!(delta_count, 4);
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["call_id"], "call_first");
    assert_eq!(output[0]["arguments"], "{\"x\":1}");
    assert_eq!(output[1]["content"][0]["text"], "working");
    assert_eq!(output[2]["call_id"], "call_second");
    assert_eq!(output[2]["arguments"], "{\"q\":2}");
    for (index, item) in output.iter().enumerate() {
        let opened = added
            .iter()
            .find(|event| event["item"]["id"] == item["id"])
            .unwrap();
        let closed = done
            .iter()
            .find(|event| event["item"]["id"] == item["id"])
            .unwrap();
        assert_eq!(opened["output_index"], index);
        assert_eq!(closed["output_index"], index);
        assert_eq!(&closed["item"], item);
    }
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence);
    }
    let stored = store.get("resp_partial").await.unwrap();
    assert_eq!(
        serde_json::to_value(stored.response).unwrap(),
        completed["response"]
    );
}

/// A pending terminal cannot withhold an already available argument fragment.
#[tokio::test]
async fn argument_fragments_are_forwarded_before_chat_finishes() {
    let chunks = vec![
        tool_fragment(0, "call_1", "write_file", ""),
        tool_fragment(0, "", "", "{\"content\":\"growing"),
    ];
    let stream = stream::iter(chunks.into_iter().map(Ok)).chain(stream::pending());
    let response = openai_responses_sse(
        Box::pin(stream),
        "resp_pending".to_owned(),
        serde_json::from_value(json!({"model": "test-model", "input": "go", "stream": true}))
            .unwrap(),
        Vec::new(),
        ResponseStore::default(),
        Provider::Codex,
    )
    .into_response();
    let mut body = response.into_body().into_data_stream();
    let mut received = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(bytes) = body.next().await {
            received.extend_from_slice(&bytes.unwrap());
            if event_values(&received)
                .iter()
                .any(|event| event["type"] == "response.function_call_arguments.delta")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let events = event_values(&received);
    assert!(
        events
            .iter()
            .any(|event| event["delta"] == "{\"content\":\"growing")
    );
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "response.function_call_arguments.done")
    );
}
