use super::*;

async fn effort_test_state(model: &str, captured: Arc<Mutex<Vec<Value>>>) -> (TempDir, AppState) {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Codex,
            access_token: jwt_with_account_id("acc_effort_test"),
            refresh_token: "unused_test_refresh".into(),
            expires_at: now_unix() + 3600,
            account_id: "acc_effort_test".into(),
        })
        .unwrap();
    let http = Client::builder().no_proxy().build().unwrap();
    let state = AppState::new_with_model_fallback(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), "http://token.invalid"),
        ),
        CodexClient::new(http, spawn_recording_codex_server(captured).await),
        None,
        ModelList::from_ids([model]),
        Some(model.into()),
    );
    (dir, state)
}

async fn send_effort(state: AppState, effort: &str, stream: bool) {
    let request = serde_json::from_value(json!({
        "model": "claude-sol",
        "max_tokens": 32,
        "stream": stream,
        "messages": [{"role": "user", "content": "Reply OK"}],
        "output_config": {"effort": effort}
    }))
    .unwrap();
    let response = handlers::messages(State(state), HeaderMap::new(), Json(request)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn gpt6_effort_survives_alias_routing_and_stream_conversion() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (_dir, state) = effort_test_state("gpt-6.1-sol", captured.clone()).await;
    let levels = ["low", "medium", "high", "xhigh", "max"];
    for stream in [false, true] {
        for effort in levels {
            send_effort(state.clone(), effort, stream).await;
        }
    }
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 10);
    for (request, expected) in requests.iter().zip(levels.into_iter().cycle()) {
        assert_eq!(request["model"], "gpt-6.1-sol");
        assert_eq!(request["reasoning"]["effort"], expected);
    }
}

#[tokio::test]
async fn gpt6_effort_fix_preserves_legacy_max_mapping_for_older_models() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (_dir, state) = effort_test_state("gpt-5.5", captured.clone()).await;
    send_effort(state, "max", false).await;
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "gpt-5.5");
    assert_eq!(requests[0]["reasoning"]["effort"], "xhigh");
}
