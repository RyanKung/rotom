use crate::{
    Error, Result,
    config::{AuthStore, Credentials, Provider},
    oauth::{CodexOAuthClient, GrokOAuthClient, KiroOAuthClient},
};
use std::sync::Arc;
use tokio::sync::RwLock;

const REFRESH_SKEW_SECS: i64 = 60;

/// Keeps OAuth credentials cached in memory and refreshes them when needed.
#[derive(Clone)]
pub struct TokenManager {
    store: AuthStore,
    oauth: OAuthClient,
    cached: Arc<RwLock<Option<Credentials>>>,
}

impl TokenManager {
    /// Creates a token manager backed by the given credential store and OAuth client.
    #[must_use]
    pub fn new(store: AuthStore, oauth: CodexOAuthClient) -> Self {
        Self::new_with_oauth(store, OAuthClient::Codex(oauth))
    }

    /// Creates a token manager for the selected provider using default HTTP clients.
    ///
    /// # Errors
    ///
    /// Returns an error if the stored credentials belong to a different provider.
    #[must_use]
    pub fn new_for_provider(store: AuthStore, provider: Provider, http: reqwest::Client) -> Self {
        let oauth = match provider {
            Provider::Codex => OAuthClient::Codex(CodexOAuthClient::new(http)),
            Provider::Grok => OAuthClient::Grok(GrokOAuthClient::new(http)),
            Provider::Kiro => OAuthClient::Kiro(KiroOAuthClient::new(http)),
        };
        Self::new_with_oauth(store, oauth)
    }

    /// Creates a token manager backed by a provider-specific OAuth client.
    #[must_use]
    pub fn new_with_oauth(store: AuthStore, oauth: OAuthClient) -> Self {
        let cached = store.load_provider(oauth.provider()).unwrap_or(None);
        Self {
            store,
            oauth,
            cached: Arc::new(RwLock::new(cached)),
        }
    }

    /// Returns the currently cached credentials without forcing a refresh.
    #[must_use]
    pub async fn credentials_snapshot(&self) -> Option<Credentials> {
        self.cached.read().await.clone()
    }

    /// Refreshes the current credentials immediately and persists the new tokens.
    ///
    /// # Errors
    ///
    /// Returns an error when no credentials are stored locally, the refresh
    /// request fails, or the refreshed credentials cannot be saved.
    pub async fn refresh(&self) -> Result<Credentials> {
        let mut guard = self.cached.write().await;
        let credentials = guard
            .clone()
            .or(self.store.load_provider(self.oauth.provider())?)
            .ok_or_else(|| {
                Error::config("not logged in; run `rotom login` before refreshing tokens")
            })?;

        let refreshed = self.refresh_credentials(&credentials).await?;
        self.store.save(&refreshed)?;
        *guard = Some(refreshed.clone());
        drop(guard);
        Ok(refreshed)
    }

    /// Returns valid credentials, refreshing them when they are near expiry.
    ///
    /// # Errors
    ///
    /// Returns an error when no credentials are stored locally, the refresh
    /// request fails, or persisted credentials cannot be loaded or saved.
    pub async fn credentials(&self) -> Result<Credentials> {
        // Check the shared cache first so uncontended reads avoid disk I/O and refresh traffic.
        let cached_credentials = self.cached.read().await.clone();
        if let Some(credentials) = cached_credentials {
            if !credentials.is_expired(REFRESH_SKEW_SECS) {
                return Ok(credentials);
            }
        }

        let mut guard = self.cached.write().await;
        let Some(credentials) = guard
            .clone()
            .or(self.store.load_provider(self.oauth.provider())?)
        else {
            return Err(Error::config(
                "not logged in; run `rotom login` before serving requests",
            ));
        };

        // Refresh slightly before the exact expiry time so callers do not race a token timeout.
        if !credentials.is_expired(REFRESH_SKEW_SECS) {
            *guard = Some(credentials.clone());
            return Ok(credentials);
        }

        let refreshed = self.refresh_credentials(&credentials).await?;
        self.store.save(&refreshed)?;
        *guard = Some(refreshed.clone());
        drop(guard);
        Ok(refreshed)
    }

    async fn refresh_credentials(&self, credentials: &Credentials) -> Result<Credentials> {
        self.oauth.refresh_token(&credentials.refresh_token).await
    }
}

/// Provider-specific OAuth refresh client used by [`TokenManager`].
#[derive(Clone)]
pub enum OAuthClient {
    /// `OpenAI` Codex OAuth refresh client.
    Codex(CodexOAuthClient),
    /// xAI Grok OAuth refresh client.
    Grok(GrokOAuthClient),
    /// Kiro imported credential refresh client.
    Kiro(KiroOAuthClient),
}

impl OAuthClient {
    const fn provider(&self) -> Provider {
        match self {
            Self::Codex(_) => Provider::Codex,
            Self::Grok(_) => Provider::Grok,
            Self::Kiro(_) => Provider::Kiro,
        }
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<Credentials> {
        match self {
            Self::Codex(client) => client.refresh_token(refresh_token).await,
            Self::Grok(client) => client.refresh_token(refresh_token).await,
            Self::Kiro(client) => client.refresh_token(refresh_token).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::now_unix;
    use crate::testsupport::TempDir;
    use axum::{Form, Json, Router, routing::post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    fn jwt_with_payload(payload: &Value) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("header.{encoded}.sig")
    }

    fn jwt_with_account_id(account_id: &str) -> String {
        jwt_with_payload(&json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
        }))
    }

    async fn refresh_handler(Form(form): Form<HashMap<String, String>>) -> Json<Value> {
        assert_eq!(form.get("refresh_token").unwrap(), "old_refresh");
        Json(json!({
            "access_token": jwt_with_account_id("acc_refreshed"),
            "refresh_token": "new_refresh",
            "expires_in": 3600
        }))
    }

    async fn spawn_refresh_server() -> String {
        let app = Router::new().route("/token", post(refresh_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    #[test]
    fn manager_loads_missing_store_without_panic() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let manager = TokenManager::new(store, CodexOAuthClient::default());

        assert!(manager.cached.blocking_read().is_none());
    }

    #[tokio::test]
    async fn manager_returns_unexpired_credentials() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let credentials = Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc".into(),
        };
        store.save(&credentials).unwrap();
        let manager = TokenManager::new(store, CodexOAuthClient::default());

        assert_eq!(manager.credentials().await.unwrap(), credentials);
    }

    #[tokio::test]
    async fn manager_refreshes_expired_credentials_and_saves_them() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "old_access".into(),
                refresh_token: "old_refresh".into(),
                expires_at: now_unix() - 1,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let oauth =
            CodexOAuthClient::new_with_token_url(Client::new(), spawn_refresh_server().await);
        let manager = TokenManager::new(store.clone(), oauth);

        let credentials = manager.credentials().await.unwrap();

        assert_eq!(credentials.refresh_token, "new_refresh");
        assert_eq!(credentials.account_id, "acc_refreshed");
        assert_eq!(store.load().unwrap(), Some(credentials));
    }

    #[tokio::test]
    async fn manager_refresh_forces_refresh_even_when_credentials_are_unexpired() {
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
        let oauth =
            CodexOAuthClient::new_with_token_url(Client::new(), spawn_refresh_server().await);
        let manager = TokenManager::new(store.clone(), oauth);

        let credentials = manager.refresh().await.unwrap();

        assert_eq!(credentials.refresh_token, "new_refresh");
        assert_eq!(credentials.account_id, "acc_refreshed");
        assert_eq!(manager.credentials_snapshot().await, Some(credentials));
    }
}
