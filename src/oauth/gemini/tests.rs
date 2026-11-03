use super::*;
use axum::{Form, Json, Router, extract::State, routing::post};
use std::{collections::HashMap, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex};

#[test]
fn oauth_flow_requires_offline_consent_pkce_and_exact_state() {
    let client = GeminiOAuthClient {
        application: Some(test_application()),
        ..GeminiOAuthClient::default()
    };
    let flow = client.create_authorization_flow().unwrap();
    let query: HashMap<_, _> = flow.authorize_url.query_pairs().collect();
    assert_eq!(query.get("access_type").unwrap(), "offline");
    assert_eq!(query.get("client_id").unwrap(), "test-client-id");
    assert_eq!(
        query.get("redirect_uri").unwrap(),
        "http://localhost:45289/oauth-callback"
    );
    assert!(
        query
            .get("scope")
            .unwrap()
            .contains("auth/experimentsandconfigs")
    );
    assert_eq!(
        query.get("code_challenge").unwrap(),
        &super::super::code_challenge(&flow.verifier)
    );
    assert_eq!(query.get("code_challenge_method").unwrap(), "S256");
    assert_ne!(
        flow.state,
        client.create_authorization_flow().unwrap().state
    );
    assert_eq!(
        GeminiOAuthClient::callback_code(
            &format!("{REDIRECT_URI}?code=abc&state={}", flow.state),
            &flow.state
        )
        .unwrap(),
        "abc"
    );
    for callback in [
        format!("{REDIRECT_URI}?code=abc"),
        format!("{REDIRECT_URI}?code=abc&state=wrong"),
        format!("{REDIRECT_URI}?code=abc&state={0}&state={0}", flow.state),
        format!("https://evil.test/?code=abc&state={}", flow.state),
        format!("{REDIRECT_URI}?error=access_denied&state={}", flow.state),
    ] {
        assert!(GeminiOAuthClient::callback_code(&callback, &flow.state).is_err());
    }
}

#[test]
fn refresh_preserves_metadata_and_validates_token_lifetime() {
    let token = TokenResponse {
        access_token: "new".into(),
        refresh_token: None,
        expires_in: 3600,
    };
    let credentials = credentials_from_token(token, Some("old-refresh"), "project".into()).unwrap();
    assert_eq!(credentials.refresh_token, "old-refresh");
    assert_eq!(credentials.account_id, "project");
    assert_eq!(credentials.provider, Provider::Gemini);
    for (access, expires, refresh) in [
        ("", 3600, Some("refresh")),
        ("access", 0, Some("refresh")),
        ("access", 3600, None),
    ] {
        assert!(
            credentials_from_token(
                TokenResponse {
                    access_token: access.into(),
                    refresh_token: refresh.map(str::to_owned),
                    expires_in: expires
                },
                None,
                String::new()
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn login_resolves_managed_project_and_refresh_keeps_it_without_reonboarding() {
    type Forms = Arc<Mutex<Vec<HashMap<String, String>>>>;
    async fn token(
        State(forms): State<Forms>,
        Form(form): Form<HashMap<String, String>>,
    ) -> Json<Value> {
        let refresh = form.get("grant_type").is_some_and(|s| s == "refresh_token");
        forms.lock().await.push(form);
        Json(if refresh {
            json!({"access_token":"refreshed","expires_in":3600})
        } else {
            json!({"access_token":"access","refresh_token":"refresh","expires_in":3600})
        })
    }
    async fn setup(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(headers.get("authorization").unwrap(), "Bearer access");
        assert_eq!(body["metadata"]["ideType"], "ANTIGRAVITY");
        Json(
            json!({"currentTier":{"id":"standard-tier"},"paidTier":{"id":"pro-tier"},"cloudaicompanionProject":"managed-project"}),
        )
    }
    let forms: Forms = Arc::default();
    let router = Router::new()
        .route("/token", post(token))
        .route("/v1internal:loadCodeAssist", post(setup))
        .with_state(forms.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = GeminiOAuthClient {
        application: Some(test_application()),
        http: Client::new(),
        token_url: format!("{base}/token"),
        assist_url: format!("{base}/v1internal"),
        generation_url: format!("{base}/v1internal"),
    };
    let original = client
        .exchange_authorization_code("code", "verifier")
        .await
        .unwrap();
    assert_eq!(original.account_id, "antigravity:");
    let original = client.ensure_project(original).await.unwrap();
    let refreshed = client.refresh_credentials(&original).await.unwrap();
    assert_eq!(refreshed.access_token, "refreshed");
    assert_eq!(refreshed.account_id, "antigravity:managed-project");
    assert_eq!(refreshed.refresh_token, "refresh");
    let forms = forms.lock().await;
    assert_eq!(forms.len(), 2);
    assert_eq!(forms[0].get("code_verifier").unwrap(), "verifier");
    assert_eq!(forms[1].get("refresh_token").unwrap(), "refresh");
    task.abort();
}

#[tokio::test]
async fn first_login_onboards_default_tier_and_rejects_missing_project() {
    async fn load() -> Json<Value> {
        Json(
            json!({"allowedTiers":[{"id":"other","isDefault":false},{"id":"free-tier","isDefault":true}]}),
        )
    }
    async fn onboard(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["tier_id"], "free-tier");
        assert_eq!(body["metadata"]["ide_type"], "ANTIGRAVITY");
        Json(json!({"done":true,"response":{"cloudaicompanionProject":{"id":"new-project"}}}))
    }
    async fn missing() -> Json<Value> {
        Json(json!({"currentTier":{"id":"standard-tier"}}))
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/good:loadCodeAssist", post(load))
                .route("/daily:onboardUser", post(onboard))
                .route("/missing:loadCodeAssist", post(missing)),
        )
        .await
        .unwrap();
    });
    let mut client = GeminiOAuthClient {
        application: Some(test_application()),
        http: Client::new(),
        token_url: format!("{base}/token"),
        assist_url: format!("{base}/good"),
        generation_url: format!("{base}/daily"),
    };
    assert_eq!(
        client.resolve_project("access").await.unwrap(),
        "new-project"
    );
    client.assist_url = format!("{base}/missing");
    client.generation_url = format!("{base}/missing");
    assert!(client.resolve_project("access").await.is_err());
    task.abort();
}

#[tokio::test]
async fn completed_onboarding_reloads_omitted_project_and_distinguishes_failure() {
    async fn load() -> Json<Value> {
        Json(json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"reloaded-project"}))
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/assist:loadCodeAssist", post(load)),
        )
        .await
        .unwrap();
    });
    let client = GeminiOAuthClient {
        application: Some(test_application()),
        http: Client::new(),
        token_url: format!("{base}/token"),
        assist_url: format!("{base}/assist"),
        generation_url: format!("{base}/assist"),
    };
    for (operation, expected) in [
        (json!({"done":true,"response":{}}), "reloaded-project"),
        (
            json!({"done":true,"response":{"cloudaicompanionProject":"string-project"}}),
            "string-project",
        ),
        (
            json!({"done":true,"response":{"cloudaicompanionProject":{"id":"object-project"}}}),
            "object-project",
        ),
    ] {
        assert_eq!(
            client
                .completed_project("access", &operation, &json!({}))
                .await
                .unwrap(),
            expected
        );
    }
    let failure = client
        .completed_project(
            "access",
            &json!({"done":true,"error":{"code":7,"message":"Account ineligible"}}),
            &json!({}),
        )
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("code 7"));
    assert!(failure.to_string().contains("Account ineligible"));
    assert!(managed_project(&json!({"cloudaicompanionProject":{"id":" "}})).is_none());
    task.abort();
}

#[test]
fn antigravity_credentials_reject_legacy_client_and_distinguish_pending_project() {
    let mut credentials = Credentials {
        provider: Provider::Gemini,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: now_unix() + 600,
        account_id: "old-project".into(),
    };
    assert!(GeminiOAuthClient::project_id(&credentials).is_err());
    credentials.account_id = "antigravity:".into();
    assert_eq!(GeminiOAuthClient::project_id(&credentials).unwrap(), "");
    credentials.account_id = "antigravity:project".into();
    assert_eq!(
        GeminiOAuthClient::project_id(&credentials).unwrap(),
        "project"
    );
}

#[tokio::test]
async fn rotated_tokens_survive_project_failure_and_retry_without_new_oauth() {
    use crate::{
        config::AuthStore,
        testsupport::TempDir,
        token::{OAuthClient, TokenManager},
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    async fn token() -> Json<Value> {
        Json(json!({"access_token":"new-access","refresh_token":"rotated","expires_in":3600}))
    }
    async fn load(
        State(ready): State<Arc<AtomicBool>>,
        headers: axum::http::HeaderMap,
    ) -> (axum::http::StatusCode, Json<Value>) {
        assert_eq!(headers.get("authorization").unwrap(), "Bearer new-access");
        if ready.load(Ordering::SeqCst) {
            (
                axum::http::StatusCode::OK,
                Json(json!({"cloudaicompanionProject":"project"})),
            )
        } else {
            (axum::http::StatusCode::FORBIDDEN, Json(json!({})))
        }
    }
    let ready = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/token", post(token))
        .route("/assist:loadCodeAssist", post(load))
        .with_state(ready.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = GeminiOAuthClient {
        application: Some(test_application()),
        http: Client::new(),
        token_url: format!("{base}/token"),
        assist_url: format!("{base}/assist"),
        generation_url: format!("{base}/daily"),
    };
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Gemini,
            access_token: "expired".into(),
            refresh_token: "old".into(),
            expires_at: 0,
            account_id: "antigravity:".into(),
        })
        .unwrap();
    let manager = TokenManager::new_with_oauth(store.clone(), OAuthClient::Gemini(client));
    assert!(manager.refresh().await.is_err());
    assert_eq!(
        store
            .load_provider(Provider::Gemini)
            .unwrap()
            .unwrap()
            .refresh_token,
        "rotated"
    );
    ready.store(true, Ordering::SeqCst);
    let credentials = manager.credentials().await.unwrap();
    assert_eq!(credentials.refresh_token, "rotated");
    assert_eq!(credentials.account_id, "antigravity:project");
    task.abort();
}

fn test_application() -> std::sync::Arc<application::Application> {
    std::sync::Arc::new(application::Application {
        client_id: "test-client-id".into(),
        client_secret: "test-client-secret".into(),
    })
}
