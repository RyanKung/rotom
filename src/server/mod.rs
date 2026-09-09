//! HTTP server state, router wiring, and endpoint tests.

use crate::{
    codex::{client::CodexClient, convert::normalize_model},
    config::Provider,
    error::{Error, Result},
    models::provider_for_model,
    openai::response::ModelList,
    status::StatusClient,
    token::TokenManager,
};
use axum::{
    Router,
    extract::Request,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use futures_util::future::try_join_all;
use reqwest::Client;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;

mod auth;
mod handlers;
mod status_response;
mod store;

pub use auth::authorize;

/// Shared application state used by all HTTP route handlers.
#[derive(Clone)]
pub struct AppState {
    upstreams: Arc<Vec<UpstreamState>>,
    status: StatusClient,
    api_key: Option<Arc<str>>,
    models: ModelList,
    model_fallback: Option<Arc<str>>,
    responses: store::ResponseStore,
    batches: store::BatchStore,
}

#[derive(Clone)]
/// Runtime state for one authenticated upstream provider.
pub struct UpstreamState {
    /// Provider served by this upstream.
    pub provider: Provider,
    /// Token manager for this provider.
    pub token_manager: TokenManager,
    /// HTTP client for this provider.
    pub client: CodexClient,
}

impl AppState {
    /// Builds application state from the configured token manager, upstream client, and model list.
    #[must_use]
    pub fn new(
        token_manager: TokenManager,
        codex: CodexClient,
        api_key: Option<String>,
        models: ModelList,
    ) -> Self {
        Self::new_with_model_fallback(token_manager, codex, api_key, models, None)
    }

    /// Builds application state and optionally enables Anthropic model fallback.
    #[must_use]
    pub fn new_with_model_fallback(
        token_manager: TokenManager,
        codex: CodexClient,
        api_key: Option<String>,
        models: ModelList,
        model_fallback: Option<String>,
    ) -> Self {
        Self::new_multi_with_model_fallback(
            vec![UpstreamState {
                provider: codex.provider(),
                token_manager,
                client: codex,
            }],
            api_key,
            models,
            model_fallback,
        )
    }

    /// Builds application state from all configured upstream providers.
    ///
    /// # Panics
    ///
    /// Panics when called with an empty upstream list.
    #[must_use]
    pub fn new_multi_with_model_fallback(
        upstreams: Vec<UpstreamState>,
        api_key: Option<String>,
        models: ModelList,
        model_fallback: Option<String>,
    ) -> Self {
        let codex = upstreams
            .iter()
            .find(|upstream| upstream.provider == Provider::Codex)
            .or_else(|| upstreams.first())
            .expect("at least one upstream provider is required");
        let status = StatusClient::new(Client::new(), codex.client.base_url().to_owned());
        Self {
            upstreams: Arc::new(upstreams),
            status,
            api_key: api_key.map(Arc::from),
            models,
            model_fallback: model_fallback.map(Arc::from),
            responses: store::ResponseStore::default(),
            batches: store::BatchStore::default(),
        }
    }

    /// Rewrites known unsupported Anthropic model ids to the configured fallback.
    pub(crate) fn rewrite_model(&self, model: &mut String) {
        if provider_for_model(model).is_none() {
            return;
        }
        let Some(fallback) = self.model_fallback.as_deref() else {
            return;
        };

        let normalized = normalize_model(model);
        if self.supports_model(&normalized) || !looks_like_anthropic_model(&normalized) {
            return;
        }

        if !self.supports_model(fallback) {
            tracing::warn!(
                requested_model = %model,
                fallback_model = %fallback,
                "model fallback ignored"
            );
            return;
        }

        tracing::warn!(
            requested_model = %model,
            fallback_model = %fallback,
            "model fallback applied"
        );
        fallback.clone_into(model);
    }

    fn supports_model(&self, candidate: &str) -> bool {
        let candidate = normalize_model_for_support(candidate);
        self.models
            .data
            .iter()
            .any(|model| normalize_model_for_support(&model.id) == candidate)
    }

    pub(crate) fn upstream_for_model(&self, model: &str) -> Option<&UpstreamState> {
        let provider = provider_for_model(model)?;
        self.upstreams
            .iter()
            .find(|upstream| upstream.provider == provider)
            .or_else(|| {
                (provider == Provider::Codex)
                    .then(|| self.upstreams.first())
                    .flatten()
            })
    }

    pub(crate) fn default_upstream(&self) -> Option<&UpstreamState> {
        self.upstreams.first()
    }

    pub(crate) fn upstream_for_provider(&self, provider: Provider) -> Option<&UpstreamState> {
        self.upstreams
            .iter()
            .find(|upstream| upstream.provider == provider)
    }
}

fn looks_like_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-")
        || model.eq_ignore_ascii_case("sonnet")
        || model.eq_ignore_ascii_case("opus")
        || model.eq_ignore_ascii_case("haiku")
        || model.contains("sonnet")
        || model.contains("opus")
        || model.contains("haiku")
}

fn normalize_model_for_support(model: &str) -> String {
    let normalized = normalize_model(model);
    normalized
        .strip_prefix("xai/")
        .or_else(|| normalized.strip_prefix("grok/"))
        .or_else(|| normalized.strip_prefix("kiro/"))
        .unwrap_or(&normalized)
        .to_owned()
}

/// Binds the local listener and serves the Axum router until shutdown.
///
/// # Errors
///
/// Returns an error when binding the socket or serving the router fails.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    serve_all(&[addr], state).await
}

/// Binds one local listener per address and serves the Axum router until shutdown.
///
/// # Errors
///
/// Returns an error when no addresses are provided, binding any socket fails,
/// or serving any router fails.
pub async fn serve_all(addrs: &[SocketAddr], state: AppState) -> Result<()> {
    if addrs.is_empty() {
        return Err(Error::config("at least one bind address is required"));
    }

    let mut listeners = Vec::with_capacity(addrs.len());
    for addr in addrs {
        listeners.push(TcpListener::bind(addr).await?);
    }

    let servers = listeners.into_iter().map(|listener| {
        let state = state.clone();
        async move { axum::serve(listener, router(state)).await }
    });
    try_join_all(servers).await?;
    Ok(())
}

/// Builds the HTTP router for all supported endpoints.
pub fn router(state: AppState) -> Router {
    // Keep route registration centralized so the server surface stays easy to audit.
    Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/auth/refresh", post(handlers::manual_refresh))
        .route("/v1/status", get(handlers::status))
        .route("/v1/models", get(handlers::models))
        .route("/v1/responses", post(handlers::responses))
        .route("/v1/responses/compact", post(handlers::compact_response))
        .route(
            "/v1/responses/input_tokens",
            post(handlers::count_response_input_tokens),
        )
        .route(
            "/v1/responses/{response_id}",
            get(handlers::get_response).delete(handlers::delete_response),
        )
        .route(
            "/v1/responses/{response_id}/cancel",
            post(handlers::cancel_response),
        )
        .route(
            "/v1/responses/{response_id}/input_items",
            get(handlers::list_response_input_items),
        )
        .route("/v1/messages", post(handlers::messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::count_message_tokens),
        )
        .route(
            "/v1/messages/batches",
            get(handlers::list_message_batches).post(handlers::create_message_batch),
        )
        .route(
            "/v1/messages/batches/{batch_id}",
            get(handlers::get_message_batch).delete(handlers::delete_message_batch),
        )
        .route(
            "/v1/messages/batches/{batch_id}/cancel",
            post(handlers::cancel_message_batch),
        )
        .route(
            "/v1/messages/batches/{batch_id}/results",
            get(handlers::message_batch_results),
        )
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/images/generations", post(handlers::image_generations))
        .route("/v1/tts", get(handlers::tts_websocket).post(handlers::tts))
        .route("/v1/tts/voices", get(handlers::tts_voices))
        .layer(middleware::from_fn(log_request_summary))
        .with_state(state)
}

async fn log_request_summary(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    tracing::debug!(
        %method,
        path = %path,
        status = response.status().as_u16(),
        "http_request"
    );
    response
}

#[cfg(test)]
mod tests;
