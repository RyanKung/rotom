use super::*;

#[tokio::test]
async fn messages_preserve_upstream_client_errors() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_bad_request_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "claude-opus-4-7",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("claude-opus-4-7")
    );
}

#[tokio::test]
async fn messages_rewrite_known_anthropic_models_to_configured_fallback() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let http = Client::new();
    let state = AppState::new_with_model_fallback(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
        Some("gpt-5.5".into()),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let (captured_len, captured_model) = {
        let captured = captured.lock().await;
        (captured.len(), captured[0]["model"].clone())
    };
    assert_eq!(captured_len, 1);
    assert_eq!(captured_model, "gpt-5.5");
}

#[tokio::test]
async fn messages_non_streaming_still_streams_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        CodexClient::new(
            http,
            spawn_stream_required_codex_server(captured.clone()).await,
        ),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": false,
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["type"], "message");
    assert_eq!(value["content"][0]["text"], "OK");
    let (captured_len, captured_stream) = {
        let captured = captured.lock().await;
        (captured.len(), captured[0]["stream"].clone())
    };
    assert_eq!(captured_len, 1);
    assert_eq!(captured_stream, true);
}

#[tokio::test]
async fn responses_preserve_incomplete_status_and_reason() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_incomplete_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["content"][0]["type"], "text");
    assert_eq!(value["content"][0]["text"], "partial");
    assert_eq!(value["stop_reason"], "max_tokens");
}

#[tokio::test]
async fn models_return_anthropic_shape_when_requested() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

    let response = handlers::models(axum::extract::State(state), headers).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["data"][0]["type"], "model");
    assert!(value["data"][0]["display_name"].is_string());
}

#[tokio::test]
async fn models_keep_native_ids_for_non_anthropic_clients() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::models(axum::extract::State(state), headers).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["data"][0]["id"], "gpt-test");
    assert!(
        value["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| !model["id"].as_str().unwrap().starts_with("claude-"))
    );
}

#[tokio::test]
async fn message_batches_can_be_retrieved_with_results() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_codex_server(false).await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert(HOST, HeaderValue::from_static("localhost:14550"));

    let response = handlers::create_message_batch(
        axum::extract::State(state.clone()),
        headers.clone(),
        Json(
            serde_json::from_value(json!({
                "requests": [
                    {
                        "custom_id": "req_1",
                        "params": {
                            "model": "gpt-5.5",
                            "max_tokens": 32,
                            "messages": [{"role": "user", "content": "hello"}]
                        }
                    },
                    {
                        "custom_id": "req_2",
                        "params": {
                            "model": "gpt-5.5",
                            "max_tokens": 32,
                            "messages": [{"role": "user", "content": "again"}]
                        }
                    }
                ]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["type"], "message_batch");
    assert_eq!(value["processing_status"], "in_progress");
    let batch_id = value["id"].as_str().unwrap().to_owned();

    let retrieved =
        wait_for_batch_to_finish(state.clone(), headers.clone(), batch_id.clone()).await;
    assert_eq!(retrieved["processing_status"], "ended");

    let results = handlers::message_batch_results(
        axum::extract::State(state),
        headers,
        axum::extract::Path(batch_id),
    )
    .await;
    assert_eq!(results.status(), StatusCode::OK);
    let body = to_bytes(results.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("\"custom_id\":\"req_1\""));
    assert!(body.contains("\"type\":\"succeeded\""));
}

#[tokio::test]
async fn responses_compact_returns_message_history_plus_compaction_item() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::compact_response(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "input": [
                    {
                        "type": "message",
                        "role": "developer",
                        "content": [{"type": "input_text", "text": "be terse"}]
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello"}]
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hi"}]
                    }
                ]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    let output = value["output"].as_array().unwrap();
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["role"], "developer");
    assert_eq!(output[1]["role"], "user");
    assert_eq!(output[2]["type"], "compaction");
    assert!(output[2]["encrypted_content"].as_str().unwrap().len() > 8);
}

#[tokio::test]
async fn message_batches_can_be_listed_and_deleted() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_delayed_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert(HOST, HeaderValue::from_static("localhost:14550"));

    let created = handlers::create_message_batch(
        axum::extract::State(state.clone()),
        headers.clone(),
        Json(
            serde_json::from_value(json!({
                "requests": [{
                    "custom_id": "req_1",
                    "params": {
                        "model": "gpt-5.5",
                        "max_tokens": 32,
                        "messages": [{"role": "user", "content": "hello"}]
                    }
                }]
            }))
            .unwrap(),
        ),
    )
    .await;
    let created_body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let created_value = serde_json::from_slice::<Value>(&created_body).unwrap();
    let batch_id = created_value["id"].as_str().unwrap().to_owned();
    assert_eq!(created_value["processing_status"], "in_progress");

    let listed =
        handlers::list_message_batches(axum::extract::State(state.clone()), headers.clone()).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
    let listed_value = serde_json::from_slice::<Value>(&listed_body).unwrap();
    assert_eq!(listed_value["data"][0]["id"], batch_id);

    let canceled = handlers::cancel_message_batch(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(batch_id.clone()),
    )
    .await;
    assert_eq!(canceled.status(), StatusCode::OK);
    let canceled_body = to_bytes(canceled.into_body(), usize::MAX).await.unwrap();
    let canceled_value = serde_json::from_slice::<Value>(&canceled_body).unwrap();
    assert_eq!(canceled_value["processing_status"], "canceling");

    let finished = wait_for_batch_to_finish(state.clone(), headers.clone(), batch_id.clone()).await;
    assert!(finished["request_counts"]["canceled"].as_u64().unwrap() >= 1);

    let results = handlers::message_batch_results(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(batch_id.clone()),
    )
    .await;
    assert_eq!(results.status(), StatusCode::OK);
    let results_body = to_bytes(results.into_body(), usize::MAX).await.unwrap();
    let results_text = String::from_utf8(results_body.to_vec()).unwrap();
    assert!(results_text.contains("\"type\":\"canceled\""));

    let deleted = handlers::delete_message_batch(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(batch_id.clone()),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);

    let missing = handlers::get_message_batch(
        axum::extract::State(state),
        headers,
        axum::extract::Path(batch_id),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn responses_forward_compaction_summary_into_upstream_request() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        CodexClient::new(
            http,
            spawn_strict_recording_codex_server(captured.clone()).await,
        ),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let compacted = handlers::compact_response(
        axum::extract::State(state.clone()),
        headers.clone(),
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "input": [
                    {
                        "type": "message",
                        "role": "developer",
                        "content": [{"type": "input_text", "text": "be terse"}]
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello"}]
                    }
                ]
            }))
            .unwrap(),
        ),
    )
    .await;
    let compacted_body = to_bytes(compacted.into_body(), usize::MAX).await.unwrap();
    let compacted_value = serde_json::from_slice::<Value>(&compacted_body).unwrap();

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "input": compacted_value["output"]
            }))
            .unwrap(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    let instructions = body["instructions"].as_str().unwrap();
    assert!(instructions.contains("developer: be terse"));
    assert!(instructions.contains("user: hello"));
}

#[tokio::test]
async fn messages_send_empty_instructions_when_system_is_absent() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    assert_eq!(body["instructions"], "");
}

#[tokio::test]
async fn messages_accept_anthropic_beta_without_forwarding_internal_keys() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("extended-thinking-2025-05-14"),
    );

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    assert!(body.get("rotom_anthropic_beta").is_none());
}

#[tokio::test]
async fn messages_ignore_unknown_claude_billing_header() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert(
        "x-anthropic-billing-header",
        HeaderValue::from_static("{\"cch\":\"bypass-third-party-cache\"}"),
    );

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    assert!(body.get("x-anthropic-billing-header").is_none());
    assert!(body.get("cch").is_none());
    assert!(find_forbidden_key_path(&body, "x-anthropic-billing-header").is_none());
    assert!(find_forbidden_key_path(&body, "cch").is_none());
}

#[tokio::test]
async fn messages_strip_claude_code_billing_block_from_system_prompt() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "system": "x-anthropic-billing-header: cc_version=2.1.38; cc_entrypoint=cli; cch=4873d;\n\nbe terse",
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    assert_eq!(body["instructions"], "");
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["input"][0]["content"][0]["text"], "be terse");
    assert!(find_forbidden_key_path(&body, "x-anthropic-billing-header").is_none());
    assert!(find_forbidden_key_path(&body, "cch").is_none());
}

#[tokio::test]
async fn messages_strip_cache_control_from_upstream_request() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "system": [{"type": "text", "text": "be terse", "cache_control": {"type": "ephemeral"}}],
                    "tools": [{"name": "lookup", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}],
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello", "cache_control": {"type": "ephemeral"}}]
                    }]
                }))
                .unwrap(),
            ),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    assert_eq!(body["instructions"], "");
    assert_eq!(body["input"][0]["role"], "developer");
    assert!(
        body["input"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert!(
        body["input"][1]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert!(body["tools"][0].get("cache_control").is_none());
}

#[tokio::test]
async fn responses_strip_cache_control_from_upstream_request() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
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
        CodexClient::new(
            http,
            spawn_strict_recording_codex_server(captured.clone()).await,
        ),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "input": [{
                    "type": "message",
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": "be terse",
                        "cache_control": {"type": "ephemeral"}
                    }]
                }],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "parameters": {"type": "object"}
                    },
                    "cache_control": {"type": "ephemeral"}
                }]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = {
        let captured = captured.lock().await;
        captured.last().cloned().unwrap()
    };
    assert!(find_forbidden_key_path(&body, "cache_control").is_none());
}
