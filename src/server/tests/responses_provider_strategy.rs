use super::*;

async fn spawn_grok_native_responses_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
    async fn native_handler(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let index = {
            let mut captured = captured.lock().await;
            captured.push(body.clone());
            captured.len()
        };
        Json(json!({
            "id": format!("resp_upstream_native_{index}"),
            "object": "response",
            "status": "completed",
            "model": body.get("model").cloned().unwrap_or_else(|| json!("grok-4.3")),
            "store": body.get("store").and_then(Value::as_bool).unwrap_or(true),
            "output": [{
                "id": format!("msg_upstream_native_{index}"),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "native"}]
            }],
            "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
        }))
    }

    async fn delete_handler(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Path(response_id): Path<String>,
    ) -> Json<Value> {
        captured
            .lock()
            .await
            .push(json!({"method": "DELETE", "id": response_id}));
        Json(json!({
            "id": response_id,
            "object": "response",
            "deleted": true
        }))
    }

    let app = Router::new()
        .route("/responses", post(native_handler))
        .route(
            "/responses/{response_id}",
            axum::routing::delete(delete_handler),
        )
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

fn grok_state(
    store: AuthStore,
    base_url: String,
    models: impl IntoIterator<Item = &'static str>,
) -> AppState {
    let http = Client::new();
    AppState::new_multi_with_model_fallback(
        vec![UpstreamState {
            provider: Provider::Grok,
            token_manager: TokenManager::new_for_provider(store, Provider::Grok, http.clone()),
            client: CodexClient::new_for_provider_base_url(http, Provider::Grok, base_url),
        }],
        None,
        ModelList::from_id_owners(models.into_iter().map(|id| (id.to_owned(), "xai"))),
        None,
    )
}

fn save_grok_credentials(store: &AuthStore) {
    store
        .save(&Credentials {
            provider: Provider::Grok,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: String::new(),
        })
        .unwrap();
}

#[tokio::test]
async fn grok_responses_use_native_upstream_ids() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_credentials(&store);
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let state = grok_state(
        store,
        spawn_grok_native_responses_server(captured.clone()).await,
        ["grok-4.3"],
    );

    let response = handlers::responses(
        axum::extract::State(state),
        HeaderMap::new(),
        Json(
            serde_json::from_value(json!({
                "model": "grok-4.3",
                "input": "hello",
                "stream": false
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["id"], "resp_upstream_native_1");
    assert_eq!(value["output"][0]["content"][0]["text"], "native");

    let (store, stream, has_previous_response_id) = {
        let captured = captured.lock().await;
        (
            captured[0]["store"].clone(),
            captured[0]["stream"].clone(),
            captured[0].get("previous_response_id").is_some(),
        )
    };
    assert_eq!(store, true);
    assert_eq!(stream, false);
    assert!(!has_previous_response_id);
}

#[tokio::test]
async fn prefixed_grok_model_is_advertised_and_forwarded_without_prefix() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_credentials(&store);
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let state = grok_state(
        store,
        spawn_grok_native_responses_server(captured.clone()).await,
        ["xai/grok-4.3"],
    );

    let listed = handlers::models(axum::extract::State(state.clone()), HeaderMap::new()).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
    let listed_json = serde_json::from_slice::<Value>(&listed_body).unwrap();
    assert_eq!(listed_json["data"][0]["id"], "xai/grok-4.3");

    let response = handlers::responses(
        axum::extract::State(state),
        HeaderMap::new(),
        Json(
            serde_json::from_value(json!({
                "model": "xai/grok-4.3",
                "input": "hello",
                "stream": false
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(captured.lock().await[0]["model"], "grok-4.3");
}

#[tokio::test]
async fn grok_continuation_uses_upstream_previous_response_id() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_credentials(&store);
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let state = grok_state(
        store,
        spawn_grok_native_responses_server(captured.clone()).await,
        ["grok-4.3"],
    );

    let first = handlers::responses(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Json(serde_json::from_value(json!({"model": "grok-4.3", "input": "first"})).unwrap()),
    )
    .await;
    let first_body = to_bytes(first.into_body(), usize::MAX).await.unwrap();
    let first_value = serde_json::from_slice::<Value>(&first_body).unwrap();
    let first_id = first_value["id"].as_str().unwrap();

    let second = handlers::responses(
        axum::extract::State(state),
        HeaderMap::new(),
        Json(
            serde_json::from_value(json!({
                "model": "grok-4.3",
                "previous_response_id": first_id,
                "input": "second"
            }))
            .unwrap(),
        ),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);

    let (captured_len, previous_response_id, input_len, input_text, has_instructions) = {
        let captured = captured.lock().await;
        let input = captured[1]["input"].as_array().unwrap();
        (
            captured.len(),
            captured[1]["previous_response_id"].clone(),
            input.len(),
            input[0]["content"][0]["text"].clone(),
            captured[1].get("instructions").is_some(),
        )
    };
    assert_eq!(captured_len, 2);
    assert_eq!(previous_response_id, first_id);
    assert_eq!(input_len, 1);
    assert_eq!(input_text, "second");
    assert!(!has_instructions);
}

#[tokio::test]
async fn grok_delete_for_native_response_deletes_upstream_resource() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_grok_credentials(&store);
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let state = grok_state(
        store,
        spawn_grok_native_responses_server(captured.clone()).await,
        ["grok-4.3"],
    );

    let created = handlers::responses(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Json(serde_json::from_value(json!({"model": "grok-4.3", "input": "hello"})).unwrap()),
    )
    .await;
    let created_body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let created_value = serde_json::from_slice::<Value>(&created_body).unwrap();
    let response_id = created_value["id"].as_str().unwrap().to_owned();

    let deleted = handlers::delete_response(
        axum::extract::State(state),
        HeaderMap::new(),
        axum::extract::Path(response_id.clone()),
    )
    .await;

    assert_eq!(deleted.status(), StatusCode::OK);
    let body = to_bytes(deleted.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["id"], response_id);
    assert_eq!(value["deleted"], true);

    let last = captured.lock().await.last().unwrap().clone();
    assert_eq!(last["method"], "DELETE");
    assert_eq!(last["id"], response_id);
}

#[tokio::test]
async fn codex_responses_keep_historical_chat_compatibility_path() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
        None,
        ModelList::from_ids(["gpt-5.5"]),
    );

    let response = handlers::responses(
        axum::extract::State(state),
        HeaderMap::new(),
        Json(serde_json::from_value(json!({"model": "gpt-5.5", "input": "hello"})).unwrap()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert!(value["id"].as_str().unwrap().starts_with("resp-"));

    let (store, stream) = {
        let captured = captured.lock().await;
        (captured[0]["store"].clone(), captured[0]["stream"].clone())
    };
    assert_eq!(store, false);
    assert_eq!(stream, true);
}
