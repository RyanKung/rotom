//! HTTP boundary regressions for incremental Responses-to-Chat tool calls.

use super::*;
use axum::{body::Body, body::BodyDataStream, http::Request};
use futures_util::StreamExt;
use std::convert::Infallible;
use tokio::{sync::Notify, task::JoinHandle, time::timeout};
use tower::ServiceExt;

/// A scripted Responses stream whose final events require explicit permission.
struct GatedUpstream {
    /// Events available while the test is observing incremental Chat chunks.
    prefix: Vec<Value>,
    /// Completion snapshots withheld until the test releases the gate.
    suffix: Vec<Value>,
    /// A retained notification prevents completion before the test allows it.
    release: Arc<Notify>,
}

/// An open Chat response together with the resources that keep its mock alive.
struct ChatStream {
    /// Downstream body polled a frame at a time instead of collected eagerly.
    body: BodyDataStream,
    /// Bytes not yet consumed as a complete SSE frame.
    pending: Vec<u8>,
    /// Permission to emit the upstream completion snapshots.
    release: Arc<Notify>,
    /// Server task aborted on success or on a failed assertion.
    upstream: JoinHandle<()>,
    /// Temporary credentials retained for the entire request lifetime.
    _credentials: TempDir,
}

impl Drop for ChatStream {
    /// Prevents failed tests from leaving a gated mock server running.
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

impl ChatStream {
    /// Reads one data frame without depending on HTTP body chunk boundaries.
    async fn next_data(&mut self) -> String {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(end) = self.pending.windows(2).position(|bytes| bytes == b"\n\n") {
                    let frame = String::from_utf8(self.pending.drain(..end + 2).collect()).unwrap();
                    if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
                        return data.to_owned();
                    }
                    continue;
                }
                let bytes = self
                    .body
                    .next()
                    .await
                    .expect("Chat SSE ended early")
                    .unwrap();
                self.pending.extend_from_slice(&bytes);
            }
        })
        .await
        .expect("Chat SSE frame was withheld until upstream completion")
    }

    /// Collects JSON chunks through the protocol's terminal sentinel.
    async fn remaining_chunks(&mut self) -> Vec<Value> {
        let mut chunks = Vec::new();
        loop {
            let data = self.next_data().await;
            if data == "[DONE]" {
                return chunks;
            }
            chunks.push(serde_json::from_str(&data).unwrap());
        }
    }
}

/// Serves incremental events immediately and gates all completion snapshots.
async fn gated_responses(State(script): State<Arc<GatedUpstream>>) -> impl IntoResponse {
    let stream = async_stream::stream! {
        for event in &script.prefix {
            yield Ok::<_, Infallible>(format!("data: {event}\n\n"));
        }
        script.release.notified().await;
        for event in &script.suffix {
            yield Ok::<_, Infallible>(format!("data: {event}\n\n"));
        }
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
}

/// Opens the public Chat route with local credentials and a gated HTTP upstream.
async fn open_chat_stream(prefix: Vec<Value>, suffix: Vec<Value>) -> ChatStream {
    let release = Arc::new(Notify::new());
    let script = Arc::new(GatedUpstream {
        prefix,
        suffix,
        release: Arc::clone(&release),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/codex/responses", post(gated_responses))
        .with_state(script);
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let credentials = TempDir::new().unwrap();
    let store = AuthStore::new(credentials.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Codex,
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "test-account".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), "http://token.invalid"),
        ),
        CodexClient::new(http, base_url),
        None,
        ModelList::from_ids(["gpt-5.5"]),
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "gpt-5.5",
                "stream": true,
                "messages": [{"role": "user", "content": "Run the requested tools."}],
                "tools": [{"type": "function", "function": {
                    "name": "lookup", "parameters": {"type": "object"}
                }}]
            })
            .to_string(),
        ))
        .unwrap();
    let response = router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    ChatStream {
        body: response.into_body().into_data_stream(),
        pending: Vec::new(),
        release,
        upstream,
        _credentials: credentials,
    }
}

/// Describes a function-call item using distinct item and public call identities.
fn call_item(item_id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call", "id": item_id, "call_id": call_id,
        "name": name, "arguments": arguments
    })
}

/// Associates an item snapshot with its Responses output position.
fn item_event(kind: &str, output_index: usize, item: &Value) -> Value {
    json!({"type": kind, "output_index": output_index, "item": item})
}

/// Builds one argument fragment without repeating the function-call metadata.
fn argument_event(output_index: usize, item_id: &str, delta: &str) -> Value {
    json!({
        "type": "response.function_call_arguments.delta", "output_index": output_index,
        "item_id": item_id, "delta": delta
    })
}

/// Creates the authoritative completed output that must not replay sent bytes.
fn completed_event(output: &[Value]) -> Value {
    json!({"type": "response.completed", "response": {
        "id": "resp_tool_test", "status": "completed", "model": "gpt-5.5",
        "output": output, "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
    }})
}

/// Extracts all tool deltas without discarding role, finish, or usage chunks.
fn tool_deltas(chunks: &[Value]) -> Vec<&Value> {
    chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"].as_array())
        .flatten()
        .collect()
}

/// Verifies the first fragment identifies its call and later fragments omit metadata.
fn assert_call_fragments(deltas: &[&Value], index: usize, call_id: &str, name: &str, args: &str) {
    assert_ne!(deltas, [] as [&Value; 0]);
    assert_eq!(deltas[0]["id"], call_id);
    assert_eq!(deltas[0]["type"], "function");
    assert_eq!(deltas[0]["function"]["name"], name);
    let mut reconstructed = String::new();
    for (position, delta) in deltas.iter().enumerate() {
        assert_eq!(delta["index"], index);
        if position > 0 {
            assert!(delta.get("id").is_none(), "continuation repeats call id");
            assert!(
                delta.get("type").is_none(),
                "continuation repeats call type"
            );
            assert!(
                delta["function"].get("name").is_none(),
                "continuation repeats function name"
            );
        }
        reconstructed.push_str(delta["function"]["arguments"].as_str().unwrap());
    }
    assert_eq!(reconstructed, args);
}

/// Confirms the tool finish reason follows all emitted call fragments.
fn assert_tool_finish(chunks: &[Value]) {
    let finish = chunks
        .iter()
        .position(|chunk| chunk["choices"][0]["finish_reason"] == "tool_calls")
        .expect("missing tool_calls finish reason");
    assert_eq!(tool_deltas(&chunks[finish..]), Vec::<&Value>::new());
}

/// The first call and both fragments arrive while completion remains impossible.
#[tokio::test]
async fn chat_tool_arguments_arrive_before_upstream_completion() {
    let arguments = "{\"path\":\"src/你好.rs\"}";
    let completed = call_item("fc_1", "call_1", "lookup", arguments);
    let mut stream = open_chat_stream(
        vec![
            item_event(
                "response.output_item.added",
                0,
                &call_item("fc_1", "call_1", "lookup", ""),
            ),
            argument_event(0, "fc_1", "{\"path\":\"src/"),
            argument_event(0, "fc_1", "你好.rs\"}"),
        ],
        vec![
            item_event("response.output_item.done", 0, &completed),
            completed_event(&[completed]),
        ],
    )
    .await;
    let mut early_chunks = Vec::new();
    while tool_deltas(&early_chunks).len() < 3 {
        let data = stream.next_data().await;
        assert_ne!(data, "[DONE]");
        early_chunks.push(serde_json::from_str(&data).unwrap());
    }
    let deltas = tool_deltas(&early_chunks);
    assert_eq!(deltas[0]["function"]["arguments"], "");
    assert_eq!(deltas[1]["function"]["arguments"], "{\"path\":\"src/");
    assert_eq!(deltas[2]["function"]["arguments"], "你好.rs\"}");
    assert_call_fragments(&deltas, 0, "call_1", "lookup", arguments);

    // Causality witness: no upstream completion event exists before this signal.
    stream.release.notify_one();
    let remaining = stream.remaining_chunks().await;
    assert!(
        tool_deltas(&remaining).is_empty(),
        "completion replayed tool arguments"
    );
    assert_tool_finish(&remaining);
}

/// Interleaving preserves compact Chat indexes despite non-tool output positions.
#[tokio::test]
async fn chat_tool_indexes_stay_compact_for_interleaved_sparse_outputs() {
    let first = call_item("fc_first", "call_first", "lookup", "{\"a\":1}");
    let second = call_item("fc_second", "call_second", "lookup", "{\"b\":2}");
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": []});
    let message = json!({"type": "message", "id": "msg_1", "content": []});
    let mut stream = open_chat_stream(
        vec![
            item_event("response.output_item.added", 0, &reasoning),
            item_event("response.output_item.added", 1, &message),
            item_event(
                "response.output_item.added",
                2,
                &call_item("fc_first", "call_first", "lookup", ""),
            ),
            item_event(
                "response.output_item.added",
                5,
                &call_item("fc_second", "call_second", "lookup", ""),
            ),
            argument_event(2, "fc_first", "{\"a\":"),
            argument_event(5, "fc_second", "{\"b\":"),
            argument_event(2, "fc_first", "1}"),
            argument_event(5, "fc_second", "2}"),
        ],
        vec![
            item_event("response.output_item.done", 5, &second),
            item_event("response.output_item.done", 2, &first),
            completed_event(&[
                reasoning,
                message,
                first,
                json!({"type": "reasoning"}),
                json!({"type": "message", "content": []}),
                second,
            ]),
        ],
    )
    .await;
    stream.release.notify_one();
    let chunks = stream.remaining_chunks().await;
    let deltas = tool_deltas(&chunks);
    let indexes: Vec<_> = deltas
        .iter()
        .map(|delta| delta["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indexes, vec![0, 1, 0, 1, 0, 1]);
    for (index, call_id, arguments) in [
        (0, "call_first", "{\"a\":1}"),
        (1, "call_second", "{\"b\":2}"),
    ] {
        let fragments: Vec<_> = deltas
            .iter()
            .copied()
            .filter(|delta| delta["index"] == index)
            .collect();
        assert_call_fragments(&fragments, index, call_id, "lookup", arguments);
    }
    assert_tool_finish(&chunks);
}

/// Providers that emit only a final response still produce a complete Chat call.
#[tokio::test]
async fn chat_tool_completed_only_snapshot_remains_supported() {
    let mut stream = open_chat_stream(
        Vec::new(),
        vec![completed_event(&[
            json!({"type": "reasoning", "id": "rs_1", "summary": []}),
            call_item(
                "fc_fallback",
                "call_fallback",
                "lookup",
                "{\"q\":\"fallback\"}",
            ),
        ])],
    )
    .await;
    stream.release.notify_one();
    let chunks = stream.remaining_chunks().await;
    let deltas = tool_deltas(&chunks);
    assert_eq!(deltas.len(), 1);
    assert_call_fragments(
        &deltas,
        0,
        "call_fallback",
        "lookup",
        "{\"q\":\"fallback\"}",
    );
    assert_tool_finish(&chunks);
}
