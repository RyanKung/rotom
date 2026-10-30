use crate::{
    forward::ForwardConfig,
    openai::response::ModelList,
    server::{AppState, router},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc},
    time::timeout,
};
use tower::ServiceExt;

struct Observed {
    path: String,
    headers: HeaderMap,
    payload: Value,
}

#[derive(Clone)]
struct MockState {
    observed: mpsc::UnboundedSender<Observed>,
    release_stream: Arc<Notify>,
}

async fn capture(State(state): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap();
    let payload = serde_json::from_slice::<Value>(&body).unwrap();
    let mode = payload
        .get("test_mode")
        .and_then(Value::as_str)
        .unwrap_or("");
    let response = match mode {
        "stream" => {
            let stream = async_stream::stream! {
                yield Ok::<_, Infallible>("event: custom\ndata: {\"delta\":\"first\"}\n\n");
                state.release_stream.notified().await;
                yield Ok::<_, Infallible>("event: custom_end\ndata: {\"provider_only\":true}\n\n");
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .header("x-request-id", "stream-123")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        "error" => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .header("retry-after", "7")
            .header("x-request-id", "error-123")
            .header("connection", "x-private-hop")
            .header("x-private-hop", "remove-me")
            .body(Body::from("{\"vendor_error\": \"busy\", \"code\":42901}"))
            .unwrap(),
        "redirect" => Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("location", "/must-not-follow")
            .body(Body::from("redirect body"))
            .unwrap(),
        _ => (StatusCode::CREATED, "provider response bytes").into_response(),
    };
    state
        .observed
        .send(Observed {
            path: parts.uri.to_string(),
            headers: parts.headers,
            payload,
        })
        .unwrap();
    response
}

async fn mock_upstream() -> (String, mpsc::UnboundedReceiver<Observed>, Arc<Notify>) {
    let (observed, receiver) = mpsc::unbounded_channel();
    let release_stream = Arc::new(Notify::new());
    let state = MockState {
        observed,
        release_stream: release_stream.clone(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(capture).with_state(state))
            .await
            .unwrap();
    });
    (url, receiver, release_stream)
}

fn forward_state(
    base_url: String,
    upstream_key: Option<&str>,
    headers: BTreeMap<String, String>,
) -> AppState {
    AppState::new_multi_with_forwards(
        Vec::new(),
        Some("local-secret".into()),
        ModelList::from_ids(Vec::<String>::new()),
        Some("unused-fallback".into()),
        BTreeMap::from([("anthropic/my-alias".into(), "deepseek/native-model".into())]),
        BTreeMap::from([(
            "deepseek".into(),
            ForwardConfig {
                base_url,
                api_key: upstream_key.map(str::to_owned),
                headers,
                models: vec!["native-model".into()],
            },
        )]),
    )
    .unwrap()
}

fn request(path: &str, payload: &Value) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", "Bearer local-secret")
        .header("x-api-key", "local-secret")
        .header("cookie", "local_session=secret")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "custom-feature")
        .header("connection", "x-local-hop, Keep-Alive")
        .header("x-local-hop", "remove-me")
        .body(Body::from(serde_json::to_vec(payload).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn forward_alias_preserves_payload_path_query_and_upstream_error() {
    let (url, mut received, _) = mock_upstream().await;
    let state = forward_state(
        format!("{url}/anthropic"),
        Some("upstream-secret"),
        BTreeMap::from([("x-api-key".into(), "configured-secret".into())]),
    );
    let payload = json!({
        "model": "anthropic/my-alias", "test_mode": "error", "max_tokens": 19,
        "messages": [{"role":"user","content":[{"type":"vendor_special","value":[1,2,3]}]}],
        "thinking": {"type":"adaptive", "unknown_vendor_field":true},
        "tools": [{"name":"tool", "vendor_extension":{"a":null}}],
        "unknown_top_level": {"deep": [false, "保留"]}
    });
    let response = router(state)
        .oneshot(request("/v1/messages?beta=true&tag=a%2Fb", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get("retry-after").unwrap(), "7");
    assert_eq!(response.headers().get("x-request-id").unwrap(), "error-123");
    assert!(!response.headers().contains_key("connection"));
    assert!(!response.headers().contains_key("x-private-hop"));
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "{\"vendor_error\": \"busy\", \"code\":42901}"
    );
    let observed = received.recv().await.unwrap();
    assert_eq!(observed.path, "/anthropic/v1/messages?beta=true&tag=a%2Fb");
    let mut expected = payload;
    expected["model"] = json!("native-model");
    assert_eq!(observed.payload, expected);
    assert_eq!(
        observed.headers.get("authorization").unwrap(),
        "Bearer upstream-secret"
    );
    assert_eq!(
        observed.headers.get("x-api-key").unwrap(),
        "configured-secret"
    );
    assert_eq!(
        observed.headers.get("anthropic-version").unwrap(),
        "2023-06-01"
    );
    assert_eq!(
        observed.headers.get("anthropic-beta").unwrap(),
        "custom-feature"
    );
    assert!(!observed.headers.contains_key("cookie"));
    assert!(!observed.headers.contains_key("x-local-hop"));
}

#[tokio::test]
async fn forward_preserves_each_requested_protocol_and_accepts_custom_model_endpoints() {
    let (url, mut received, _) = mock_upstream().await;
    let state = forward_state(format!("{url}/v1/"), None, BTreeMap::new());
    for path in [
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/chat/completions",
        "/v1/responses",
        "/v1/custom-vendor-operation",
    ] {
        let payload = json!({"model":"deepseek/native-model", "vendor_only":{"raw":true}});
        let response = router(state.clone())
            .oneshot(request(path, &payload))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "{path}");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "provider response bytes"
        );
        let observed = received.recv().await.unwrap();
        assert_eq!(observed.path, path);
        assert_eq!(
            observed.payload,
            json!({"model":"native-model", "vendor_only":{"raw":true}})
        );
        assert!(!observed.headers.contains_key("authorization"));
        assert!(!observed.headers.contains_key("x-api-key"));
        assert!(!observed.headers.contains_key("cookie"));
    }
}

#[tokio::test]
async fn forward_sse_reaches_client_before_upstream_finishes_without_rewriting_events() {
    let (url, mut received, release) = mock_upstream().await;
    let state = forward_state(url, Some("upstream-secret"), BTreeMap::new());
    let response = timeout(
        Duration::from_secs(2),
        router(state).oneshot(request(
            "/v1/messages",
            &json!({"model":"deepseek/native-model", "stream":true,"test_mode":"stream"}),
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "stream-123"
    );
    let mut stream = response.into_body().into_data_stream();
    let first = timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first, "event: custom\ndata: {\"delta\":\"first\"}\n\n");
    release.notify_one();
    let second = timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        second,
        "event: custom_end\ndata: {\"provider_only\":true}\n\n"
    );
    assert!(stream.next().await.is_none());
    assert_eq!(
        received.recv().await.unwrap().payload.get("model").unwrap(),
        "native-model"
    );
}

#[tokio::test]
async fn forward_rejects_unauthorized_and_undeclared_models_without_contacting_upstream() {
    let (url, mut received, _) = mock_upstream().await;
    let state = forward_state(url, Some("upstream-secret"), BTreeMap::new());
    let mut unauthorized = request("/v1/messages", &json!({"model":"deepseek/native-model"}));
    unauthorized.headers_mut().remove("authorization");
    unauthorized.headers_mut().remove("x-api-key");
    let response = router(state.clone()).oneshot(unauthorized).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router(state.clone())
        .oneshot(request(
            "/v1/messages",
            &json!({"model":"deepseek/unlisted", "messages":[]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(received.try_recv().is_err());
}

#[tokio::test]
async fn forward_redirect_is_returned_without_following_it() {
    let (url, mut received, _) = mock_upstream().await;
    let state = forward_state(url, Some("upstream-secret"), BTreeMap::new());
    let response = router(state)
        .oneshot(request(
            "/v1/messages",
            &json!({"model":"deepseek/native-model","test_mode":"redirect"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/must-not-follow"
    );
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "redirect body"
    );
    assert_eq!(received.recv().await.unwrap().path, "/v1/messages");
    assert!(received.try_recv().is_err());
}

#[tokio::test]
async fn forward_models_and_aliases_are_discoverable_in_both_api_formats() {
    let state = forward_state(
        "http://upstream.invalid/anthropic".into(),
        None,
        BTreeMap::new(),
    );
    for anthropic in [false, true] {
        let mut request = Request::builder()
            .uri("/v1/models")
            .header("x-api-key", "local-secret");
        if anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        let ids = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"deepseek/native-model"));
        assert!(ids.contains(&"anthropic/my-alias"));
    }
}
