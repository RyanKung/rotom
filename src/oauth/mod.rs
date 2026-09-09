mod callback;
mod grok;
mod kiro;
mod pkce;

use crate::{
    Error, Result,
    config::{Credentials, Provider, now_unix},
    oauth::pkce::Pkce,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

pub use callback::{CallbackOutcome, CallbackServer};
pub use grok::{GrokOAuthClient, XAI_API_BASE_URL};
pub use kiro::{
    KIRO_CLI_DATABASE_RELATIVE_PATH, KIRO_DESKTOP_TOKEN_RELATIVE_PATH, KiroAuthorizationCallback,
    KiroOAuthClient, api_region_from_credentials, default_cli_database_path,
    default_desktop_token_path, parse_kiro_authorization_callback, profile_arn_from_credentials,
};
pub use pkce::{code_challenge, generate_pkce};

/// OAuth client identifier used by the Codex desktop-style authorization flow.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Authorization endpoint used to start the browser-based OAuth flow.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// Token endpoint used for authorization code exchange and refresh requests.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Local callback URL registered for the OAuth authorization code flow.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// OAuth scopes requested for the local Codex session.
pub const SCOPE: &str = "openid profile email offline_access";

const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone, PartialEq, Eq)]
/// Values needed to complete an OAuth authorization code flow with PKCE.
pub struct AuthorizationFlow {
    /// PKCE verifier that must be supplied during code exchange.
    pub verifier: String,
    /// CSRF protection token that must match the callback payload.
    pub state: String,
    /// Fully constructed authorization URL to open in the browser.
    pub authorize_url: Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Normalized authorization data parsed from CLI or callback input.
pub struct AuthorizationInput {
    /// Authorization code returned by the OAuth provider, if present.
    pub code: Option<String>,
    /// State value returned by the OAuth provider, if present.
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

#[derive(Clone)]
/// Minimal OAuth client for exchanging and refreshing Codex credentials.
pub struct CodexOAuthClient {
    http: Client,
    token_url: String,
}

impl Default for CodexOAuthClient {
    fn default() -> Self {
        Self::new(Client::new())
    }
}

impl CodexOAuthClient {
    /// Creates an OAuth client backed by the provided HTTP client.
    #[must_use]
    pub fn new(http: Client) -> Self {
        Self {
            http,
            token_url: TOKEN_URL.to_owned(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_token_url(http: Client, token_url: impl Into<String>) -> Self {
        Self {
            http,
            token_url: token_url.into(),
        }
    }

    /// Exchanges an authorization code plus PKCE verifier for stored credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP exchange fails or the OAuth server
    /// rejects the authorization code.
    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
    ) -> Result<Credentials> {
        let response = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .await?;

        parse_token_response(response, "code exchange").await
    }

    /// Refreshes an existing refresh token and returns replacement credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP exchange fails or the OAuth server
    /// rejects the refresh token.
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<Credentials> {
        let response = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await?;

        parse_token_response(response, "refresh").await
    }
}

/// Creates a browser-ready OAuth flow and the local state needed to validate it.
///
/// # Errors
///
/// Returns an error when the authorization URL cannot be constructed.
pub fn create_authorization_flow(originator: &str) -> Result<AuthorizationFlow> {
    let mut rng = StdRng::from_os_rng();
    let Pkce {
        verifier,
        challenge,
    } = generate_pkce(&mut rng);
    let state = create_state(&mut rng);
    let mut authorize_url = Url::parse(AUTHORIZE_URL)?;
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", originator);

    Ok(AuthorizationFlow {
        verifier,
        state,
        authorize_url,
    })
}

/// Parses authorization input from a full callback URL, query string, or raw code.
#[must_use]
pub fn parse_authorization_input(input: &str) -> AuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return AuthorizationInput {
            code: None,
            state: None,
        };
    }

    if let Ok(url) = Url::parse(value) {
        return AuthorizationInput {
            code: query_value(&url, "code"),
            state: query_value(&url, "state"),
        };
    }

    // Support the shell-friendly `code#state` format printed by some helpers.
    if let Some((code, state)) = value.split_once('#') {
        return AuthorizationInput {
            code: non_empty(code),
            state: non_empty(state),
        };
    }

    if value.contains("code=") {
        return parse_query_like_input(value);
    }

    AuthorizationInput {
        code: Some(value.to_owned()),
        state: None,
    }
}

/// Extracts the `ChatGPT` account identifier embedded in an OAuth access token.
///
/// # Errors
///
/// Returns an error when the token is not a valid JWT payload or does not
/// contain the expected account-id claim.
pub fn account_id_from_access_token(access_token: &str) -> Result<String> {
    let payload = decode_jwt_payload(access_token)?;
    let account_id = payload
        .get(JWT_CLAIM_PATH)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::oauth("access token is missing chatgpt_account_id"))?;

    Ok(account_id.to_owned())
}

fn create_state(rng: &mut impl RngCore) -> String {
    let mut bytes = [0_u8; 16];
    rng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn query_value(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

fn parse_query_like_input(value: &str) -> AuthorizationInput {
    let query = value
        .split_once('?')
        .map_or(value, |(_, query)| query)
        .trim_start_matches('?');
    // Ignore any fragment suffix so pasted callback URLs and raw query strings behave the same.
    let query = query.split_once('#').map_or(query, |(query, _)| query);
    let pairs = url::form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();

    AuthorizationInput {
        code: pairs
            .iter()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string()),
        state: pairs
            .iter()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.to_string()),
    }
}

fn decode_jwt_payload(token: &str) -> Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::oauth("invalid JWT access token"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Error::oauth("invalid JWT payload encoding"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn parse_token_response(response: reqwest::Response, operation: &str) -> Result<Credentials> {
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::oauth(format!(
            "OAuth {operation} failed with status {status}: {text}"
        )));
    }

    let token = response.json::<TokenResponse>().await?;
    if token.expires_in <= 0 {
        return Err(Error::oauth("OAuth token response has invalid expires_in"));
    }

    // The Codex API expects the account id in a header, so normalize it once here.
    let account_id = account_id_from_access_token(&token.access_token)?;
    Ok(Credentials {
        provider: Provider::Codex,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now_unix().saturating_add(token.expires_in),
        account_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Form, Json, Router, extract::State, routing::post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::Client;
    use serde_json::json;
    use std::{collections::HashMap, sync::Arc};
    use tokio::{net::TcpListener, sync::Mutex};

    type RefreshForm = HashMap<String, String>;
    type RefreshFormState = Arc<Mutex<Option<RefreshForm>>>;

    fn jwt_with_payload(payload: &Value) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("header.{encoded}.sig")
    }

    fn jwt_with_account_id(account_id: &str) -> String {
        jwt_with_payload(&json!({
            JWT_CLAIM_PATH: { "chatgpt_account_id": account_id }
        }))
    }

    async fn refresh_handler(
        State(last_form): State<RefreshFormState>,
        Form(form): Form<RefreshForm>,
    ) -> Json<Value> {
        *last_form.lock().await = Some(form);
        Json(json!({
            "access_token": jwt_with_account_id("acc_refreshed"),
            "refresh_token": "new_refresh",
            "expires_in": 3600
        }))
    }

    async fn spawn_refresh_server() -> (String, RefreshFormState) {
        let last_form = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/token", post(refresh_handler))
            .with_state(last_form.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (url, last_form)
    }

    #[test]
    fn builds_openclaw_compatible_authorization_url() {
        let flow = create_authorization_flow("pi").unwrap();
        let pairs = flow
            .authorize_url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(pairs.get("client_id").unwrap(), CLIENT_ID);
        assert_eq!(pairs.get("redirect_uri").unwrap(), REDIRECT_URI);
        assert_eq!(pairs.get("scope").unwrap(), SCOPE);
        assert_eq!(pairs.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(pairs.get("codex_cli_simplified_flow").unwrap(), "true");
        assert_eq!(pairs.get("originator").unwrap(), "pi");
        assert!(!flow.verifier.is_empty());
        assert_eq!(flow.state.len(), 32);
    }

    #[test]
    fn parses_full_redirect_url() {
        let input = "http://localhost:1455/auth/callback?code=abc&state=xyz";

        assert_eq!(
            parse_authorization_input(input),
            AuthorizationInput {
                code: Some("abc".into()),
                state: Some("xyz".into())
            }
        );
    }

    #[test]
    fn parses_code_hash_state() {
        assert_eq!(
            parse_authorization_input("abc#xyz"),
            AuthorizationInput {
                code: Some("abc".into()),
                state: Some("xyz".into())
            }
        );
    }

    #[test]
    fn parses_callback_address_without_scheme() {
        assert_eq!(
            parse_authorization_input("localhost:1455/auth/callback?code=abc&state=xyz"),
            AuthorizationInput {
                code: Some("abc".into()),
                state: Some("xyz".into())
            }
        );
    }

    #[test]
    fn parses_raw_query_string() {
        assert_eq!(
            parse_authorization_input("code=abc&state=xyz"),
            AuthorizationInput {
                code: Some("abc".into()),
                state: Some("xyz".into())
            }
        );
    }

    #[test]
    fn extracts_account_id_from_jwt() {
        let token = jwt_with_payload(&json!({
            JWT_CLAIM_PATH: { "chatgpt_account_id": "acc_123" }
        }));

        assert_eq!(account_id_from_access_token(&token).unwrap(), "acc_123");
    }

    #[test]
    fn rejects_jwt_without_account_id() {
        let token = jwt_with_payload(&json!({ "sub": "user" }));

        assert!(account_id_from_access_token(&token).is_err());
    }

    #[tokio::test]
    async fn refresh_token_posts_refresh_grant_and_parses_credentials() {
        let (token_url, last_form) = spawn_refresh_server().await;
        let client = CodexOAuthClient::new_with_token_url(Client::new(), token_url);

        let credentials = client.refresh_token("old_refresh").await.unwrap();

        assert_eq!(credentials.refresh_token, "new_refresh");
        assert_eq!(credentials.account_id, "acc_refreshed");

        let form = last_form.lock().await.clone().unwrap();
        assert_eq!(form.get("grant_type").unwrap(), "refresh_token");
        assert_eq!(form.get("refresh_token").unwrap(), "old_refresh");
        assert_eq!(form.get("client_id").unwrap(), CLIENT_ID);
    }
}
