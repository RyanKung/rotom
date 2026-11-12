use crate::{
    config::{AuthStore, Credentials, Provider},
    forward::ForwardConfig,
    openai::response::ModelList,
    server::{AppState, CodexClient, UpstreamState, router},
    testsupport::TempDir,
    token::TokenManager,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use reqwest::Client;
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

#[tokio::test]
async fn forward_service_prefix_falls_back_to_listed_upstream_provider_when_model_not_in_forward() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Vercel,
            access_token: "vercel-token".into(),
            refresh_token: String::new(),
            account_id: "vercel-acct".into(),
            expires_at: i64::MAX,
        })
        .unwrap();
    let http = Client::new();
    let (vercel_url, mut vercel_received) = mock_upstream().await;
    let state = AppState::new_multi_with_forwards(
        vec![UpstreamState {
            provider: Provider::Vercel,
            token_manager: TokenManager::new_for_provider(store, Provider::Vercel, http.clone()),
            client: CodexClient::new(http, format!("{vercel_url}/v1")),
        }],
        Some("local".into()),
        ModelList::from_id_owners([(
            "deepseek/deepseek-v4.1-flash-1017".to_owned(),
            "vercel-ai-gateway",
        )]),
        None,
        BTreeMap::new(),
        BTreeMap::from([(
            "deepseek".into(),
            ForwardConfig {
                base_url: "http://forward.invalid/anthropic".into(),
                api_key: Some("up".into()),
                headers: BTreeMap::new(),
                models: vec!["native".into()],
            },
        )]),
    )
    .unwrap();

    // Unlisted forward model still returns a 400 config error.
    let unknown_response = router(state.clone())
        .oneshot(post(
            "/v1/chat/completions",
            &json!({
                "model": "deepseek/unconfigured",
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(unknown_response.status(), StatusCode::BAD_REQUEST);
    let unknown_body: Value = serde_json::from_slice(
        &to_bytes(unknown_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        unknown_body["error"]["message"],
        "configuration error: forward model deepseek/unconfigured is not configured"
    );

    // Listed Vercel AI Gateway model sharing the `deepseek/` prefix falls through to Vercel.
    let _ = router(state)
        .oneshot(post(
            "/v1/chat/completions",
            &json!({
                "model": "deepseek/deepseek-v4.1-flash-1017",
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    let (upstream_path, upstream_body) = vercel_received.recv().await.unwrap();
    assert_eq!(upstream_path, "/v1/codex/responses");
    assert_eq!(upstream_body["model"], "deepseek/deepseek-v4.1-flash-1017");
}
