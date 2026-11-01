use super::*;
use crate::{forward::ForwardConfig, openai::response::ModelList, testsupport::TempDir};
use axum::{
    body::{Body, to_bytes},
    http::{Request as HttpRequest, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn config(url: &str, key: &str, model: &str) -> AppConfig {
    AppConfig {
        api_key: Some(key.into()),
        model_aliases: BTreeMap::from([("friendly".into(), format!("remote/{model}"))]),
        forwards: BTreeMap::from([(
            "remote".into(),
            ForwardConfig {
                base_url: url.into(),
                api_key: None,
                headers: BTreeMap::new(),
                models: vec![model.into()],
            },
        )]),
        ..AppConfig::default()
    }
}

fn runtime(dir: &TempDir, config: AppConfig) -> Arc<Runtime> {
    let store = AppConfigStore::new(dir.path().join("config.json"));
    store.save(&config).unwrap();
    let state = AppState::new_multi_with_forwards(
        vec![],
        config.api_key.clone(),
        ModelList::from_id_owners(Vec::<(String, &'static str)>::new()),
        None,
        config.model_aliases.clone(),
        config.forwards.clone(),
    )
    .unwrap();
    Arc::new(Runtime::new(
        state,
        ReloadSettings {
            store,
            startup_config: config,
            api_key_override: None,
            fallback_override: None,
            default_fallback: "gpt-6-sol".into(),
        },
    ))
}

async fn models(runtime: &Arc<Runtime>, key: &str) -> (StatusCode, Value) {
    let response = dispatch(
        State(Arc::clone(runtime)),
        HttpRequest::builder()
            .uri("/v1/models")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn rejected_reload_preserves_auth_models_and_routing() {
    let dir = TempDir::new().unwrap();
    let original = config("http://127.0.0.1:9", "old-key", "old-model");
    let runtime = runtime(&dir, original.clone());
    let before = models(&runtime, "old-key").await;
    let mut invalid_forward = original.clone();
    invalid_forward.forwards.get_mut("remote").unwrap().base_url = "file:///secret".into();
    let mut invalid_alias = original.clone();
    invalid_alias
        .model_aliases
        .insert("bad alias".into(), "remote/new".into());
    let mut changed_port = original.clone();
    changed_port.bind_port = Some(4321);
    let mut changed_provider = original.clone();
    changed_provider.provider = Some(crate::config::Provider::Grok);
    let mut no_backends = original;
    no_backends.forwards.clear();
    let cases = vec![
        "{".to_owned(),
        serde_json::to_string(&invalid_forward).unwrap(),
        serde_json::to_string(&invalid_alias).unwrap(),
        serde_json::to_string(&changed_port).unwrap(),
        serde_json::to_string(&changed_provider).unwrap(),
        serde_json::to_string(&no_backends).unwrap(),
    ];
    for raw in cases {
        std::fs::write(runtime.settings.store.path(), raw).unwrap();
        assert!(runtime.reload().await.is_err());
        assert_eq!(models(&runtime, "old-key").await, before);
        assert_eq!(
            models(&runtime, "new-key").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    std::fs::remove_file(runtime.settings.store.path()).unwrap();
    assert!(runtime.reload().await.is_err());
    assert_eq!(models(&runtime, "old-key").await, before);
}

#[tokio::test]
async fn successful_reload_replaces_auth_models_aliases_and_preserves_stores() {
    let dir = TempDir::new().unwrap();
    let runtime = runtime(&dir, config("http://127.0.0.1:9", "old-key", "old"));
    let mut next = config("http://127.0.0.1:10", "new-key", "new");
    next.model_fallback = Some("remote/new".into());
    runtime.settings.store.save(&next).unwrap();
    let candidate = runtime.prepare().await.unwrap();
    let mut alias = "friendly".to_owned();
    candidate.rewrite_model(&mut alias);
    assert_eq!(alias, "remote/new");
    assert_eq!(candidate.model_fallback.as_deref(), Some("remote/new"));
    // Preparing alone does not publish any of the candidate's auth or model changes.
    assert_eq!(models(&runtime, "old-key").await.0, StatusCode::OK);
    runtime.reload().await.unwrap();
    assert_eq!(
        models(&runtime, "old-key").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = models(&runtime, "new-key").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "remote/new")
    );
    assert!(
        !body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "remote/old")
    );
    // The stores are shared handles, including work inserted by an old generation.
    let batch = crate::anthropic::MessageBatch {
        id: "batch-old".into(),
        kind: "message_batch",
        processing_status: "in_progress",
        request_counts: crate::anthropic::MessageBatchRequestCounts {
            processing: 1,
            succeeded: 0,
            errored: 0,
            canceled: 0,
            expired: 0,
        },
        ended_at: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        expires_at: "2026-01-02T00:00:00Z".into(),
        archived_at: None,
        cancel_initiated_at: None,
        results_url: None,
    };
    runtime
        .base
        .batches
        .insert(super::super::store::StoredBatch {
            batch,
            results: vec![],
            cancel_requested: false,
        })
        .await;
    assert!(candidate.batches.get("batch-old").await.is_some());
    let response = dispatch(
        State(Arc::clone(&runtime)),
        HttpRequest::builder()
            .uri("/v1/messages/batches/batch-old")
            .header("authorization", "Bearer new-key")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["id"], "batch-old");
}

#[tokio::test]
async fn explicit_startup_overrides_survive_file_reload() {
    let dir = TempDir::new().unwrap();
    let mut runtime = runtime(&dir, config("http://127.0.0.1:9", "old", "model"));
    let owned = Arc::get_mut(&mut runtime).unwrap();
    owned.settings.api_key_override = Some("override".into());
    owned.settings.fallback_override = Some("override-model".into());
    let candidate = owned.prepare().await.unwrap();
    assert_eq!(candidate.api_key.as_deref(), Some("override"));
    assert_eq!(candidate.model_fallback.as_deref(), Some("override-model"));
}

#[tokio::test]
async fn reload_keeps_open_stream_and_routes_new_requests_to_new_destination() {
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream = Router::new().route(
        "/v1/chat/completions",
        post({
            let release = Arc::clone(&release);
            move || {
                let release = Arc::clone(&release);
                async move {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>("data: first\n\n");
                            release.notified().await;
                            yield Ok::<_, std::io::Error>("data: last\n\ndata: [DONE]\n\n");
                        }))
                        .unwrap()
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_url = format!("http://{}", listener.local_addr().unwrap());
    let old_server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_url = format!("http://{}", listener.local_addr().unwrap());
    let new_server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/chat/completions",
                post(|axum::Json(payload): axum::Json<Value>| async move {
                    axum::Json(json!({"destination":"new", "model": payload["model"]}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let dir = TempDir::new().unwrap();
    let runtime = runtime(&dir, config(&old_url, "old-key", "old-model"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let app = Router::new()
        .fallback(dispatch)
        .with_state(Arc::clone(&runtime));
    let api_server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let http = reqwest::Client::new();
    let mut stream = http
        .post(&api)
        .bearer_auth("old-key")
        .json(&json!({"model":"friendly", "stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(stream.chunk().await.unwrap().unwrap(), "data: first\n\n");
    // A rejected candidate also leaves the already-open stream and old auth intact.
    std::fs::write(runtime.settings.store.path(), "{").unwrap();
    assert!(runtime.reload().await.is_err());
    assert_eq!(models(&runtime, "old-key").await.0, StatusCode::OK);
    runtime
        .settings
        .store
        .save(&config(&new_url, "new-key", "new-model"))
        .unwrap();
    runtime.reload().await.unwrap();
    let fresh: Value = http
        .post(&api)
        .bearer_auth("new-key")
        .json(&json!({"model":"friendly"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fresh, json!({"destination":"new", "model":"new-model"}));
    release.notify_one();
    let tail = tokio::time::timeout(std::time::Duration::from_secs(3), stream.text())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tail, "data: last\n\ndata: [DONE]\n\n");
    api_server.abort();
    old_server.abort();
    new_server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn control_socket_acknowledges_success_and_rejection_and_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let runtime = runtime(&dir, config("http://127.0.0.1:9", "key", "model"));
    let control_path = runtime.settings.store.path().to_path_buf();
    let socket = control::ControlSocket::bind(&control_path).unwrap();
    assert_eq!(
        std::fs::metadata(control::socket_path(&control_path).unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(control::ControlSocket::bind(&control_path).is_err());
    let server_runtime = Arc::clone(&runtime);
    let server = tokio::spawn(async move { socket.serve(&server_runtime).await });
    let path = control_path.clone();
    tokio::task::spawn_blocking(move || control::request(&path))
        .await
        .unwrap()
        .unwrap();
    std::fs::write(runtime.settings.store.path(), "{").unwrap();
    let error = tokio::task::spawn_blocking(move || control::request(&control_path))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("JSON"));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn native_model_overrides_and_fallback_apply_to_the_next_snapshot() {
    use crate::{
        codex::client::CodexClient,
        config::{AuthStore, Provider},
        token::TokenManager,
    };
    let dir = TempDir::new().unwrap();
    let http = reqwest::Client::new();
    let upstream = super::super::UpstreamState {
        provider: Provider::Codex,
        token_manager: TokenManager::new_for_provider(
            AuthStore::new(dir.path().join("auth.json")),
            Provider::Codex,
            http.clone(),
        ),
        client: CodexClient::new_for_provider(http, Provider::Codex),
    };
    let initial = AppConfig::default();
    let store = AppConfigStore::new(dir.path().join("config.json"));
    let state = AppState::new_multi_with_model_fallback(
        vec![upstream],
        None,
        ModelList::from_id_owners([("gpt-old".into(), "openai-codex")]),
        None,
    );
    let runtime = Arc::new(Runtime::new(
        state,
        ReloadSettings {
            store,
            startup_config: initial,
            api_key_override: None,
            fallback_override: None,
            default_fallback: "gpt-6-sol".into(),
        },
    ));
    let config = AppConfig {
        models: BTreeMap::from([(Provider::Codex, vec!["gpt-new".into()])]),
        model_aliases: BTreeMap::from([("friendly".into(), "gpt-new".into())]),
        model_fallback: Some("gpt-new".into()),
        ..AppConfig::default()
    };
    runtime.settings.store.save(&config).unwrap();
    let candidate = runtime.prepare().await.unwrap();
    for mut model in ["friendly".to_owned(), "claude-unsupported".to_owned()] {
        candidate.rewrite_model(&mut model);
        assert_eq!(model, "gpt-new");
    }
    runtime.reload().await.unwrap();
    let (_, body) = models(&runtime, "unused").await;
    let ids: Vec<_> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-new", "friendly"]);
}

#[cfg(unix)]
#[tokio::test]
async fn socket_recovers_after_crash_but_never_removes_an_unrelated_file() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.json");
    let socket_path = control::socket_path(&config_path).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    drop(listener); // Simulate a process exit that left the pathname behind.
    let socket = control::ControlSocket::bind(&config_path).unwrap();
    assert!(socket_path.exists());
    assert!(control::ControlSocket::bind(&config_path).is_err());
    drop(socket);
    assert!(!socket_path.exists());
    std::fs::write(&socket_path, "unrelated").unwrap();
    assert!(control::ControlSocket::bind(&config_path).is_err());
    assert_eq!(std::fs::read_to_string(&socket_path).unwrap(), "unrelated");
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_file(config_path.with_extension("reload.lock")).unwrap();
}

#[cfg(not(unix))]
#[test]
fn reload_command_reports_unsupported_transport() {
    assert!(matches!(
        request_reload(),
        Err(Error::Reload(ReloadError::UnsupportedPlatform))
    ));
}

#[tokio::test]
async fn unrepresentable_api_keys_are_rejected_without_replacing_working_auth() {
    let dir = TempDir::new().unwrap();
    let initial = config("http://127.0.0.1:9", "old-key", "model");
    let runtime = runtime(&dir, initial.clone());
    for key in ["密钥", "café", "bad\nkey", "bad\tkey", "bad\u{7f}key"] {
        let mut candidate = initial.clone();
        candidate.api_key = Some(key.into());
        runtime.settings.store.save(&candidate).unwrap();
        assert!(matches!(
            runtime.reload().await,
            Err(Error::Reload(ReloadError::InvalidApiKey))
        ));
        assert_eq!(models(&runtime, "old-key").await.0, StatusCode::OK);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn long_config_home_serves_and_reloads_through_its_directory_alias() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let dir = TempDir::new().unwrap();
    let initial = config("http://127.0.0.1:9", "old-key", "model");
    let Runtime {
        base, mut settings, ..
    } = Arc::try_unwrap(runtime(&dir, initial.clone()))
        .ok()
        .unwrap();
    let long_home = dir.path().join("long-home-".repeat(20));
    std::fs::create_dir(&long_home).unwrap();
    settings.store = AppConfigStore::new(long_home.join("config.json"));
    settings.store.save(&initial).unwrap();
    let alias = dir.path().join("alias");
    symlink(&long_home, &alias).unwrap();
    let alias_config = alias.join("config.json");
    let socket_path = control::socket_path(settings.store.path()).unwrap();
    assert!(socket_path.as_os_str().as_bytes().len() < 104);
    assert_eq!(socket_path, control::socket_path(&alias_config).unwrap());
    assert_ne!(
        socket_path,
        control::socket_path(&dir.path().join("config.json")).unwrap()
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let server = tokio::spawn(async move { serve_reloadable(&[addr], base, settings).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !socket_path.exists() {
            assert!(!server.is_finished());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/v1/models");
    assert_eq!(
        http.get(&url)
            .bearer_auth("old-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let mut updated = initial;
    updated.api_key = Some("new-key".into());
    AppConfigStore::new(&alias_config).save(&updated).unwrap();
    tokio::task::spawn_blocking(move || control::request(&alias_config))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        http.get(&url)
            .bearer_auth("new-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        http.get(&url)
            .bearer_auth("old-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    server.abort();
    let _ = server.await;
    assert!(!socket_path.exists());
}
