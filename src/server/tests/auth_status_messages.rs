use super::*;

#[test]
fn authorizes_when_no_api_key_is_configured() {
    assert!(authorize(&HeaderMap::new(), None).is_ok());
}

#[test]
fn authorizes_bearer_token() {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer secret"),
    );

    assert!(authorize(&headers, Some("secret")).is_ok());
}

#[test]
fn rejects_wrong_api_key() {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("wrong"));

    assert!(matches!(
        authorize(&headers, Some("secret")),
        Err(Error::Unauthorized)
    ));
}

#[tokio::test]
async fn manual_refresh_refreshes_credentials_without_returning_tokens() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "old_access".into(),
            refresh_token: "old_refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let state = test_state(
        store.clone(),
        spawn_refresh_server().await,
        Some("secret".into()),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::manual_refresh(axum::extract::State(state), headers).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["providers"][0]["account_id"], "acc_refreshed");
    assert!(value["providers"][0].get("expires_at").is_some());
    assert!(value.get("access_token").is_none());
    assert!(value.get("refresh_token").is_none());

    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.refresh_token, "new_refresh");
    assert_eq!(saved.account_id, "acc_refreshed");
}

#[tokio::test]
async fn manual_refresh_requires_api_key_when_configured() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));

    let response = handlers::manual_refresh(axum::extract::State(state), HeaderMap::new()).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn status_returns_account_and_rate_limit_snapshot() {
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

    let status_base_url = spawn_status_server().await;
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, status_base_url),
        Some("secret".into()),
        ModelList::from_ids(["gpt-test"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::status(axum::extract::State(state), headers).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["account_id"], "acc_old");
    assert_eq!(value["account"]["plan"], "chatgptplus");
    assert!(value["account"]["subscription_expires_at_local"].is_string());
    assert!(value["account"]["subscription_remaining_seconds"].is_number());
    assert_eq!(value["rate_limits"][0]["name"], "5h");
    assert_eq!(value["rate_limits"][0]["remaining_percent"], 90.0);
    assert!(value["rate_limits"][0]["reset_at_local"].is_string());
    assert!(value["rate_limits"][0]["reset_in_seconds"].is_number());
    assert!(value["token"]["expires_at_local"].is_string());
    assert_eq!(value["warnings"], json!([]));
}

#[tokio::test]
async fn status_returns_authenticated_snapshot_for_unsupported_provider() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Grok,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: String::new(),
        })
        .unwrap();

    let http = Client::new();
    let state = AppState::new_multi_with_model_fallback(
        vec![UpstreamState {
            provider: Provider::Grok,
            token_manager: TokenManager::new_for_provider(store, Provider::Grok, http.clone()),
            client: CodexClient::new_for_provider(http, Provider::Grok),
        }],
        Some("secret".into()),
        ModelList::from_ids(["grok-4.3"]),
        None,
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::status(axum::extract::State(state), headers).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["provider"], "grok");
    assert!(value["token"]["remaining_seconds"].is_number());
    assert!(value["account"].is_null());
    assert_eq!(value["rate_limits"], json!([]));
    assert_eq!(
        value["warnings"].as_array().unwrap(),
        &[
            json!("Grok account metadata support is not implemented"),
            json!("Grok rate-limit support is not implemented"),
        ]
    );
    assert!(value.get("expires_at").is_none());
}

#[tokio::test]
async fn messages_returns_anthropic_message_response() {
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
    assert_eq!(value["type"], "message");
    assert_eq!(value["role"], "assistant");
    assert_eq!(value["content"][0]["type"], "text");
    assert_eq!(value["content"][0]["text"], "OK");
    assert_eq!(value["stop_reason"], "end_turn");
    assert_eq!(value["usage"]["input_tokens"], 12);
}

#[tokio::test]
async fn messages_returns_tool_use_blocks() {
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
        CodexClient::new(http, spawn_codex_server(true).await),
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
    assert_eq!(value["content"][0]["type"], "tool_use");
    assert_eq!(value["content"][0]["id"], "call_1");
    assert_eq!(value["content"][0]["name"], "lookup");
    assert_eq!(value["stop_reason"], "tool_use");
}

#[tokio::test]
async fn messages_return_thinking_blocks_from_reasoning_output() {
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
        CodexClient::new(http, spawn_reasoning_codex_server().await),
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
                "thinking": {"type": "enabled", "budget_tokens": 4096},
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["content"][0]["type"], "thinking");
    assert_eq!(value["content"][0]["thinking"], "work");
    assert_eq!(value["content"][0]["signature"], "sig");
    assert_eq!(value["content"][1]["type"], "text");
}

#[tokio::test]
async fn messages_preserve_cache_usage_fields() {
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
        CodexClient::new(http, spawn_cache_usage_codex_server().await),
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
    assert_eq!(value["usage"]["cache_creation_input_tokens"], 100);
    assert_eq!(value["usage"]["cache_read_input_tokens"], 200);
    assert_eq!(value["usage"]["server_tool_use"]["web_search_requests"], 1);
}

#[tokio::test]
async fn count_message_tokens_returns_estimate() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::count_message_tokens(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role": "user", "content": "Reply with the single word OK"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert!(value["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn count_message_tokens_include_documents_tools_and_thinking() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("extended-thinking-2025-05-14"),
    );

    let response = handlers::count_message_tokens(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "thinking": {"type": "enabled", "budget_tokens": 2048},
                    "tools": [{"name": "lookup", "description": "fetch docs", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}],
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "document", "source": {"type": "text", "text": "document body"}}]
                    }]
                }))
                .unwrap(),
            ),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert!(value["input_tokens"].as_u64().unwrap() > 0);
}
