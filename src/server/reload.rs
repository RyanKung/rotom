//! Prepare a complete immutable router, then publish it in one short write.
//!
//! Invariant: each request owns exactly one router generation. Preparing a
//! candidate never mutates the published generation or the shared request stores.

use super::AppState;
#[cfg(any(unix, test))]
use super::{resolve_model_list_for_upstreams, router};
#[cfg(any(unix, test))]
use crate::Error;
use crate::{
    Result,
    config::{AppConfig, AppConfigStore},
};
#[cfg(any(unix, test))]
use axum::{
    Router,
    extract::{Request, State},
    response::Response,
};
use std::net::SocketAddr;
#[cfg(any(unix, test))]
use std::sync::Arc;
#[cfg(any(unix, test))]
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
};
#[cfg(any(unix, test))]
use tower::ServiceExt;

#[cfg(unix)]
mod control;
#[cfg(test)]
mod tests;

/// Failures specific to the atomic reload protocol.
#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    /// A field outside the hot-reload scope changed.
    #[error("bind_host, bind_port, auth_file and provider changes require a restart")]
    RestartRequired,
    /// Missing files are not interpreted as a request to reset running settings.
    #[error("configuration file is missing; old configuration retained")]
    MissingConfig,
    /// Model identifiers must be usable without silent normalization.
    #[error("model IDs, aliases and fallback must be non-empty and contain no whitespace")]
    InvalidModelId,
    /// Local authentication keys must fit in HTTP headers.
    #[error("local API key must not contain control characters")]
    InvalidApiKey,
    /// No candidate was committed within the preparation deadline.
    #[error("reload preparation timed out; old configuration retained")]
    PreparationTimeout,
    /// Each configuration home has exactly one reload endpoint.
    #[error("reload socket is already in use; use a separate ROTOM_HOME for each server")]
    ControlInUse,
    /// Only the reload command is accepted on the private socket.
    #[error("unknown control command")]
    InvalidCommand,
    /// A daemon reported a rejected candidate over the control protocol.
    #[error("reload rejected: {0}")]
    Rejected(String),
    /// This platform has no supported local control transport.
    #[error("daemon reload requires Unix local sockets")]
    UnsupportedPlatform,
}

/// Configuration source and startup overrides retained across reloads.
pub struct ReloadSettings {
    /// Exact configuration file used at startup.
    pub store: AppConfigStore,
    /// Startup values for fields that require restart.
    pub startup_config: AppConfig,
    /// Explicit CLI/environment API key, which takes precedence over the file.
    pub api_key_override: Option<String>,
    /// Explicit CLI/environment fallback, which takes precedence over the file.
    pub fallback_override: Option<String>,
    /// Built-in fallback when neither the file nor startup options specify one.
    pub default_fallback: String,
}

#[cfg(any(unix, test))]
struct Runtime {
    published: RwLock<Router>,
    // Shared token managers and response/batch stores survive every generation.
    base: AppState,
    settings: ReloadSettings,
    reload_lock: Mutex<()>,
}

#[cfg(any(unix, test))]
impl Runtime {
    fn new(base: AppState, settings: ReloadSettings) -> Self {
        Self {
            published: RwLock::new(router(base.clone())),
            base,
            settings,
            reload_lock: Mutex::new(()),
        }
    }

    async fn reload(&self) -> Result<()> {
        // Serialize prepare + publish, so a slower reload cannot replace a newer one.
        let _guard = self.reload_lock.lock().await;
        let candidate = tokio::time::timeout(std::time::Duration::from_secs(45), self.prepare())
            .await
            .map_err(|_| Error::from(ReloadError::PreparationTimeout))??;
        let candidate = router(candidate);
        *self.published.write().await = candidate;
        Ok(())
    }

    async fn prepare(&self) -> Result<AppState> {
        let config = self
            .settings
            .store
            .load()?
            .ok_or_else(|| Error::from(ReloadError::MissingConfig))?;
        validate_reload(&self.settings.startup_config, &config)?;
        let models = resolve_model_list_for_upstreams(&self.base.upstreams, Some(&config)).await?;
        let api_key = self
            .settings
            .api_key_override
            .clone()
            .or_else(|| config.api_key.clone());
        let fallback = self
            .settings
            .fallback_override
            .clone()
            .or_else(|| config.model_fallback.clone())
            .or_else(|| Some(self.settings.default_fallback.clone()));
        let mut candidate = AppState::new_multi_with_forwards(
            self.base.upstreams.as_ref().clone(),
            api_key,
            models,
            fallback,
            config.model_aliases,
            config.forwards,
        )?;
        candidate.responses = self.base.responses.clone();
        candidate.batches = self.base.batches.clone();
        Ok(candidate)
    }
}

#[cfg(any(unix, test))]
fn validate_reload(startup: &AppConfig, next: &AppConfig) -> Result<()> {
    if startup.bind_host != next.bind_host
        || startup.bind_port != next.bind_port
        || startup.auth_file != next.auth_file
        || startup.provider != next.provider
    {
        return Err(ReloadError::RestartRequired.into());
    }
    for id in next
        .models
        .values()
        .flatten()
        .chain(next.model_aliases.keys())
        .chain(next.model_aliases.values())
        .chain(next.model_fallback.iter())
    {
        if id.is_empty() || id.chars().any(char::is_whitespace) || id.chars().any(char::is_control)
        {
            return Err(ReloadError::InvalidModelId.into());
        }
    }
    if next
        .api_key
        .as_ref()
        .is_some_and(|key| key.chars().any(char::is_control))
    {
        return Err(ReloadError::InvalidApiKey.into());
    }
    Ok(())
}

#[cfg(any(unix, test))]
async fn dispatch(State(runtime): State<Arc<Runtime>>, request: Request) -> Response {
    // Drop the lock before reading the request body or contacting an upstream.
    let snapshot = runtime.published.read().await.clone();
    match snapshot.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// Serves API listeners with atomic configuration reload through a private local socket.
///
/// # Errors
/// Returns an error if listeners or the control socket cannot be bound or served.
#[cfg(unix)]
pub async fn serve_reloadable(
    addrs: &[SocketAddr],
    state: AppState,
    settings: ReloadSettings,
) -> Result<()> {
    if addrs.is_empty() {
        return Err(Error::config("at least one bind address is required"));
    }
    let runtime = Arc::new(Runtime::new(state, settings));
    let mut listeners = Vec::with_capacity(addrs.len());
    for addr in addrs {
        listeners.push(TcpListener::bind(addr).await?);
    }
    let app = Router::new()
        .fallback(dispatch)
        .with_state(Arc::clone(&runtime));
    let servers = listeners.into_iter().map(|listener| {
        let app = app.clone();
        async move { axum::serve(listener, app).await }
    });
    let control = control::ControlSocket::bind(runtime.settings.store.path())?;
    tokio::select! {
        result = futures_util::future::try_join_all(servers) => { result?; }
        result = control.serve(&runtime) => { result?; }
    }
    Ok(())
}

/// Serves ordinary API listeners on platforms without a local reload transport.
///
/// # Errors
/// Returns an error if API listeners cannot be bound or served.
#[cfg(not(unix))]
pub async fn serve_reloadable(
    addrs: &[SocketAddr],
    state: AppState,
    _settings: ReloadSettings,
) -> Result<()> {
    super::serve_all(addrs, state).await
}

/// Requests reload and waits for an explicit success or failure acknowledgement.
///
/// # Errors
/// Returns an error for an unavailable daemon, rejected configuration, or control IO.
pub fn request_reload() -> Result<()> {
    #[cfg(unix)]
    {
        control::request(AppConfigStore::default_path()?.as_path())?;
        println!("reloaded runtime configuration; active requests were preserved");
        Ok(())
    }
    #[cfg(not(unix))]
    Err(ReloadError::UnsupportedPlatform.into())
}
