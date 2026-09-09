use crate::{
    Error, Result,
    codex::kiro::{DEFAULT_KIRO_BASE_URL, kiro_headers},
    config::{Credentials, Provider},
    oauth::XAI_API_BASE_URL,
};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde_json::Value;

const CODEX_PRIORITY_SERVICE_TIER: &str = "priority";
const CODEX_UPSTREAM: CodexUpstream = CodexUpstream;
const GROK_UPSTREAM: GrokUpstream = GrokUpstream;
const KIRO_UPSTREAM: KiroUpstream = KiroUpstream;

/// Default upstream base URL for `Codex` response requests.
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Upstream support level for one Responses API resource operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseResourceCapability {
    /// The upstream provider documents and serves this operation.
    UpstreamSupported,
    /// rotom implements this operation locally for client compatibility.
    LocalCompat,
    /// Neither the upstream nor rotom should claim support for this operation.
    Unsupported,
}

/// Provider support matrix for Responses API resource lifecycle operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseResourceCapabilities {
    /// `GET /v1/responses/{response_id}` support.
    pub retrieve: ResponseResourceCapability,
    /// `DELETE /v1/responses/{response_id}` support.
    pub delete: ResponseResourceCapability,
    /// `POST /v1/responses/{response_id}/cancel` support.
    pub cancel: ResponseResourceCapability,
    /// `GET /v1/responses/{response_id}/input_items` support.
    pub list_input_items: ResponseResourceCapability,
}

/// Provider-specific strategy for creating Responses API objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseCreationStrategy {
    /// Preserve the historical rotom compatibility path unless native replay is required.
    ChatCompatibility,
    /// Use the upstream provider's native `/responses` create endpoint.
    NativeResponses,
}

pub(super) trait UpstreamProvider: Sync {
    fn provider(&self) -> Provider;

    fn default_base_url(&self) -> &'static str;

    fn responses_url(&self, base_url: &str) -> String;

    fn response_resource_url(&self, base_url: &str, response_id: &str) -> Option<String>;

    fn resource_capabilities(&self) -> ResponseResourceCapabilities;

    fn response_creation_strategy(&self) -> ResponseCreationStrategy;

    fn headers(&self, credentials: &Credentials) -> Result<HeaderMap>;

    fn prepare_request(&self, body: &mut Value);
}

struct CodexUpstream;

impl UpstreamProvider for CodexUpstream {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn default_base_url(&self) -> &'static str {
        DEFAULT_CODEX_BASE_URL
    }

    fn responses_url(&self, base_url: &str) -> String {
        resolve_codex_url(base_url)
    }

    fn response_resource_url(&self, _base_url: &str, _response_id: &str) -> Option<String> {
        None
    }

    fn resource_capabilities(&self) -> ResponseResourceCapabilities {
        ResponseResourceCapabilities {
            retrieve: ResponseResourceCapability::LocalCompat,
            delete: ResponseResourceCapability::LocalCompat,
            cancel: ResponseResourceCapability::Unsupported,
            list_input_items: ResponseResourceCapability::LocalCompat,
        }
    }

    fn response_creation_strategy(&self) -> ResponseCreationStrategy {
        ResponseCreationStrategy::ChatCompatibility
    }

    fn headers(&self, credentials: &Credentials) -> Result<HeaderMap> {
        codex_headers(credentials)
    }

    fn prepare_request(&self, body: &mut Value) {
        remove_codex_unsupported_keys(body);
    }
}

struct GrokUpstream;

impl UpstreamProvider for GrokUpstream {
    fn provider(&self) -> Provider {
        Provider::Grok
    }

    fn default_base_url(&self) -> &'static str {
        XAI_API_BASE_URL
    }

    fn responses_url(&self, base_url: &str) -> String {
        resolve_grok_responses_url(base_url)
    }

    fn response_resource_url(&self, base_url: &str, response_id: &str) -> Option<String> {
        Some(resolve_response_resource_url(
            &resolve_grok_responses_url(base_url),
            response_id,
        ))
    }

    fn resource_capabilities(&self) -> ResponseResourceCapabilities {
        ResponseResourceCapabilities {
            retrieve: ResponseResourceCapability::UpstreamSupported,
            delete: ResponseResourceCapability::UpstreamSupported,
            cancel: ResponseResourceCapability::Unsupported,
            list_input_items: ResponseResourceCapability::LocalCompat,
        }
    }

    fn response_creation_strategy(&self) -> ResponseCreationStrategy {
        ResponseCreationStrategy::NativeResponses
    }

    fn headers(&self, credentials: &Credentials) -> Result<HeaderMap> {
        grok_headers(credentials)
    }

    fn prepare_request(&self, body: &mut Value) {
        remove_grok_unsupported_keys(body);
    }
}

struct KiroUpstream;

impl UpstreamProvider for KiroUpstream {
    fn provider(&self) -> Provider {
        Provider::Kiro
    }

    fn default_base_url(&self) -> &'static str {
        DEFAULT_KIRO_BASE_URL
    }

    fn responses_url(&self, base_url: &str) -> String {
        base_url.trim_end_matches('/').to_owned()
    }

    fn response_resource_url(&self, _base_url: &str, _response_id: &str) -> Option<String> {
        None
    }

    fn resource_capabilities(&self) -> ResponseResourceCapabilities {
        ResponseResourceCapabilities {
            retrieve: ResponseResourceCapability::LocalCompat,
            delete: ResponseResourceCapability::LocalCompat,
            cancel: ResponseResourceCapability::Unsupported,
            list_input_items: ResponseResourceCapability::LocalCompat,
        }
    }

    fn response_creation_strategy(&self) -> ResponseCreationStrategy {
        ResponseCreationStrategy::NativeResponses
    }

    fn headers(&self, credentials: &Credentials) -> Result<HeaderMap> {
        kiro_headers(credentials)
    }

    fn prepare_request(&self, _body: &mut Value) {}
}

pub(super) fn adapter_for_provider(provider: Provider) -> &'static dyn UpstreamProvider {
    match provider {
        Provider::Codex => &CODEX_UPSTREAM,
        Provider::Grok => &GROK_UPSTREAM,
        Provider::Kiro => &KIRO_UPSTREAM,
    }
}

/// Resolves a configured base URL into the concrete Codex responses endpoint.
#[must_use]
pub fn resolve_codex_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_owned()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Resolves a configured xAI base URL into the concrete Responses endpoint.
#[must_use]
pub fn resolve_grok_responses_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/responses") {
        normalized.to_owned()
    } else {
        format!("{normalized}/responses")
    }
}

/// Resolves a configured xAI base URL into the native TTS endpoint.
#[must_use]
pub fn resolve_grok_tts_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/tts") {
        normalized.to_owned()
    } else {
        format!("{normalized}/tts")
    }
}

/// Resolves a configured xAI base URL into the native streaming TTS WebSocket endpoint.
///
/// The provided raw query string is preserved so new xAI connection options can pass through
/// without requiring a gateway release.
///
/// # Errors
///
/// Returns an error when the configured base URL is invalid or does not use an HTTP or WebSocket
/// scheme.
pub fn resolve_grok_tts_websocket_url(base_url: &str, raw_query: Option<&str>) -> Result<String> {
    let mut url = url::Url::parse(&resolve_grok_tts_url(base_url))?;
    let websocket_scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        scheme => {
            return Err(Error::config(format!(
                "unsupported Grok TTS WebSocket URL scheme: {scheme}"
            )));
        }
    };
    url.set_scheme(websocket_scheme)
        .map_err(|()| Error::config("invalid Grok TTS WebSocket URL scheme"))?;
    url.set_query(raw_query.filter(|query| !query.is_empty()));
    Ok(url.into())
}

/// Resolves a configured xAI base URL into the built-in TTS voices endpoint.
#[must_use]
pub fn resolve_grok_tts_voices_url(base_url: &str) -> String {
    format!("{}/voices", resolve_grok_tts_url(base_url))
}

/// Appends an opaque response id to a provider Responses collection URL.
#[must_use]
pub fn resolve_response_resource_url(responses_url: &str, response_id: &str) -> String {
    format!("{}/{response_id}", responses_url.trim_end_matches('/'))
}

/// Builds the required HTTP headers for authenticated Codex backend requests.
///
/// # Errors
///
/// Returns an error when any derived header value is invalid.
pub fn codex_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(
        HeaderName::from_static("chatgpt-account-id"),
        header_value(&credentials.account_id)?,
    );
    headers.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static("pi"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("pi (rust; rotom)"));
    headers.insert(
        HeaderName::from_static("openai-beta"),
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Builds HTTP headers for authenticated xAI Grok API requests.
///
/// # Errors
///
/// Returns an error when any derived header value is invalid.
pub fn grok_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("rotom"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Builds HTTP headers for authenticated xAI TTS generation requests.
///
/// # Errors
///
/// Returns an error when the bearer token cannot be represented as a header.
pub fn grok_tts_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = grok_auth_headers(credentials)?;
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Builds HTTP headers for authenticated xAI TTS voice-list requests.
///
/// # Errors
///
/// Returns an error when the bearer token cannot be represented as a header.
pub fn grok_tts_voices_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = grok_auth_headers(credentials)?;
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Builds HTTP headers for an authenticated xAI TTS WebSocket upgrade.
///
/// # Errors
///
/// Returns an error when the bearer token cannot be represented as a header.
pub fn grok_tts_websocket_headers(credentials: &Credentials) -> Result<HeaderMap> {
    grok_auth_headers(credentials)
}

fn grok_auth_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("rotom"));
    Ok(headers)
}

fn remove_grok_unsupported_keys(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("service_tier");
        let has_tools = object
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            object.remove("tool_choice");
        }
    }
}

fn remove_codex_unsupported_keys(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("temperature");
        object.remove("top_p");
        object.remove("max_output_tokens");
        object.remove("stop");
        normalize_codex_service_tier(object);
    }
}

fn normalize_codex_service_tier(object: &mut serde_json::Map<String, Value>) {
    let Some(tier) = object.get("service_tier").and_then(Value::as_str) else {
        return;
    };
    if let Some(normalized) = normalize_codex_service_tier_value(tier) {
        object.insert(
            "service_tier".to_owned(),
            Value::String(normalized.to_owned()),
        );
    } else {
        object.remove("service_tier");
    }
}

fn normalize_codex_service_tier_value(tier: &str) -> Option<&'static str> {
    let tier = tier.trim();
    if tier.eq_ignore_ascii_case("priority") || tier.eq_ignore_ascii_case("fast") {
        Some(CODEX_PRIORITY_SERVICE_TIER)
    } else {
        None
    }
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_codex_url_variants() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://example.com/codex"),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://example.com/codex/responses"),
            "https://example.com/codex/responses"
        );
    }

    #[test]
    fn resolves_grok_tts_url_variants() {
        assert_eq!(
            resolve_grok_tts_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/tts"
        );
        assert_eq!(
            resolve_grok_tts_url("https://api.x.ai/v1/tts/"),
            "https://api.x.ai/v1/tts"
        );
        assert_eq!(
            resolve_grok_tts_voices_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/tts/voices"
        );
        assert_eq!(
            resolve_grok_tts_websocket_url(
                "https://api.x.ai/v1",
                Some("language=zh&voice=eve&future=1")
            )
            .unwrap(),
            "wss://api.x.ai/v1/tts?language=zh&voice=eve&future=1"
        );
        assert_eq!(
            resolve_grok_tts_websocket_url("http://127.0.0.1:8080/tts/", None).unwrap(),
            "ws://127.0.0.1:8080/tts"
        );
    }

    #[test]
    fn builds_grok_tts_headers_without_sse_accept() {
        let credentials = Credentials {
            provider: Provider::Grok,
            access_token: "token".into(),
            refresh_token: String::new(),
            expires_at: 1,
            account_id: String::new(),
        };

        let generation = grok_tts_headers(&credentials).unwrap();
        assert_eq!(generation["authorization"], "Bearer token");
        assert_eq!(generation["content-type"], "application/json");
        assert!(!generation.contains_key("accept"));

        let voices = grok_tts_voices_headers(&credentials).unwrap();
        assert_eq!(voices["authorization"], "Bearer token");
        assert_eq!(voices["accept"], "application/json");
        assert!(!voices.contains_key("content-type"));

        let websocket = grok_tts_websocket_headers(&credentials).unwrap();
        assert_eq!(websocket["authorization"], "Bearer token");
        assert!(!websocket.contains_key("content-type"));
        assert!(!websocket.contains_key("accept"));
    }

    #[test]
    fn builds_required_codex_headers() {
        let credentials = Credentials {
            provider: Provider::Codex,
            access_token: "token".into(),
            refresh_token: "refresh".into(),
            expires_at: 1,
            account_id: "acc".into(),
        };
        let headers = codex_headers(&credentials).unwrap();

        assert_eq!(headers["authorization"], "Bearer token");
        assert_eq!(headers["chatgpt-account-id"], "acc");
        assert_eq!(headers["openai-beta"], "responses=experimental");
    }

    #[test]
    fn codex_adapter_prepares_request_and_headers() {
        let adapter = adapter_for_provider(Provider::Codex);
        let mut body = json!({
            "model": "gpt-5.5",
            "input": "hello",
            "temperature": 0.2,
            "top_p": 0.8,
            "max_output_tokens": 128,
            "stop": ["DONE"],
            "service_tier": "fast"
        });
        let credentials = Credentials {
            provider: Provider::Codex,
            access_token: "token".into(),
            refresh_token: "refresh".into(),
            expires_at: 1,
            account_id: "acc".into(),
        };

        adapter.prepare_request(&mut body);
        let headers = adapter.headers(&credentials).unwrap();

        assert_eq!(adapter.provider(), Provider::Codex);
        assert_eq!(adapter.default_base_url(), DEFAULT_CODEX_BASE_URL);
        assert_eq!(
            adapter.responses_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            adapter.resource_capabilities(),
            ResponseResourceCapabilities {
                retrieve: ResponseResourceCapability::LocalCompat,
                delete: ResponseResourceCapability::LocalCompat,
                cancel: ResponseResourceCapability::Unsupported,
                list_input_items: ResponseResourceCapability::LocalCompat,
            }
        );
        assert_eq!(
            adapter.response_creation_strategy(),
            ResponseCreationStrategy::ChatCompatibility
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("stop").is_none());
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(headers["chatgpt-account-id"], "acc");
    }

    #[test]
    fn grok_adapter_preserves_supported_response_controls() {
        let adapter = adapter_for_provider(Provider::Grok);
        let mut body = json!({
            "model": "grok-4",
            "input": "hello",
            "temperature": 0.2,
            "top_p": 0.8,
            "stop": ["DONE"],
            "include": ["reasoning.encrypted_content"],
            "service_tier": "auto",
            "tool_choice": "auto",
            "tools": [{"type": "function", "name": "lookup"}]
        });
        let credentials = Credentials {
            provider: Provider::Grok,
            access_token: "token".into(),
            refresh_token: String::new(),
            expires_at: 1,
            account_id: String::new(),
        };

        adapter.prepare_request(&mut body);
        let headers = adapter.headers(&credentials).unwrap();

        assert_eq!(adapter.provider(), Provider::Grok);
        assert_eq!(adapter.default_base_url(), XAI_API_BASE_URL);
        assert_eq!(
            adapter.responses_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/responses"
        );
        assert_eq!(
            adapter.response_resource_url("https://api.x.ai/v1", "resp_1"),
            Some("https://api.x.ai/v1/responses/resp_1".to_owned())
        );
        assert_eq!(
            adapter.resource_capabilities(),
            ResponseResourceCapabilities {
                retrieve: ResponseResourceCapability::UpstreamSupported,
                delete: ResponseResourceCapability::UpstreamSupported,
                cancel: ResponseResourceCapability::Unsupported,
                list_input_items: ResponseResourceCapability::LocalCompat,
            }
        );
        assert_eq!(
            adapter.response_creation_strategy(),
            ResponseCreationStrategy::NativeResponses
        );
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["stop"], json!(["DONE"]));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tool_choice"], "auto");
        assert!(body.get("service_tier").is_none());
        assert_eq!(headers["authorization"], "Bearer token");
    }

    #[test]
    fn removes_grok_tool_choice_without_tools() {
        let mut body = json!({
            "model": "grok-4",
            "input": "hello",
            "include": ["reasoning.encrypted_content"],
            "service_tier": "auto",
            "tool_choice": "auto"
        });

        remove_grok_unsupported_keys(&mut body);

        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("service_tier").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn preserves_grok_tool_choice_with_tools() {
        let mut body = json!({
            "model": "grok-4",
            "input": "hello",
            "tool_choice": "auto",
            "tools": [{"type": "function", "name": "lookup"}]
        });

        remove_grok_unsupported_keys(&mut body);

        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn removes_codex_unsupported_response_controls() {
        let mut body = json!({
            "model": "gpt-5.5",
            "input": "hello",
            "temperature": 0.2,
            "top_p": 0.8,
            "max_output_tokens": 128,
            "stop": ["DONE"],
            "service_tier": "fast",
            "parallel_tool_calls": false
        });

        remove_codex_unsupported_keys(&mut body);

        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("stop").is_none());
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn removes_codex_unknown_service_tier() {
        let mut body = json!({
            "model": "gpt-5.5",
            "input": "hello",
            "service_tier": "standard_only"
        });

        remove_codex_unsupported_keys(&mut body);

        assert!(body.get("service_tier").is_none());
    }
}
