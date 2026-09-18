use crate::{
    Error, Result,
    codex::{
        convert::to_codex_request,
        events::{
            collect_output, collect_response_value, event_error, event_tool_call, finish_reason,
            is_done_event, normalize_incomplete_result_finish_reason,
            normalize_incomplete_result_response, response_has_usable_output, response_tool_calls,
            text_delta,
        },
        kiro, sse,
        upstream::{UpstreamProvider, adapter_for_provider},
    },
    config::{Credentials, Provider, now_unix},
    openai::response::{
        AssistantMessage, ChatChoice, ChatCompletionChunk, ChatCompletionResponse, chunk_finished,
        chunk_with_content, chunk_with_role, chunk_with_tool_call,
    },
};
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, Method, Response, Upgraded, Version,
    header::{
        ACCEPT, CONNECTION, CONTENT_TYPE, HeaderMap, HeaderValue, SEC_WEBSOCKET_ACCEPT,
        SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION,
        UPGRADE,
    },
};
use serde_json::Value;
use std::pin::Pin;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::Role,
    },
};
use url::Url;

/// Default upstream base URL for `Codex` response requests.
pub use crate::codex::upstream::DEFAULT_CODEX_BASE_URL;
pub use crate::codex::upstream::{
    DEFAULT_VERCEL_AI_GATEWAY_BASE_URL, ResponseCreationStrategy, ResponseResourceCapabilities,
    ResponseResourceCapability, codex_headers, grok_headers, grok_tts_headers,
    grok_tts_voices_headers, grok_tts_websocket_headers, resolve_codex_url,
    resolve_grok_responses_url, resolve_grok_tts_url, resolve_grok_tts_voices_url,
    resolve_grok_tts_websocket_url, resolve_vercel_responses_url, vercel_headers,
};

/// Established xAI TTS WebSocket carried by the proxy-aware HTTP client.
pub(crate) type GrokTtsWebSocket = WebSocketStream<Upgraded>;

/// Result of attempting the HTTP upgrade for an xAI TTS WebSocket.
pub(crate) enum GrokTtsWebSocketConnect {
    /// The upstream accepted the upgrade and the stream is ready for frames.
    Connected(GrokTtsWebSocket),
    /// The upstream returned a normal HTTP error response before upgrading.
    Rejected(Response),
}

/// Failure while constructing or validating an xAI TTS WebSocket upgrade.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GrokTtsWebSocketConnectError {
    /// The configured WebSocket URL is invalid.
    #[error("invalid Grok TTS WebSocket URL: {0}")]
    InvalidUrl(#[source] url::ParseError),
    /// The configured URL scheme cannot be converted to HTTP for the upgrade request.
    #[error("unsupported Grok TTS WebSocket URL scheme: {0}")]
    UnsupportedScheme(String),
    /// Authentication headers could not be constructed.
    #[error("invalid Grok TTS WebSocket authentication: {0}")]
    Authentication(#[source] Error),
    /// The proxy-aware HTTP request failed before a response arrived.
    #[error("Grok TTS WebSocket upgrade request failed: {0}")]
    Request(#[source] reqwest::Error),
    /// The upstream response did not confirm a WebSocket upgrade.
    #[error("Grok TTS WebSocket response is missing `Upgrade: websocket`")]
    MissingUpgrade,
    /// The upstream response did not include the required connection token.
    #[error("Grok TTS WebSocket response is missing `Connection: upgrade`")]
    MissingConnectionUpgrade,
    /// The upstream response did not authenticate the WebSocket key.
    #[error("Grok TTS WebSocket response has an invalid `Sec-WebSocket-Accept`")]
    InvalidAcceptKey,
    /// The upstream selected an extension that rotom did not offer.
    #[error("Grok TTS WebSocket response selected an unexpected extension")]
    UnexpectedExtension,
    /// The upstream selected a subprotocol that rotom did not offer.
    #[error("Grok TTS WebSocket response selected an unexpected subprotocol")]
    UnexpectedSubprotocol,
    /// Hyper could not return the upgraded byte stream.
    #[error("Grok TTS WebSocket stream upgrade failed: {0}")]
    Upgrade(#[source] reqwest::Error),
}

#[derive(Clone)]
/// HTTP client wrapper for the `ChatGPT` `Codex` responses backend.
pub struct CodexClient {
    http: Client,
    base_url: String,
    provider: Provider,
}

impl CodexClient {
    /// Creates a Codex client with the provided HTTP client and backend base URL.
    #[must_use]
    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            provider: Provider::Codex,
        }
    }

    /// Creates an upstream client for the selected provider.
    #[must_use]
    pub fn new_for_provider(http: Client, provider: Provider) -> Self {
        let base_url = adapter_for_provider(provider).default_base_url();
        Self::new_for_provider_base_url(http, provider, base_url)
    }

    /// Creates an upstream client for the selected provider and base URL.
    #[must_use]
    pub fn new_for_provider_base_url(
        http: Client,
        provider: Provider,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            provider,
        }
    }

    /// Returns the default upstream base URL used by the client.
    #[must_use]
    pub const fn default_base_url() -> &'static str {
        DEFAULT_CODEX_BASE_URL
    }

    /// Returns the configured upstream base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the upstream provider used by this client.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// Returns the provider's Responses resource lifecycle support matrix.
    #[must_use]
    pub fn response_resource_capabilities(&self) -> ResponseResourceCapabilities {
        self.upstream_provider().resource_capabilities()
    }

    /// Returns how this provider should create OpenAI-compatible response objects.
    #[must_use]
    pub fn response_creation_strategy(&self) -> ResponseCreationStrategy {
        self.upstream_provider().response_creation_strategy()
    }

    /// Sends a non-streaming chat completion request and collects the full response body.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request fails or the Codex response
    /// stream cannot be collected into a final completion payload.
    pub async fn complete_chat(
        &self,
        request: crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<ChatCompletionResponse> {
        if self.provider == Provider::Kiro {
            return self.complete_kiro_chat(request, credentials).await;
        }
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        let response = self.send_chat(&request, credentials).await?;
        let output = collect_output(response).await?;

        let usage = output.usage;
        let message = AssistantMessage {
            role: "assistant",
            content: output.text,
            tool_calls: (!output.tool_calls.is_empty()).then_some(output.tool_calls),
            images: (!output.images.is_empty()).then_some(output.images),
        };

        Ok(ChatCompletionResponse {
            id,
            object: "chat.completion",
            created,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message,
                finish_reason: output.finish_reason,
            }],
            usage,
        })
    }

    /// Sends a streaming chat completion request and yields OpenAI-compatible chunks.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request fails before the response
    /// stream can be handed back to the caller.
    pub async fn stream_chat(
        &self,
        request: crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk>> + Send>>> {
        if self.provider == Provider::Kiro {
            return self.stream_kiro_chat(request, credentials).await;
        }
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        let response = self.send_chat(&request, credentials).await?;
        let mut events = Box::pin(sse::json_events(Box::pin(response.bytes_stream())));

        let stream = async_stream::try_stream! {
            yield chunk_with_role(&id, created, &model);
            let mut finished = false;
            let mut tool_call_count = 0_u32;
            let mut seen_tool_call_ids = std::collections::HashSet::<String>::new();
            let mut saw_usable_output = false;

            while let Some(event) = events.next().await {
                let event = event?;
                if let Some(message) = event_error(&event) {
                    Err(Error::upstream(message))?;
                }
                if let Some(delta) = text_delta(&event) {
                    if !delta.is_empty() {
                        saw_usable_output = true;
                        yield chunk_with_content(&id, created, &model, delta);
                    }
                }
                if let Some(tool_call) = event_tool_call(&event) {
                    if seen_tool_call_ids.insert(tool_call.id.clone()) {
                        // The SSE stream can repeat tool calls across incremental and completed events.
                        yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                        tool_call_count += 1;
                        saw_usable_output = true;
                    }
                }
                if is_done_event(&event) {
                    // Some tool calls appear only on the terminal completed event.
                    for tool_call in response_tool_calls(&event) {
                        if seen_tool_call_ids.insert(tool_call.id.clone()) {
                            yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                            tool_call_count += 1;
                            saw_usable_output = true;
                        }
                    }
                    if event
                        .get("response")
                        .is_some_and(response_has_usable_output)
                    {
                        saw_usable_output = true;
                    }
                    finished = true;
                    let reason = if tool_call_count > 0 {
                        "tool_calls".to_owned()
                    } else {
                        normalize_incomplete_result_finish_reason(
                            finish_reason(&event),
                            saw_usable_output,
                        )
                    };
                    yield chunk_finished(&id, created, &model, &reason);
                    break;
                }
            }

            if !finished {
                yield chunk_finished(&id, created, &model, "stop");
            }
        };

        Ok(Box::pin(stream))
    }

    /// Sends a non-streaming Responses-style request body and returns the final response envelope.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request fails or the response stream
    /// cannot be collected into one final response value.
    pub async fn complete_response(
        &self,
        request: Value,
        credentials: &Credentials,
    ) -> Result<Value> {
        if self.provider == Provider::Kiro {
            let response = self.send_kiro_body(&request, credentials).await?;
            return kiro::collect_response_value(response, request).await;
        }
        let response = self.send_body(&request, credentials).await?;
        if response_is_json(&response) {
            let mut value = response.json::<Value>().await?;
            normalize_incomplete_result_response(&mut value);
            Ok(value)
        } else {
            collect_response_value(response).await
        }
    }

    /// Sends a streaming Responses-style request body and yields raw upstream JSON SSE events.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request cannot be started.
    pub async fn stream_response(
        &self,
        request: Value,
        credentials: &Credentials,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>> {
        if self.provider == Provider::Kiro {
            let response = self.send_kiro_body(&request, credentials).await?;
            return Ok(kiro::response_event_stream(response, request));
        }
        let response = self.send_body(&request, credentials).await?;
        Ok(Box::pin(sse::json_named_events(Box::pin(
            response.bytes_stream(),
        ))))
    }

    /// Retrieves an upstream-stored Responses API object when the provider supports it.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider does not expose retrieval or the
    /// upstream request fails.
    pub async fn retrieve_response(
        &self,
        response_id: &str,
        credentials: &Credentials,
    ) -> Result<Value> {
        let response = self
            .send_response_resource(Method::GET, response_id, credentials)
            .await?;
        response.json::<Value>().await.map_err(Into::into)
    }

    /// Deletes an upstream-stored Responses API object when the provider supports it.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider does not expose deletion or the
    /// upstream request fails.
    pub async fn delete_response(
        &self,
        response_id: &str,
        credentials: &Credentials,
    ) -> Result<Value> {
        let response = self
            .send_response_resource(Method::DELETE, response_id, credentials)
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if text.trim().is_empty() {
            Ok(serde_json::json!({
                "id": response_id,
                "object": "response",
                "deleted": status.is_success()
            }))
        } else {
            serde_json::from_str(&text).map_err(Into::into)
        }
    }

    /// Sends a native xAI text-to-speech request and returns its raw response.
    ///
    /// The response may contain audio bytes or JSON when the request enables
    /// timestamps, so callers must preserve the upstream content type.
    ///
    /// # Errors
    ///
    /// Returns an error when this client is not configured for Grok or the
    /// HTTP request fails. xAI status codes and bodies are returned unchanged.
    pub async fn synthesize_grok_speech(
        &self,
        body: &Value,
        credentials: &Credentials,
    ) -> Result<Response> {
        if self.provider != Provider::Grok {
            return Err(Error::config("text-to-speech requires a Grok upstream"));
        }

        crate::logging::trace_json("upstream.grok.tts.request", body);
        let url = resolve_grok_tts_url(&self.base_url);
        let response = self
            .http
            .post(&url)
            .headers(grok_tts_headers(credentials)?)
            .json(body)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.grok.tts.response_started",
            url = %url,
            status = response.status().as_u16()
        );
        Ok(response)
    }

    /// Opens the native xAI TTS WebSocket through the same proxy-aware HTTP client as REST.
    ///
    /// # Errors
    ///
    /// Returns an error when this client is not configured for Grok, the request cannot be sent,
    /// or the upstream returns an invalid WebSocket handshake.
    pub(crate) async fn connect_grok_tts_websocket(
        &self,
        websocket_url: &str,
        credentials: &Credentials,
    ) -> std::result::Result<GrokTtsWebSocketConnect, GrokTtsWebSocketConnectError> {
        if self.provider != Provider::Grok {
            return Err(GrokTtsWebSocketConnectError::Authentication(Error::config(
                "text-to-speech requires a Grok upstream",
            )));
        }

        let http_url = websocket_http_upgrade_url(websocket_url)?;
        let websocket_key = generate_key();
        let response = self
            .http
            .get(http_url)
            .version(Version::HTTP_11)
            .headers(
                grok_tts_websocket_headers(credentials)
                    .map_err(GrokTtsWebSocketConnectError::Authentication)?,
            )
            .header(CONNECTION, "Upgrade")
            .header(UPGRADE, "websocket")
            .header(SEC_WEBSOCKET_VERSION, "13")
            .header(SEC_WEBSOCKET_KEY, &websocket_key)
            .send()
            .await
            .map_err(GrokTtsWebSocketConnectError::Request)?;

        if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
            return Ok(GrokTtsWebSocketConnect::Rejected(response));
        }
        validate_websocket_upgrade(response.headers(), &websocket_key)?;
        let upgraded = response
            .upgrade()
            .await
            .map_err(GrokTtsWebSocketConnectError::Upgrade)?;
        let socket = WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await;
        Ok(GrokTtsWebSocketConnect::Connected(socket))
    }

    /// Lists the native built-in xAI text-to-speech voices.
    ///
    /// # Errors
    ///
    /// Returns an error when this client is not configured for Grok or the
    /// HTTP request fails. xAI status codes and bodies are returned unchanged.
    pub async fn list_grok_tts_voices(&self, credentials: &Credentials) -> Result<Response> {
        if self.provider != Provider::Grok {
            return Err(Error::config("text-to-speech requires a Grok upstream"));
        }

        let url = resolve_grok_tts_voices_url(&self.base_url);
        let response = self
            .http
            .get(&url)
            .headers(grok_tts_voices_headers(credentials)?)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.grok.tts_voices.response_started",
            url = %url,
            status = response.status().as_u16()
        );
        Ok(response)
    }

    async fn send_chat(
        &self,
        request: &crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<Response> {
        self.send_body(&to_codex_request(request)?, credentials)
            .await
    }

    async fn complete_kiro_chat(
        &self,
        request: crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<ChatCompletionResponse> {
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        let body = to_codex_request(&request)?;
        let response = self.send_kiro_body(&body, credentials).await?;
        let output = kiro::collect_chat_output(response, body).await?;

        let usage = output.usage;
        let message = AssistantMessage {
            role: "assistant",
            content: output.text,
            tool_calls: (!output.tool_calls.is_empty()).then_some(output.tool_calls),
            images: (!output.images.is_empty()).then_some(output.images),
        };

        Ok(ChatCompletionResponse {
            id,
            object: "chat.completion",
            created,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message,
                finish_reason: output.finish_reason,
            }],
            usage,
        })
    }

    async fn stream_kiro_chat(
        &self,
        request: crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk>> + Send>>> {
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        let body = to_codex_request(&request)?;
        let response = self.send_kiro_body(&body, credentials).await?;
        let mut events = kiro::response_event_stream(response, body);

        let stream = async_stream::try_stream! {
            yield chunk_with_role(&id, created, &model);
            let mut finished = false;
            let mut tool_call_count = 0_u32;
            let mut seen_tool_call_ids = std::collections::HashSet::<String>::new();
            let mut saw_usable_output = false;

            while let Some(event) = events.next().await {
                let event = event?.value;
                if let Some(message) = event_error(&event) {
                    Err(Error::upstream(message))?;
                }
                if let Some(delta) = text_delta(&event) {
                    if !delta.is_empty() {
                        saw_usable_output = true;
                        yield chunk_with_content(&id, created, &model, delta);
                    }
                }
                if let Some(tool_call) = event_tool_call(&event) {
                    if seen_tool_call_ids.insert(tool_call.id.clone()) {
                        yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                        tool_call_count += 1;
                        saw_usable_output = true;
                    }
                }
                if is_done_event(&event) {
                    for tool_call in response_tool_calls(&event) {
                        if seen_tool_call_ids.insert(tool_call.id.clone()) {
                            yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                            tool_call_count += 1;
                            saw_usable_output = true;
                        }
                    }
                    if event
                        .get("response")
                        .is_some_and(response_has_usable_output)
                    {
                        saw_usable_output = true;
                    }
                    finished = true;
                    let reason = if tool_call_count > 0 {
                        "tool_calls".to_owned()
                    } else {
                        normalize_incomplete_result_finish_reason(
                            finish_reason(&event),
                            saw_usable_output,
                        )
                    };
                    yield chunk_finished(&id, created, &model, &reason);
                    break;
                }
            }

            if !finished {
                yield chunk_finished(&id, created, &model, "stop");
            }
        };

        Ok(Box::pin(stream))
    }

    async fn send_body(&self, body: &Value, credentials: &Credentials) -> Result<Response> {
        let mut upstream_body = body.clone();
        let upstream = self.upstream_provider();
        upstream.prepare_request(&mut upstream_body);
        crate::logging::trace_json("upstream.request", &upstream_body);
        let url = upstream.responses_url(&self.base_url);
        let response = self
            .http
            .post(&url)
            .headers(upstream.headers(credentials)?)
            .json(&upstream_body)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.response_started",
            url = %url,
            status = response.status().as_u16()
        );
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(parse_error_response(response, upstream.provider()).await)
        }
    }

    async fn send_kiro_body(&self, body: &Value, credentials: &Credentials) -> Result<Response> {
        let upstream_body = kiro::to_kiro_payload(body, credentials)?;
        crate::logging::trace_json("upstream.kiro.request", &upstream_body);
        let url = kiro::kiro_endpoint_url(&self.base_url, credentials)?;
        let response = self
            .http
            .post(&url)
            .headers(kiro::kiro_headers(credentials)?)
            .json(&upstream_body)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.kiro.response_started",
            url = %url,
            status = response.status().as_u16()
        );
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(parse_error_response(response, Provider::Kiro).await)
        }
    }

    async fn send_response_resource(
        &self,
        method: Method,
        response_id: &str,
        credentials: &Credentials,
    ) -> Result<Response> {
        let upstream = self.upstream_provider();
        let Some(url) = upstream.response_resource_url(&self.base_url, response_id) else {
            return Err(unsupported_response_resource(
                upstream.provider(),
                method.as_str(),
            ));
        };
        let mut headers = upstream.headers(credentials)?;
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = self
            .http
            .request(method, &url)
            .headers(headers)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.response_resource_started",
            url = %url,
            status = response.status().as_u16()
        );
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(parse_error_response(response, upstream.provider()).await)
        }
    }

    fn upstream_provider(&self) -> &'static dyn UpstreamProvider {
        adapter_for_provider(self.provider)
    }
}

fn websocket_http_upgrade_url(
    websocket_url: &str,
) -> std::result::Result<Url, GrokTtsWebSocketConnectError> {
    let mut url = Url::parse(websocket_url).map_err(GrokTtsWebSocketConnectError::InvalidUrl)?;
    let http_scheme = match url.scheme() {
        "wss" | "https" => "https",
        "ws" | "http" => "http",
        scheme => {
            return Err(GrokTtsWebSocketConnectError::UnsupportedScheme(
                scheme.to_owned(),
            ));
        }
    };
    url.set_scheme(http_scheme)
        .map_err(|()| GrokTtsWebSocketConnectError::UnsupportedScheme(url.scheme().to_owned()))?;
    Ok(url)
}

fn validate_websocket_upgrade(
    headers: &HeaderMap,
    websocket_key: &str,
) -> std::result::Result<(), GrokTtsWebSocketConnectError> {
    if !header_contains_token(headers, UPGRADE, "websocket") {
        return Err(GrokTtsWebSocketConnectError::MissingUpgrade);
    }
    if !header_contains_token(headers, CONNECTION, "upgrade") {
        return Err(GrokTtsWebSocketConnectError::MissingConnectionUpgrade);
    }
    let expected_accept = derive_accept_key(websocket_key.as_bytes());
    if headers
        .get(SEC_WEBSOCKET_ACCEPT)
        .and_then(|value| value.to_str().ok())
        != Some(expected_accept.as_str())
    {
        return Err(GrokTtsWebSocketConnectError::InvalidAcceptKey);
    }
    if headers.contains_key(SEC_WEBSOCKET_EXTENSIONS) {
        return Err(GrokTtsWebSocketConnectError::UnexpectedExtension);
    }
    if headers.contains_key(SEC_WEBSOCKET_PROTOCOL) {
        return Err(GrokTtsWebSocketConnectError::UnexpectedSubprotocol);
    }
    Ok(())
}

fn header_contains_token(
    headers: &HeaderMap,
    name: reqwest::header::HeaderName,
    token: &str,
) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}

fn unsupported_response_resource(provider: Provider, operation: &str) -> Error {
    Error::upstream_with_status(
        reqwest::StatusCode::NOT_IMPLEMENTED,
        format!(
            "{} upstream does not support Responses resource {operation}",
            provider.display_name()
        ),
    )
}

fn response_is_json(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

async fn parse_error_response(response: Response, provider: Provider) -> Error {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    crate::logging::trace_text("upstream.codex.error_body", &text);
    // Prefer the structured upstream error message when the backend provides one.
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/detail"))
                .or_else(|| value.pointer("/message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(text);

    let downstream_status = if status.is_client_error() {
        status
    } else {
        reqwest::StatusCode::BAD_GATEWAY
    };

    Error::upstream_with_status(
        downstream_status,
        format!(
            "{} backend returned {status}: {message}",
            provider.display_name()
        ),
    )
}

#[must_use]
fn chat_completion_id() -> String {
    format!("chatcmpl-{}-{:08x}", now_unix(), rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_upstream_provider_adapter() {
        let http = Client::new();
        let codex = CodexClient::new_for_provider(http.clone(), Provider::Codex);
        let grok = CodexClient::new_for_provider(http.clone(), Provider::Grok);
        let vercel = CodexClient::new_for_provider(http, Provider::Vercel);

        assert_eq!(codex.upstream_provider().provider(), Provider::Codex);
        assert_eq!(grok.upstream_provider().provider(), Provider::Grok);
        assert_eq!(vercel.upstream_provider().provider(), Provider::Vercel);
        assert_eq!(
            codex.response_creation_strategy(),
            ResponseCreationStrategy::ChatCompatibility
        );
        assert_eq!(
            grok.response_creation_strategy(),
            ResponseCreationStrategy::NativeResponses
        );
        assert_eq!(
            vercel.response_creation_strategy(),
            ResponseCreationStrategy::NativeResponses
        );
    }

    #[test]
    fn websocket_upgrade_urls_preserve_authority_path_and_query() {
        let secure =
            websocket_http_upgrade_url("wss://api.x.ai/v1/tts?language=zh&voice=eve&future=1")
                .ok()
                .map(|url| url.to_string());
        let local = websocket_http_upgrade_url("ws://127.0.0.1:8080/tts?language=en")
            .ok()
            .map(|url| url.to_string());

        assert_eq!(
            secure.as_deref(),
            Some("https://api.x.ai/v1/tts?language=zh&voice=eve&future=1")
        );
        assert_eq!(
            local.as_deref(),
            Some("http://127.0.0.1:8080/tts?language=en")
        );
        assert!(matches!(
            websocket_http_upgrade_url("ftp://api.x.ai/v1/tts"),
            Err(GrokTtsWebSocketConnectError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn websocket_connection_header_accepts_comma_separated_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));

        assert!(header_contains_token(&headers, CONNECTION, "upgrade"));
        assert!(!header_contains_token(&headers, CONNECTION, "websocket"));
    }
}
