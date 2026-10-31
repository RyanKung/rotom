use crate::{
    forward::ForwardConfig,
    openai::response::ModelList,
    server::{AppState, router},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::{net::TcpListener, sync::mpsc};
use tower::ServiceExt;

async fn capture(
    State(tx): State<mpsc::UnboundedSender<(String, Value)>>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let payload = serde_json::from_slice(&to_bytes(body, usize::MAX).await.unwrap()).unwrap();
    tx.send((parts.uri.to_string(), payload)).unwrap();
    (StatusCode::OK, "ok").into_response()
}

async fn mock_upstream() -> (String, mpsc::UnboundedReceiver<(String, Value)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(capture).with_state(tx))
            .await
            .unwrap();
    });
    (url, rx)
}

fn state(base_url: String) -> AppState {
    AppState::new_multi_with_forwards(
        Vec::new(),
        Some("local".into()),
        ModelList::from_ids(Vec::<String>::new()),
        None,
        BTreeMap::from([("anthropic/alias".into(), "deepseek/native".into())]),
        BTreeMap::from([(
            "deepseek".into(),
            ForwardConfig {
                base_url,
                api_key: Some("up".into()),
                headers: BTreeMap::new(),
                models: vec!["native".into()],
            },
        )]),
    )
    .unwrap()
}

fn post(path: &str, payload: &Value) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", "Bearer local")
        .body(Body::from(serde_json::to_vec(payload).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn alias_rewrites_model_and_joins_path() {
    let (url, mut received) = mock_upstream().await;
    let payload = json!({"model": "anthropic/alias", "keep": true});
    let response = router(state(format!("{url}/anthropic")))
        .oneshot(post("/v1/messages?x=1", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (path, body) = received.recv().await.unwrap();
    assert_eq!(path, "/anthropic/v1/messages?x=1");
    assert_eq!(body["model"], "native");
    assert_eq!(body["keep"], true);
}

#[tokio::test]
async fn models_list_includes_forward_alias() {
    let request = Request::builder()
        .uri("/v1/models")
        .header("x-api-key", "local")
        .body(Body::empty())
        .unwrap();
    let body = to_bytes(
        router(state("http://upstream.invalid/anthropic".into()))
            .oneshot(request)
            .await
            .unwrap()
            .into_body(),
        usize::MAX,
    )
    .await
    .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let ids: Vec<_> = value["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"deepseek/native"));
    assert!(ids.contains(&"anthropic/alias"));
}
