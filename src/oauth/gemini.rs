use crate::{
    Error, Result,
    config::{Credentials, Provider, now_unix},
};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

mod application;
const REDIRECT_URI: &str = "http://localhost:45289/oauth-callback";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal";
/// Antigravity generation and onboarding service.
pub const GENERATION_URL: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
/// Client protocol revision used for Antigravity compatibility.
const CLIENT_VERSION: &str = "2.28.2";

/// Antigravity wire identity for the host platform.
pub fn user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("antigravity/hub/{CLIENT_VERSION} {platform}/{architecture}")
}
const PROJECT_PREFIX: &str = "antigravity:";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";

/// Antigravity installed-application OAuth, exposed as the Gemini provider.
/// Clones share only the HTTP connection pool and immutable endpoint configuration.
#[derive(Clone)]
pub struct GeminiOAuthClient {
    http: Client,
    token_url: String,
    assist_url: String,
    generation_url: String,
    application: Option<std::sync::Arc<application::Application>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

impl Default for GeminiOAuthClient {
    fn default() -> Self {
        Self::new(Client::new())
    }
}

impl GeminiOAuthClient {
    /// Uses the caller's proxy-aware HTTP transport.
    #[must_use]
    pub fn new(http: Client) -> Self {
        Self {
            http,
            token_url: TOKEN_URL.into(),
            assist_url: ASSIST_URL.into(),
            generation_url: GENERATION_URL.into(),
            application: None,
        }
    }

    fn application(&self) -> Result<std::sync::Arc<application::Application>> {
        if let Some(application) = &self.application {
            return Ok(std::sync::Arc::clone(application));
        }
        application::load().map(std::sync::Arc::new)
    }

    /// Creates a consent URL with offline access, PKCE, and fresh CSRF state.
    /// # Errors
    /// Returns an error if the authorization URL cannot be constructed.
    pub fn create_authorization_flow(&self) -> Result<super::AuthorizationFlow> {
        let application = self.application()?;
        let mut rng = StdRng::from_os_rng();
        let pkce = super::generate_pkce(&mut rng);
        let mut random = [0_u8; 32];
        rng.fill_bytes(&mut random);
        let state = hex::encode(random);
        let mut authorize_url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth")?;
        authorize_url
            .query_pairs_mut()
            .append_pair("client_id", &application.client_id)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("response_type", "code")
            .append_pair("scope", SCOPE)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        Ok(super::AuthorizationFlow {
            verifier: pkce.verifier,
            state,
            authorize_url,
        })
    }

    /// Validates the full loopback redirect, requiring exactly one code and state.
    /// # Errors
    /// Rejects missing or mismatched state, wrong redirects, and denied consent.
    pub fn callback_code(callback: &str, expected_state: &str) -> Result<String> {
        let url = Url::parse(callback)?;
        let expected = Url::parse(REDIRECT_URI)?;
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::oauth("unexpected Gemini OAuth callback URL"));
        }
        let values = |key: &str| {
            url.query_pairs()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
                .collect::<Vec<_>>()
        };
        let states = values("state");
        if expected_state.is_empty()
            || states.len() != 1
            || states.first().map(String::as_str) != Some(expected_state)
        {
            return Err(Error::oauth("Gemini OAuth callback state mismatch"));
        }
        if !values("error").is_empty() {
            return Err(Error::oauth("Google authorization was denied"));
        }
        let codes = values("code");
        match codes.as_slice() {
            [code] if !code.is_empty() => Ok(code.clone()),
            _ => Err(Error::oauth(
                "Gemini OAuth callback must contain one authorization code",
            )),
        }
    }

    /// Exchanges consent for tokens. Persist these before calling `ensure_project`.
    /// # Errors
    /// Returns an error on rejected tokens, ineligible accounts, or failed onboarding.
    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
    ) -> Result<Credentials> {
        let application = self.application()?;
        let token = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("client_id", &application.client_id),
                ("client_secret", &application.client_secret),
                ("redirect_uri", REDIRECT_URI),
                ("code", code),
                ("code_verifier", verifier),
            ])
            .await?;
        credentials_from_token(token, None, PROJECT_PREFIX.into())
    }

    /// Refreshes access while retaining project identity and non-rotated refresh tokens.
    /// # Errors
    /// Returns an error on missing metadata or rejected/invalid token responses.
    pub async fn refresh_credentials(&self, previous: &Credentials) -> Result<Credentials> {
        if previous.provider != Provider::Gemini
            || !previous.account_id.starts_with(PROJECT_PREFIX)
            || previous.refresh_token.is_empty()
        {
            return Err(Error::oauth(
                "old or invalid Gemini credentials; run rotom login --provider gemini for Antigravity",
            ));
        }
        let application = self.application()?;
        let token = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &application.client_id),
                ("client_secret", &application.client_secret),
                ("refresh_token", &previous.refresh_token),
            ])
            .await?;
        credentials_from_token(
            token,
            Some(&previous.refresh_token),
            previous.account_id.clone(),
        )
    }

    /// Completes project setup without repeating OAuth. Pending tokens remain reusable.
    /// # Errors
    /// Returns an error for legacy credentials or unsuccessful account provisioning.
    pub async fn ensure_project(&self, mut credentials: Credentials) -> Result<Credentials> {
        let project = Self::project_id(&credentials)?;
        if project.is_empty() {
            let project = self.resolve_project(&credentials.access_token).await?;
            credentials.account_id = format!("{PROJECT_PREFIX}{project}");
        }
        Ok(credentials)
    }

    /// Extracts the project and rejects credentials issued to the retired CLI client.
    /// An empty project means OAuth succeeded but provisioning is still pending.
    /// # Errors
    /// Returns an error when the credential does not belong to Antigravity.
    pub fn project_id(credentials: &Credentials) -> Result<&str> {
        if credentials.provider != Provider::Gemini {
            return Err(Error::oauth("expected Gemini credentials"));
        }
        credentials.account_id.strip_prefix(PROJECT_PREFIX).ok_or_else(||
            Error::oauth("Gemini credentials use the retired client; run rotom login --provider gemini to migrate to Antigravity"))
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let response = self
            .http
            .post(&self.token_url)
            .timeout(Duration::from_secs(30))
            .form(form)
            .send()
            .await?;
        if !response.status().is_success() {
            // Token responses can contain credentials: never include the body in errors.
            return Err(Error::oauth(format!(
                "Google token exchange failed ({})",
                response.status()
            )));
        }
        Ok(response.json().await?)
    }

    async fn assist_post(&self, method: &str, token: &str, body: &Value) -> Result<Value> {
        let response = self
            .http
            .post(format!(
                "{}:{method}",
                if method == "onboardUser" {
                    &self.generation_url
                } else {
                    &self.assist_url
                }
            ))
            .timeout(Duration::from_secs(30))
            .bearer_auth(token)
            .header("User-Agent", user_agent())
            .header("X-Goog-Api-Client", "gl-node/22.21.1")
            .json(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::oauth(format!(
                "Gemini {method} failed ({}); check subscription eligibility",
                response.status()
            )));
        }
        Ok(response.json().await?)
    }

    async fn completed_project(
        &self,
        token: &str,
        operation: &Value,
        metadata: &Value,
    ) -> Result<String> {
        // A completed long-running operation can represent failure, not success.
        if let Some(error) = operation.get("error").filter(|value| !value.is_null()) {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unspecified failure");
            return Err(Error::oauth(format!(
                "Gemini onboarding failed (code {code}): {message}"
            )));
        }
        if let Some(project) = operation.get("response").and_then(managed_project) {
            return Ok(project.into());
        }
        // Some completion responses omit the project; read the resulting account state.
        let loaded = self
            .assist_post("loadCodeAssist", token, &json!({"metadata":metadata}))
            .await?;
        if let Some(project) = managed_project(&loaded) {
            return Ok(project.into());
        }
        let reasons = loaded
            .get("ineligibleTiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tier| tier.get("reasonMessage").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        Err(Error::oauth(format!(
            "Gemini onboarding returned no managed project after reloading account state. Complete account eligibility/project setup in the official Antigravity app. {reasons}"
        )))
    }

    async fn resolve_project(&self, token: &str) -> Result<String> {
        let metadata = json!({"ideType":"ANTIGRAVITY"});
        let loaded = self
            .assist_post("loadCodeAssist", token, &json!({"metadata":metadata}))
            .await?;
        if let Some(project) = managed_project(&loaded) {
            return Ok(project.into());
        }
        let tier = loaded
            .get("allowedTiers")
            .and_then(Value::as_array)
            .and_then(|tiers| {
                tiers
                    .iter()
                    .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
                    .or_else(|| tiers.first())
            })
            .and_then(|tier| tier.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && *id != "legacy-tier")
            .unwrap_or("free-tier");
        let request = json!({"tier_id":tier,"metadata":{"ide_type":"ANTIGRAVITY","ide_version":CLIENT_VERSION,"ide_name":"antigravity"}});
        for attempt in 0..5 {
            let operation = self.assist_post("onboardUser", token, &request).await?;
            if operation.get("error").is_some_and(|error| !error.is_null())
                || operation.get("done").and_then(Value::as_bool) == Some(true)
            {
                return self.completed_project(token, &operation, &metadata).await;
            }
            if attempt < 4 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        Err(Error::oauth(
            "Antigravity onboarding timed out; run rotom refresh --provider gemini to retry without authorizing again",
        ))
    }
}

fn managed_project(value: &Value) -> Option<&str> {
    let project = value.get("cloudaicompanionProject")?;
    project
        .as_str()
        .or_else(|| project.get("id").and_then(Value::as_str))
        .filter(|id| !id.trim().is_empty())
}

fn credentials_from_token(
    token: TokenResponse,
    fallback: Option<&str>,
    project: String,
) -> Result<Credentials> {
    let refresh_token = token
        .refresh_token
        .filter(|s| !s.is_empty())
        .or_else(|| fallback.map(str::to_owned))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::oauth("Google did not return a refresh token; grant offline consent again")
        })?;
    if token.access_token.is_empty() || token.expires_in <= 0 {
        return Err(Error::oauth("invalid Google token response"));
    }
    Ok(Credentials {
        provider: Provider::Gemini,
        access_token: token.access_token,
        refresh_token,
        expires_at: now_unix().saturating_add(token.expires_in),
        account_id: project,
    })
}

#[cfg(test)]
mod tests;
