use super::*;

async fn gemini_state(store: AuthStore, captured: Arc<Mutex<Vec<Value>>>) -> AppState {
    async fn upstream(
        State(captured): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        let is_followup = body
            .pointer("/request/contents")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts
                                .iter()
                                .any(|part| part.get("functionResponse").is_some())
                        })
                })
            });
        captured.lock().await.push(body);
        let parts = if is_followup {
            json!([{"text":"done"}])
        } else {
            json!([{"text":"checking"},{"functionCall":{"name":"lookup","args":{"key":"a"}},"thoughtSignature":"signed"}])
        };
        (
            [("content-type", "text/event-stream")],
            format!(
                "data: {}\n\n",
                json!({"response":{"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]}})
            ),
        )
    }
    store
        .save(&Credentials {
            provider: Provider::Gemini,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "antigravity:project".into(),
        })
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1internal", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1internal:streamGenerateContent", post(upstream))
                .with_state(captured),
        )
        .await
        .unwrap();
    });
    let http = Client::new();
    AppState::new_multi_with_model_fallback_and_aliases(
        vec![UpstreamState {
            provider: Provider::Gemini,
            token_manager: TokenManager::new_for_provider(store, Provider::Gemini, http.clone()),
            client: CodexClient::new_for_provider_base_url(http, Provider::Gemini, url),
        }],
        None,
        ModelList::from_id_owners([("gemini-2.5-pro".into(), "gemini")]),
        None,
        std::collections::BTreeMap::from([("my-gemini".into(), "gemini-2.5-pro".into())]),
    )
}

async fn response_json(response: axum::response::Response) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

#[tokio::test]
async fn gemini_responses_replay_preserves_signed_calls_and_aliases() {
    let dir = TempDir::new().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = gemini_state(
        AuthStore::new(dir.path().join("auth.json")),
        captured.clone(),
    )
    .await;
    let first = response_json(handlers::responses(State(state.clone()),HeaderMap::new(),Json(serde_json::from_value(json!({
        "model":"my-gemini","input":"lookup a","max_output_tokens":123,
        "tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"key":{"type":"string"}}}}]
    })).unwrap())).await).await;
    let call = first["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    let second = response_json(handlers::responses(State(state),HeaderMap::new(),Json(serde_json::from_value(json!({
        "model":"my-gemini","previous_response_id":first["id"],
        "input":[{"type":"function_call_output","call_id":call["call_id"],"output":"found"}]
    })).unwrap())).await).await;
    assert_eq!(second["output"][0]["content"][0]["text"], "done");
    let captured = captured.lock().await;
    assert_eq!(captured[0]["model"], "gemini-2.5-pro");
    assert_eq!(
        captured[0]["request"]["generationConfig"]["maxOutputTokens"],
        123
    );
    assert!(
        captured[1]["request"]["contents"]
            .to_string()
            .contains("signed")
    );
    drop(captured);
}

#[tokio::test]
async fn gemini_messages_roundtrip_tools_and_preserve_token_budget() {
    let dir = TempDir::new().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = gemini_state(
        AuthStore::new(dir.path().join("auth.json")),
        captured.clone(),
    )
    .await;
    let user = json!({"role":"user","content":"lookup a"});
    let first = response_json(handlers::messages(State(state.clone()),HeaderMap::new(),Json(serde_json::from_value(json!({
        "model":"my-gemini","max_tokens":256,"messages":[user],
        "tools":[{"name":"lookup","input_schema":{"type":"object","properties":{"key":{"type":"string"}}}}]
    })).unwrap())).await).await;
    assert_eq!(first["stop_reason"], "tool_use");
    let call = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "tool_use")
        .unwrap();
    let second = response_json(handlers::messages(State(state),HeaderMap::new(),Json(serde_json::from_value(json!({
        "model":"my-gemini","max_tokens":256,"messages":[user,{"role":"assistant","content":first["content"]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":call["id"],"content":"found"}]}]
    })).unwrap())).await).await;
    assert_eq!(second["content"][0]["text"], "done");
    let captured = captured.lock().await;
    assert_eq!(
        captured[0]["request"]["generationConfig"]["maxOutputTokens"],
        256
    );
    assert!(
        captured[1]["request"]["contents"]
            .to_string()
            .contains("signed")
    );
    drop(captured);
}

#[test]
fn gemini_namespace_does_not_capture_vercel_google_models() {
    assert_eq!(
        crate::models::provider_for_model("gemini-2.5-pro"),
        Some(Provider::Gemini)
    );
    assert_eq!(
        crate::models::provider_for_model("gemini/gemini-2.5-pro"),
        Some(Provider::Gemini)
    );
    assert_eq!(
        crate::models::provider_for_model("google/gemini-2.5-pro"),
        Some(Provider::Vercel)
    );
    assert_eq!(
        normalize_model_for_support("gemini/gemini-2.5-pro"),
        "gemini-2.5-pro"
    );
}
