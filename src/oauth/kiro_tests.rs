use super::*;
use crate::testsupport::TempDir;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, header::USER_AGENT as HTTP_USER_AGENT},
    routing::post,
};
use reqwest::Client;
use serde_json::json;
use std::sync::Arc;
use tokio::{net::TcpListener, sync::Mutex};

type CreateTokenCapture = Arc<Mutex<Option<(Value, Option<String>)>>>;

async fn create_token_handler(
    State(capture): State<CreateTokenCapture>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let user_agent = headers
        .get(HTTP_USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    *capture.lock().await = Some((body, user_agent));
    Json(json!({
        "accessToken": "new-access",
        "refreshToken": "new-refresh",
        "expiresIn": 3600,
        "profileArn": "arn:aws:codewhisperer:us-west-2:123:profile/test"
    }))
}

async fn spawn_create_token_server() -> (String, CreateTokenCapture) {
    let capture = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/oauth/token", post(create_token_handler))
        .with_state(capture.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (base_url, capture)
}

#[test]
fn builds_kiro_portal_authorization_url() {
    let flow = KiroOAuthClient::create_authorization_flow().unwrap();
    let pairs = flow
        .authorize_url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();

    assert_eq!(
        flow.authorize_url.as_str().split('?').next().unwrap(),
        KIRO_AUTH_PORTAL_URL
    );
    assert_eq!(pairs.get("redirect_uri").unwrap(), KIRO_AUTH_REDIRECT_URI);
    assert_eq!(pairs.get("redirect_from").unwrap(), KIRO_AUTH_REDIRECT_FROM);
    assert_eq!(pairs.get("code_challenge_method").unwrap(), "S256");
    assert_eq!(flow.state.len(), 32);
    assert_eq!(flow.verifier.len(), 43);
}

#[test]
fn parses_kiro_portal_callback() {
    let callback = parse_kiro_authorization_callback(
        "http://localhost:3128/oauth/callback?login_option=google&code=abc&state=xyz",
    )
    .unwrap();

    assert_eq!(callback.code, "abc");
    assert_eq!(callback.state.as_deref(), Some("xyz"));
    assert_eq!(callback.login_option, "google");
    assert_eq!(callback.path, "/oauth/callback");
    assert_eq!(
        callback.token_exchange_redirect_uri(),
        "http://localhost:3128/oauth/callback?login_option=google"
    );
}

#[test]
fn parses_kiro_portal_callback_error() {
    let error = parse_kiro_authorization_callback(
            "http://localhost:3128/oauth/callback?error=access_denied&error_description=user%20cancelled",
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "OAuth error: Kiro login failed: user cancelled"
    );
}

#[tokio::test]
async fn exchange_social_callback_posts_official_json_shape() {
    let (base_url, capture) = spawn_create_token_server().await;
    let callback = parse_kiro_authorization_callback(
        "http://localhost:3128/oauth/callback?login_option=github&code=abc&state=xyz",
    )
    .unwrap();
    let client = KiroOAuthClient::new_with_auth_endpoint(Client::new(), base_url);

    let credentials = client
        .exchange_authorization_callback(&callback, "verifier")
        .await
        .unwrap();

    assert_eq!(credentials.provider, Provider::Kiro);
    assert_eq!(credentials.account_id, "kiro-desktop:us-west-2");
    assert!(!credentials.refresh_token.contains("new-access"));

    let (body, user_agent) = capture.lock().await.clone().unwrap();
    assert_eq!(body["code"], "abc");
    assert_eq!(body["code_verifier"], "verifier");
    assert_eq!(
        body["redirect_uri"],
        "http://localhost:3128/oauth/callback?login_option=github"
    );
    assert!(body.get("invitation_code").is_none());
    assert!(
        user_agent
            .as_deref()
            .is_some_and(|value| value.starts_with("KiroIDE-"))
    );
}

#[test]
fn imports_desktop_json_without_exposing_secret_fields() {
    let credentials = credentials_from_desktop_json(
        r#"{
              "accessToken": "access",
              "refreshToken": "refresh",
              "expiresAt": "2026-05-29T00:00:00Z",
              "profileArn": "arn:aws:codewhisperer:us-west-2:123:profile/test",
              "userAgent": "KiroIDE-2.4.2-test"
            }"#,
    )
    .unwrap();

    assert_eq!(credentials.provider, Provider::Kiro);
    assert_eq!(credentials.account_id, "kiro-desktop:us-west-2");
    assert!(!credentials.refresh_token.contains("access"));
    assert!(credentials.refresh_token.contains("KiroIDE-2.4.2-test"));
}

#[test]
fn imports_cli_sqlite_with_device_registration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
    connection
            .execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                [
                    "kirocli:social:token",
                    r#"{"accessToken":"access","refreshToken":"refresh","expiresAt":1893456000,"profileArn":"arn:aws:codewhisperer:eu-central-1:123:profile/test"}"#,
                ],
            )
            .unwrap();
    connection
        .execute(
            "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
            [
                "kirocli:oidc:device-registration",
                r#"{"clientId":"client","clientSecret":"secret","region":"us-east-1"}"#,
            ],
        )
        .unwrap();

    let credentials = KiroOAuthClient::import_cli_database(&path).unwrap();

    assert_eq!(credentials.provider, Provider::Kiro);
    assert_eq!(credentials.account_id, "kiro-sso:us-east-1->eu-central-1");
    assert!(!credentials.refresh_token.contains("access"));
    assert!(credentials.refresh_token.contains("client"));
    assert!(credentials.refresh_token.contains("secret"));
}

#[test]
fn imports_cli_social_token_without_device_registration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
    connection
            .execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                [
                    "kirocli:social:token",
                    r#"{"access_token":"access","refresh_token":"refresh","expires_at":1893456000,"profile_arn":"arn:aws:codewhisperer:ap-southeast-1:123:profile/test","provider":"google"}"#,
                ],
            )
            .unwrap();

    let credentials = KiroOAuthClient::import_cli_database(&path).unwrap();

    assert_eq!(credentials.provider, Provider::Kiro);
    assert_eq!(credentials.account_id, "kiro-desktop:ap-southeast-1");
    let secret = decode_refresh_secret(&credentials.refresh_token).unwrap();
    assert_eq!(account_label(&secret), "kiro-desktop:ap-southeast-1");
}

/// Builds a kiro-cli database shaped like an IAM Identity Center login: an
/// OIDC token without a profile ARN plus a device registration, and an
/// optional selected profile in the `state` table.
fn identity_center_cli_database(dir: &TempDir, token: &str, state_profile: Option<&str>) -> PathBuf {
    let path = dir.path().join("data.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE state (key TEXT PRIMARY KEY, value BLOB);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
            ["kirocli:odic:token", token],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
            [
                "kirocli:odic:device-registration",
                r#"{"client_id":"client","client_secret":"secret","region":"us-east-1"}"#,
            ],
        )
        .unwrap();
    if let Some(profile) = state_profile {
        connection
            .execute(
                "INSERT INTO state (key, value) VALUES (?1, ?2)",
                ["api.codewhisperer.profile", profile],
            )
            .unwrap();
    }
    path
}

const IDENTITY_CENTER_TOKEN: &str = r#"{"access_token":"access","refresh_token":"refresh","expires_at":"2030-01-01T00:00:00Z","region":"us-east-1","start_url":"https://example.awsapps.com/start","oauth_flow":"DeviceCode","scopes":["codewhisperer:completions"]}"#;

#[test]
fn imports_identity_center_profile_arn_from_cli_state() {
    let dir = TempDir::new().unwrap();
    let path = identity_center_cli_database(
        &dir,
        IDENTITY_CENTER_TOKEN,
        Some(r#"{"arn":"arn:aws:codewhisperer:eu-central-1:123:profile/STATE","profile_name":"dev"}"#),
    );

    let credentials = KiroOAuthClient::import_cli_database(&path).unwrap();

    assert_eq!(
        profile_arn_from_credentials(&credentials).unwrap(),
        "arn:aws:codewhisperer:eu-central-1:123:profile/STATE"
    );
    assert_eq!(api_region_from_credentials(&credentials).unwrap(), "eu-central-1");
    assert_eq!(credentials.account_id, "kiro-sso:us-east-1->eu-central-1");
}

#[test]
fn token_profile_arn_takes_precedence_over_cli_state() {
    let dir = TempDir::new().unwrap();
    let token = r#"{"access_token":"access","refresh_token":"refresh","expires_at":1893456000,"region":"us-east-1","profile_arn":"arn:aws:codewhisperer:us-west-2:123:profile/TOKEN"}"#;
    let path = identity_center_cli_database(
        &dir,
        token,
        Some(r#"{"arn":"arn:aws:codewhisperer:eu-central-1:123:profile/STATE","profile_name":"dev"}"#),
    );

    let credentials = KiroOAuthClient::import_cli_database(&path).unwrap();

    assert_eq!(
        profile_arn_from_credentials(&credentials).unwrap(),
        "arn:aws:codewhisperer:us-west-2:123:profile/TOKEN"
    );
}

#[test]
fn rejects_identity_center_import_without_any_profile_arn() {
    let dir = TempDir::new().unwrap();
    let path = identity_center_cli_database(&dir, IDENTITY_CENTER_TOKEN, None);

    let error = KiroOAuthClient::import_cli_database(&path).unwrap_err();

    assert!(error.to_string().contains("no profile ARN"), "{error}");
}

#[test]
fn ignores_malformed_cli_state_profile() {
    let dir = TempDir::new().unwrap();
    let path = identity_center_cli_database(&dir, IDENTITY_CENTER_TOKEN, Some("not json"));

    let error = KiroOAuthClient::import_cli_database(&path).unwrap_err();

    assert!(error.to_string().contains("no profile ARN"), "{error}");
}

#[test]
fn builds_sso_refresh_body_with_camel_case_json() {
    let body = sso_refresh_body("client", "secret", "old-refresh");

    assert_eq!(body["grantType"], "refresh_token");
    assert_eq!(body["clientId"], "client");
    assert_eq!(body["clientSecret"], "secret");
    assert_eq!(body["refreshToken"], "old-refresh");
    assert_eq!(
        sso_token_url("us-east-1").unwrap(),
        "https://oidc.us-east-1.amazonaws.com/token"
    );
}

#[test]
fn parses_kiro_camel_case_token_response_shape() {
    let token: KiroTokenResponse = serde_json::from_value(serde_json::json!({
        "accessToken": "access",
        "refreshToken": "refresh",
        "expiresIn": 3600,
        "profileArn": "arn:aws:codewhisperer:us-east-1:123:profile/test"
    }))
    .unwrap();

    assert_eq!(token.access_token, "access");
    assert_eq!(token.refresh_token.as_deref(), Some("refresh"));
    assert_eq!(token.expires_in, 3600);
    assert_eq!(
        token.profile_arn.as_deref(),
        Some("arn:aws:codewhisperer:us-east-1:123:profile/test")
    );
}

#[test]
fn rejects_region_injection() {
    assert!(validate_region("us-east-1").is_ok());
    assert!(validate_region("us-east-1.example.com").is_err());
}

#[test]
fn parses_cli_model_ids_from_list_models_json() {
    // Captured shape of `kiro-cli chat --list-models --format json`.
    let payload = r#"{
        "models": [
            {"model_name": "auto", "model_id": "auto", "context_window_tokens": 1000000, "rate_multiplier": 1.0},
            {"model_name": "claude-opus-5.5", "model_id": "claude-opus-5.5", "context_window_tokens": 1000000, "rate_multiplier": 2.0},
            {"model_name": "gpt-5.6-sol", "model_id": "gpt-5.6-sol", "context_window_tokens": 1000000, "rate_multiplier": 4.4}
        ],
        "default_model": "auto"
    }"#;

    let ids = parse_cli_model_ids(payload).unwrap();

    assert_eq!(ids, vec!["auto", "claude-opus-5.5", "gpt-5.6-sol"]);
}

#[test]
fn parses_cli_model_ids_accepts_camel_case_alias() {
    let payload = r#"{"models": [{"modelId": "claude-sonnet-5"}, {"modelId": "glm-5"}]}"#;

    let ids = parse_cli_model_ids(payload).unwrap();

    assert_eq!(ids, vec!["claude-sonnet-5", "glm-5"]);
}

#[test]
fn parses_cli_model_ids_dedupes_and_drops_blank_ids() {
    let payload = r#"{
        "models": [
            {"model_id": "auto"},
            {"model_id": "  "},
            {"model_id": "claude-opus-5"},
            {"model_id": "auto"}
        ]
    }"#;

    let ids = parse_cli_model_ids(payload).unwrap();

    assert_eq!(ids, vec!["auto", "claude-opus-5"]);
}

#[test]
fn parse_cli_model_ids_rejects_invalid_json() {
    let error = parse_cli_model_ids("not json").unwrap_err();

    assert!(
        error.to_string().contains("could not parse Kiro CLI model list"),
        "{error}"
    );
}

#[test]
fn parse_cli_model_ids_rejects_empty_model_list() {
    let error = parse_cli_model_ids(r#"{"models": []}"#).unwrap_err();

    assert!(
        error.to_string().contains("did not contain any model ids"),
        "{error}"
    );
}
