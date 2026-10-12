use super::*;
use axum::{
    body::Body,
    extract::{RawQuery, WebSocketUpgrade, ws::Message as ServerWebSocketMessage},
    http::{Request, header::CONTENT_TYPE},
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as ClientWebSocketMessage, client::IntoClientRequest},
};
use tower::ServiceExt;

#[derive(Default)]
struct CapturedTts {
    requests: Vec<Value>,
    authorizations: Vec<String>,
    voice_list_authorization: Option<String>,
    websocket_queries: Vec<String>,
    websocket_authorizations: Vec<String>,
    websocket_messages: Vec<Value>,
}

async fn tts_websocket_handler(
    State(captured): State<Arc<Mutex<CapturedTts>>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    websocket: WebSocketUpgrade,
) -> axum::response::Response {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let query = raw_query.unwrap_or_default();
    {
        let mut captured = captured.lock().await;
        captured.websocket_queries.push(query.clone());
        captured.websocket_authorizations.push(authorization);
    }

    if query.contains("language=invalid") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "invalid WebSocket language",
                    "type": "invalid_request_error",
                    "code": "invalid_language"
                }
            })),
        )
            .into_response();
    }

    websocket
        .on_upgrade(move |mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                let ServerWebSocketMessage::Text(text) = message else {
                    if matches!(message, ServerWebSocketMessage::Close(_)) {
                        break;
                    }
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                captured.lock().await.websocket_messages.push(event.clone());
                let response = match event.get("type").and_then(Value::as_str) {
                    Some("session.update") => json!({
                        "type": "session.updated",
                        "replace": event.get("replace").cloned().unwrap_or_else(|| json!({}))
                    }),
                    Some("text.delta") => json!({
                        "type": "audio.delta",
                        "delta": "SUQz",
                        "audio_duration": 0.1,
                        "audio_timestamps": {
                            "graph_chars": ["你"],
                            "graph_times": [[0.0, 0.1]]
                        }
                    }),
                    Some("text.done") => json!({
                        "type": "audio.done",
                        "trace_id": "trace_test"
                    }),
                    Some("text.clear") => json!({"type": "audio.clear"}),
                    _ => json!({
                        "type": "error",
                        "message": "unknown event"
                    }),
                };
                if socket
                    .send(ServerWebSocketMessage::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
        .into_response()
}

async fn spawn_grok_tts_server(captured: Arc<Mutex<CapturedTts>>) -> String {
    async fn tts_handler(
        State(captured): State<Arc<Mutex<CapturedTts>>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let with_timestamps = body
            .get("with_timestamps")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        {
            let mut captured = captured.lock().await;
            captured.requests.push(body);
            captured.authorizations.push(authorization);
        }

        if headers
            .get("x-force-error")
            .and_then(|value| value.to_str().ok())
            == Some("true")
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "invalid language",
                        "type": "invalid_request_error",
                        "code": "invalid_language"
                    }
                })),
            )
                .into_response();
        }

        if with_timestamps {
            let mut response = Json(json!({
                "audio": "SUQz",
                "content_type": "audio/mpeg",
                "duration": 0.4,
                "audio_timestamps": {
                    "graph_chars": ["h", "e", "l", "l", "o"],
                    "graph_times": [
                        [0.0, 0.08],
                        [0.08, 0.16],
                        [0.16, 0.24],
                        [0.24, 0.32],
                        [0.32, 0.4]
                    ]
                }
            }))
            .into_response();
            response
                .headers_mut()
                .insert("x-request-id", HeaderValue::from_static("req_timestamps"));
            return response;
        }

        let mut response = axum::response::Response::new(Body::from("ID3audio"));
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
        response
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static("req_audio"));
        response
    }

    async fn voices_handler(
        State(captured): State<Arc<Mutex<CapturedTts>>>,
        headers: HeaderMap,
    ) -> Json<Value> {
        captured.lock().await.voice_list_authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Json(json!({
            "voices": [
                {"voice_id": "eve", "name": "Eve"},
                {"voice_id": "ara", "name": "Ara"}
            ]
        }))
    }

    let app = Router::new()
        .route("/tts", get(tts_websocket_handler).post(tts_handler))
        .route("/tts/voices", get(voices_handler))
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    url
}

async fn spawn_grok_tts_proxy(state: AppState) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    url
}

fn save_grok_tts_credentials(store: &AuthStore) {
    store
        .save(&Credentials {
            provider: Provider::Grok,
            access_token: "grok_access".into(),
            refresh_token: "grok_refresh".into(),
            expires_at: now_unix() + 600,
            account_id: String::new(),
        })
        .unwrap();
}

fn grok_tts_state(store: AuthStore, base_url: String, api_key: Option<String>) -> AppState {
    let http = Client::new();
    grok_tts_state_with_http(store, base_url, api_key, http)
}

fn grok_tts_state_with_http(
    store: AuthStore,
    base_url: String,
    api_key: Option<String>,
    http: Client,
) -> AppState {
    AppState::new_multi_with_model_fallback(
        vec![UpstreamState {
            provider: Provider::Grok,
            token_manager: TokenManager::new_for_provider(store, Provider::Grok, http.clone()),
            client: CodexClient::new_for_provider_base_url(http, Provider::Grok, base_url),
        }],
        api_key,
        ModelList::from_ids(["grok-4.3"]),
        None,
    )
}

#[tokio::test]
async fn grok_tts_route_preserves_native_request_and_audio_response() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(
        store,
        spawn_grok_tts_server(captured.clone()).await,
        Some("local-secret".into()),
    );
    let body = json!({
        "text": "hello",
        "voice_id": "eve",
        "language": "en",
        "output_format": {"codec": "mp3", "sample_rate": 24000, "bit_rate": 128_000},
        "speed": 1.1,
        "optimize_streaming_latency": 1,
        "replace": {"rotom": "row tom"},
        "future_xai_field": true
    });

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .header("x-api-key", "local-secret")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "audio/mpeg");
    assert_eq!(response.headers()["x-request-id"], "req_audio");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(bytes.as_ref(), b"ID3audio");

    let captured = captured.lock().await;
    assert_eq!(captured.requests, vec![body]);
    assert_eq!(captured.authorizations, vec!["Bearer grok_access"]);
    drop(captured);
}

#[tokio::test]
async fn grok_tts_route_preserves_timestamp_json_response() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(store, spawn_grok_tts_server(captured).await, None);

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"text":"hello","voice_id":"eve","language":"en","with_timestamps":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(response.headers()["x-request-id"], "req_timestamps");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["audio"], "SUQz");
    assert_eq!(value["content_type"], "audio/mpeg");
    assert_eq!(value["audio_timestamps"]["graph_chars"][0], "h");
}

#[tokio::test]
async fn grok_tts_voices_route_uses_native_shape_and_token() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(store, spawn_grok_tts_server(captured.clone()).await, None);

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/v1/tts/voices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["voices"][0]["voice_id"], "eve");
    assert_eq!(
        captured.lock().await.voice_list_authorization.as_deref(),
        Some("Bearer grok_access")
    );
}

#[tokio::test]
async fn grok_tts_route_enforces_local_api_key_before_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(
        store,
        spawn_grok_tts_server(captured.clone()).await,
        Some("local-secret".into()),
    );

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hello","language":"en"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(captured.lock().await.requests, Vec::<Value>::new());
}

#[tokio::test]
async fn grok_tts_route_preserves_upstream_error_shape() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let upstream_url = spawn_grok_tts_server(captured).await;
    let http = Client::builder()
        .default_headers({
            let mut headers = HeaderMap::new();
            headers.insert("x-force-error", HeaderValue::from_static("true"));
            headers
        })
        .build()
        .unwrap();
    let state = AppState::new_multi_with_model_fallback(
        vec![UpstreamState {
            provider: Provider::Grok,
            token_manager: TokenManager::new_for_provider(store, Provider::Grok, http.clone()),
            client: CodexClient::new_for_provider_base_url(http, Provider::Grok, upstream_url),
        }],
        None,
        ModelList::from_ids(["grok-4.3"]),
        None,
    );

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hello","language":"invalid"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["error"]["code"], "invalid_language");
    assert_eq!(value["error"]["message"], "invalid language");
}

#[tokio::test]
async fn grok_tts_websocket_preserves_query_token_and_native_events() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(
        store,
        spawn_grok_tts_server(captured.clone()).await,
        Some("local-secret".into()),
    );
    let proxy_url = spawn_grok_tts_proxy(state).await;
    let query = "language=zh&voice=eve&codec=mp3&with_timestamps=true&future_xai_field=1";
    let mut request = format!("{proxy_url}/v1/tts?{query}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer local-secret"),
    );
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

    let session_update = json!({
        "type": "session.update",
        "replace": {"rotom": "row tom"}
    });
    socket
        .send(ClientWebSocketMessage::Text(
            session_update.to_string().into(),
        ))
        .await
        .unwrap();
    let session_updated = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&session_updated).unwrap(),
        json!({"type": "session.updated", "replace": {"rotom": "row tom"}})
    );

    let text_delta = json!({"type": "text.delta", "delta": "你好"});
    socket
        .send(ClientWebSocketMessage::Text(text_delta.to_string().into()))
        .await
        .unwrap();
    let audio_delta = socket.next().await.unwrap().unwrap().into_text().unwrap();
    let audio_delta = serde_json::from_str::<Value>(&audio_delta).unwrap();
    assert_eq!(audio_delta["type"], "audio.delta");
    assert_eq!(audio_delta["delta"], "SUQz");
    assert_eq!(audio_delta["audio_timestamps"]["graph_chars"][0], "你");

    let text_clear = json!({"type": "text.clear"});
    socket
        .send(ClientWebSocketMessage::Text(text_clear.to_string().into()))
        .await
        .unwrap();
    let audio_clear = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&audio_clear).unwrap(),
        json!({"type": "audio.clear"})
    );

    let replacement_delta = json!({"type": "text.delta", "delta": "重新开始"});
    socket
        .send(ClientWebSocketMessage::Text(
            replacement_delta.to_string().into(),
        ))
        .await
        .unwrap();
    let replacement_audio = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&replacement_audio).unwrap()["type"],
        "audio.delta"
    );

    let text_done = json!({"type": "text.done"});
    socket
        .send(ClientWebSocketMessage::Text(text_done.to_string().into()))
        .await
        .unwrap();
    let audio_done = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&audio_done).unwrap(),
        json!({"type": "audio.done", "trace_id": "trace_test"})
    );
    socket.close(None).await.unwrap();

    let captured = captured.lock().await;
    assert_eq!(captured.websocket_queries, vec![query]);
    assert_eq!(
        captured.websocket_authorizations,
        vec!["Bearer grok_access"]
    );
    assert_eq!(
        captured.websocket_messages,
        vec![
            session_update,
            text_delta,
            text_clear,
            replacement_delta,
            text_done
        ]
    );
    drop(captured);
}

#[tokio::test]
async fn grok_tts_websocket_uses_the_upstream_http_clients_proxy() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let forward_proxy_url = spawn_grok_tts_server(captured.clone()).await;
    let http = Client::builder()
        .proxy(reqwest::Proxy::http(&forward_proxy_url).unwrap())
        .build()
        .unwrap();
    let state = grok_tts_state_with_http(store, "http://unreachable.invalid".into(), None, http);
    let proxy_url = spawn_grok_tts_proxy(state).await;
    let (mut socket, response) = connect_async(format!("{proxy_url}/v1/tts?language=en"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

    socket
        .send(ClientWebSocketMessage::Text(
            json!({"type": "text.delta", "delta": "proxied"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let audio_delta = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&audio_delta).unwrap()["type"],
        "audio.delta"
    );
    socket.close(None).await.unwrap();

    let captured = captured.lock().await;
    assert_eq!(captured.websocket_queries, vec!["language=en"]);
    assert_eq!(
        captured.websocket_authorizations,
        vec!["Bearer grok_access"]
    );
    drop(captured);
}

#[tokio::test]
async fn grok_tts_websocket_enforces_local_api_key_before_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(
        store,
        spawn_grok_tts_server(captured.clone()).await,
        Some("local-secret".into()),
    );
    let proxy_url = spawn_grok_tts_proxy(state).await;

    let error = connect_async(format!("{proxy_url}/v1/tts?language=en"))
        .await
        .unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP handshake error");
    };
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        captured.lock().await.websocket_queries,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn grok_tts_websocket_preserves_upstream_pre_upgrade_error() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_tts_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedTts::default()));
    let state = grok_tts_state(
        store,
        spawn_grok_tts_server(captured.clone()).await,
        Some("local-secret".into()),
    );
    let proxy_url = spawn_grok_tts_proxy(state).await;
    let mut request = format!("{proxy_url}/v1/tts?language=invalid")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer local-secret"),
    );

    let error = connect_async(request).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP handshake error");
    };
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.body().as_deref().unwrap_or_default();
    let value = serde_json::from_slice::<Value>(body).unwrap();
    assert_eq!(value["error"]["code"], "invalid_language");
    assert_eq!(
        captured.lock().await.websocket_authorizations,
        vec!["Bearer grok_access"]
    );
}
