use super::*;
use crate::{
    Error,
    config::{AuthStore, Credentials, Provider, now_unix},
    oauth::CodexOAuthClient,
    openai::response::ModelList,
    testsupport::TempDir,
};
use axum::{
    Json,
    body::to_bytes,
    extract::{Form, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::HOST},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

fn jwt_with_account_id(account_id: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
        }))
        .unwrap(),
    );
    format!("header.{encoded}.sig")
}

#[test]
fn model_support_normalization_accepts_provider_prefixes() {
    assert_eq!(
        normalize_model_for_support("kiro/claude-sonnet-4.5"),
        "claude-sonnet-4.5"
    );
    assert_eq!(normalize_model_for_support("grok/grok-4.3"), "grok-4.3");
    assert_eq!(
        normalize_model_for_support("openai-codex/gpt-5.5"),
        "gpt-5.5"
    );
    assert_eq!(
        normalize_model_for_support("vercel/openai/gpt-6-astra"),
        "openai/gpt-6-astra"
    );
}

#[test]
fn removed_cursor_models_cannot_fall_back_to_another_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let http = Client::new();
    let state = AppState::new_with_model_fallback(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), "http://token.invalid"),
        ),
        CodexClient::new(http, "http://codex.invalid"),
        None,
        ModelList::from_ids(["gpt-5.5"]),
        Some("gpt-5.5".into()),
    );
    let mut model = "cursor/sonnet-4".to_owned();

    state.rewrite_model(&mut model);

    assert_eq!(model, "cursor/sonnet-4");
    assert!(state.upstream_for_model(&model).is_none());
}

async fn refresh_handler(Form(form): Form<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(form.get("refresh_token").unwrap(), "old_refresh");
    Json(json!({
        "access_token": jwt_with_account_id("acc_refreshed"),
        "refresh_token": "new_refresh",
        "expires_in": 3600
    }))
}

async fn spawn_refresh_server() -> String {
    let app = Router::new().route("/token", post(refresh_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn account_handler() -> Json<Value> {
    Json(json!({
        "accounts": {
            "default": {
                "account": {
                    "name": "Personal",
                    "structure": "personal"
                },
                "entitlement": {
                    "subscription_plan": "chatgptplus",
                    "has_active_subscription": true,
                    "expires_at": "2026-05-01T00:00:00Z"
                }
            }
        }
    }))
}

async fn usage_handler() -> Json<Value> {
    Json(json!({
        "email": "test@example.com",
        "plan_type": "pro",
        "rate_limit": {
            "primary_window": {
                "used_percent": 10,
                "reset_at": "2026-04-27T12:00:00Z"
            },
            "secondary_window": {
                "remaining_percent": 90,
                "reset_at": "2026-05-01T00:00:00Z"
            }
        },
        "credits": { "balance": 1 }
    }))
}

async fn spawn_status_server() -> String {
    let app = Router::new()
        .route("/accounts/check/v4-2023-04-27", get(account_handler))
        .route("/wham/usage", get(usage_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn codex_complete_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
    )
}

async fn codex_tool_call_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n",
    )
}

async fn codex_image_stream_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_upstream\",\"model\":\"gpt-5.5\",\"output\":[]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_upstream\",\"model\":\"gpt-5.5\",\"output\":[{\"type\":\"image_generation_call\",\"id\":\"ig_1\",\"result\":\"YWJj\",\"output_format\":\"png\",\"revised_prompt\":\"refined prompt\"}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
        ),
    )
}

async fn codex_tool_stream_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_1\",\"delta\":\"{\\\"q\\\":\\\"x\\\"}\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_tool_stream\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n"
        ),
    )
}

async fn codex_reasoning_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_reasoning\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"work\"}],\"encrypted_content\":\"sig\"},{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
    )
}

async fn codex_reasoning_stream_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: response.reasoning_text.delta\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"output_index\":0,\"item_id\":\"rs_1\",\"content_index\":0,\"delta\":\"step\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"step\"}],\"encrypted_content\":\"sig\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"OK\"}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":1,\"item_id\":\"msg_1\",\"content_index\":0,\"text\":\"OK\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_reasoning_stream\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"step\"}],\"encrypted_content\":\"sig\"},{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
        ),
    )
}

async fn codex_cache_usage_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200,\"output_tokens\":5,\"total_tokens\":317,\"server_tool_use\":{\"web_search_requests\":1}}}}\n\n",
    )
}

async fn codex_cache_usage_stream_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"OK\"}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"text\":\"OK\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_cache\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200,\"output_tokens\":5,\"total_tokens\":317,\"server_tool_use\":{\"web_search_requests\":1}}}}\n\n"
        ),
    )
}

async fn codex_null_output_stream_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"OK\"}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"text\":\"OK\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_null_output\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
        ),
    )
}

async fn codex_incomplete_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_incomplete\",\"model\":\"gpt-5.5\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
    )
}

async fn codex_incomplete_result_stream_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"OK\"}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"text\":\"OK\"}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_incomplete_result\",\"model\":\"gpt-5.5\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"incomplete_result\"},\"output\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
        ),
    )
}

async fn codex_bad_request_handler() -> impl axum::response::IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "detail": "The 'claude-opus-4-7' model is not supported when using Codex with a ChatGPT account."
        })),
    )
}

async fn spawn_codex_server(tool_call: bool) -> String {
    let app = if tool_call {
        Router::new().route("/codex/responses", post(codex_tool_call_handler))
    } else {
        Router::new().route("/codex/responses", post(codex_complete_handler))
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_image_stream_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_image_stream_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_tool_stream_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_tool_stream_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_incomplete_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_incomplete_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_incomplete_result_stream_codex_server() -> String {
    let app = Router::new().route(
        "/codex/responses",
        post(codex_incomplete_result_stream_handler),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_reasoning_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_reasoning_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_reasoning_stream_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_reasoning_stream_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_bad_request_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_bad_request_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_cache_usage_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_cache_usage_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_cache_usage_stream_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_cache_usage_stream_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_null_output_stream_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(codex_null_output_stream_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn delayed_codex_complete_handler() -> impl axum::response::IntoResponse {
    sleep(Duration::from_millis(100)).await;
    codex_complete_handler().await
}

async fn spawn_delayed_codex_server() -> String {
    let app = Router::new().route("/codex/responses", post(delayed_codex_complete_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_recording_codex_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
    async fn recording_handler(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        captured.lock().await.push(body);
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
        )
    }

    let app = Router::new()
        .route("/codex/responses", post(recording_handler))
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_strict_recording_codex_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
    async fn strict_recording_handler(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        if let Some(path) = find_forbidden_key_path(&body, "cache_control") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "detail": format!("Unknown parameter: '{path}'.")
                })),
            );
        }
        if let Some(path) = find_forbidden_key_path(&body, "max_output_tokens") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "detail": format!("Unsupported parameter: {path}")
                })),
            );
        }

        captured.lock().await.push(body);
        (
            StatusCode::OK,
            Json(json!({
                "id": "resp_strict",
                "model": "gpt-5.5",
                "output": [{"type":"message","content":[{"type":"output_text","text":"OK"}]}],
                "usage": {"input_tokens":12,"output_tokens":5,"total_tokens":17}
            })),
        )
    }

    let app = Router::new()
        .route("/codex/responses", post(strict_recording_handler))
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_stream_required_codex_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
    async fn stream_required_handler(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        captured.lock().await.push(body.clone());
        if body.get("stream").and_then(Value::as_bool) != Some(true) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "detail": "Stream must be set to true"
                })),
            )
                .into_response();
        }

        (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream_required\",\"model\":\"gpt-5.5\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
            )
                .into_response()
    }

    let app = Router::new()
        .route("/codex/responses", post(stream_required_handler))
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

async fn spawn_grok_response_resource_server(captured: Arc<Mutex<Vec<String>>>) -> String {
    async fn retrieve_handler(
        State(captured): State<Arc<Mutex<Vec<String>>>>,
        Path(response_id): Path<String>,
    ) -> Json<Value> {
        captured.lock().await.push(format!("GET {response_id}"));
        Json(json!({
            "id": response_id,
            "object": "response",
            "status": "completed",
            "model": "grok-4.3",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "upstream"}]}]
        }))
    }

    async fn delete_handler(
        State(captured): State<Arc<Mutex<Vec<String>>>>,
        Path(response_id): Path<String>,
    ) -> Json<Value> {
        captured.lock().await.push(format!("DELETE {response_id}"));
        Json(json!({
            "id": response_id,
            "object": "response",
            "deleted": true
        }))
    }

    let app = Router::new()
        .route(
            "/responses/{response_id}",
            get(retrieve_handler).delete(delete_handler),
        )
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

fn assert_stream_events_in_order(stream: &str, events: &[&str]) {
    let mut cursor = 0;
    for event in events {
        let offset = stream[cursor..]
            .find(event)
            .unwrap_or_else(|| panic!("missing stream event {event} after byte {cursor}"));
        cursor += offset + event.len();
    }
}

fn sse_frame<'a>(stream: &'a str, event: &str) -> &'a str {
    let start = stream
        .find(event)
        .unwrap_or_else(|| panic!("missing stream event {event}"));
    let end = stream[start..]
        .find("\n\n")
        .map_or(stream.len(), |offset| start + offset);
    &stream[start..end]
}

fn find_forbidden_key_path(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(object) => {
            if object.contains_key(key) {
                return Some(key.to_owned());
            }

            object.iter().find_map(|(name, nested)| {
                find_forbidden_key_path(nested, key).map(|suffix| {
                    if suffix.starts_with('[') {
                        format!("{name}{suffix}")
                    } else {
                        format!("{name}.{suffix}")
                    }
                })
            })
        }
        Value::Array(array) => array.iter().enumerate().find_map(|(index, nested)| {
            find_forbidden_key_path(nested, key).map(|suffix| {
                if suffix.starts_with('[') {
                    format!("[{index}]{suffix}")
                } else {
                    format!("[{index}].{suffix}")
                }
            })
        }),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn test_state(store: AuthStore, token_url: String, api_key: Option<String>) -> AppState {
    let http = Client::new();
    AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), token_url),
        ),
        CodexClient::new(http, "http://codex.invalid"),
        api_key,
        ModelList::from_ids(["gpt-test"]),
    )
}

async fn wait_for_batch_to_finish(state: AppState, headers: HeaderMap, batch_id: String) -> Value {
    for _ in 0..50 {
        let response = handlers::get_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Path(batch_id.clone()),
        )
        .await;
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        if value["processing_status"] == "ended" {
            return value;
        }
        sleep(Duration::from_millis(20)).await;
    }

    panic!("batch did not finish in time");
}

mod auth_status_messages;
mod batches_and_replay;
mod responses_provider_strategy;
mod responses_resources;
mod responses_streaming;
mod tts;
