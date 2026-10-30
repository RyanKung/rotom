//! HTTP boundary for configured services that accept the incoming API protocol.

use super::{AppState, auth::authorize};
use crate::{
    Error, Result,
    forward::{ForwardConfig, resolve_model, upstream_url},
};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::Value;

const MAX_FORWARD_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Routes declared forward models before typed handlers can alter their payloads.
pub(super) async fn middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if state.forwards.is_empty() || !is_forward_candidate(&request) {
        return next.run(request).await;
    }
    if let Err(error) = authorize(request.headers(), state.api_key.as_deref()) {
        return error.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_FORWARD_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Error::upstream_with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "forward request body exceeds the limit or could not be read",
            )
            .into_response();
        }
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(&bytes) else {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    let Some(requested_model) = payload
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.to_owned())
    else {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    let model = state
        .model_aliases
        .get(&requested_model)
        .map_or(requested_model.as_str(), String::as_str);
    let Some((forward, upstream_model)) = resolve_model(&state.forwards, model) else {
        if model
            .split_once('/')
            .is_some_and(|(service, _)| state.forwards.contains_key(service))
        {
            return Error::config(format!("forward model {model} is not configured"))
                .into_response();
        }
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    let upstream_model = upstream_model.to_owned();
    if let Some(model) = payload.get_mut("model") {
        *model = Value::String(upstream_model);
    }
    let path = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path(), axum::http::uri::PathAndQuery::as_str);
    match send_forward(&state.forward_http, forward, &parts.headers, path, &payload).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

fn is_forward_candidate(request: &Request) -> bool {
    request.method() == Method::POST
        && request.uri().path().starts_with("/v1/")
        && request.uri().path() != "/v1/auth/refresh"
}

async fn send_forward(
    http: &reqwest::Client,
    config: &ForwardConfig,
    headers: &HeaderMap,
    path: &str,
    payload: &Value,
) -> Result<Response> {
    let url = upstream_url(&config.base_url, path)?;
    let mut upstream_headers = proxy_headers(headers, true);
    if let Some(api_key) = &config.api_key {
        let mut value = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| Error::config("invalid forward API key"))?;
        value.set_sensitive(true);
        upstream_headers.insert(header::AUTHORIZATION, value);
    }
    for (name, value) in &config.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::config("invalid forward header name"))?;
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| Error::config("invalid forward header value"))?;
        value.set_sensitive(true);
        upstream_headers.insert(name, value);
    }
    // Only `model` changes. Every other JSON value reaches the upstream intact.
    let upstream = http
        .post(url)
        .headers(upstream_headers)
        .body(serde_json::to_vec(payload)?)
        .send()
        .await?;
    let status = upstream.status();
    let headers = proxy_headers(upstream.headers(), false);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

fn proxy_headers(source: &HeaderMap, request: bool) -> HeaderMap {
    let connection_headers = source
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| HeaderName::from_bytes(value.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    let mut headers = HeaderMap::new();
    for (name, value) in source {
        if is_hop_by_hop_header(name)
            || connection_headers.contains(name)
            || (request && is_local_request_header(name))
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

fn is_local_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "x-api-key" | "cookie" | "host" | "content-length"
    )
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
