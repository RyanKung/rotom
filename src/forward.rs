//! Configuration and routing rules for transparent third-party API forwarding.

use crate::{Error, Result};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

/// One third-party API endpoint whose request and response formats remain unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForwardConfig {
    /// HTTP(S) service root, optionally including a path prefix such as `/anthropic`.
    pub base_url: String,
    /// Optional upstream bearer token, independent of Rotom's local API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Static upstream headers, including alternate authentication such as `x-api-key`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Upstream model IDs exposed locally as `<service>/<model>`.
    pub models: Vec<String>,
}

/// Invalid forwarding configuration, without including credential values in diagnostics.
#[derive(Debug, thiserror::Error)]
enum ForwardConfigError {
    #[error(
        "forward service {service:?} requires a non-reserved ASCII name using letters, digits, '-' or '_'"
    )]
    InvalidServiceName { service: String },
    #[error(
        "forward service {service:?} requires an HTTP(S) base_url without credentials, query or fragment"
    )]
    InvalidBaseUrl { service: String },
    #[error(
        "forward service {service:?} requires at least one non-empty model ID without whitespace or duplicate entries"
    )]
    InvalidModels { service: String },
    #[error("forward service {service:?} has an invalid header name or value")]
    InvalidHeader { service: String },
    #[error("forward service {service:?} cannot configure transport header {header:?}")]
    TransportHeader { service: String, header: String },
    #[error("forward service {service:?} has an empty or invalid bearer api_key")]
    InvalidApiKey { service: String },
    #[error("forward request path must start with '/' and must not contain a fragment")]
    InvalidRequestPath,
}

impl From<ForwardConfigError> for Error {
    fn from(error: ForwardConfigError) -> Self {
        Self::config(error.to_string())
    }
}

/// Validates service identities, endpoint URLs, model lists and outbound headers.
///
/// # Errors
///
/// Returns a configuration error for malformed or conflicting forwarding settings.
pub fn validate_forwards(forwards: &BTreeMap<String, ForwardConfig>) -> Result<()> {
    for (name, config) in forwards {
        validate_forward(name, config)?;
    }
    Ok(())
}

fn validate_forward(
    name: &str,
    config: &ForwardConfig,
) -> std::result::Result<(), ForwardConfigError> {
    if !valid_service_name(name) {
        return Err(ForwardConfigError::InvalidServiceName {
            service: name.to_owned(),
        });
    }
    validated_base_url(&config.base_url, name)?;
    let mut seen = BTreeSet::new();
    if config.models.is_empty()
        || config
            .models
            .iter()
            .any(|model| !valid_model_id(model) || !seen.insert(model))
    {
        return Err(ForwardConfigError::InvalidModels {
            service: name.to_owned(),
        });
    }
    if let Some(api_key) = &config.api_key {
        if api_key.trim().is_empty() || HeaderValue::from_str(&format!("Bearer {api_key}")).is_err()
        {
            return Err(ForwardConfigError::InvalidApiKey {
                service: name.to_owned(),
            });
        }
    }
    for (header, value) in &config.headers {
        let parsed = HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
            ForwardConfigError::InvalidHeader {
                service: name.to_owned(),
            }
        })?;
        HeaderValue::from_str(value).map_err(|_| ForwardConfigError::InvalidHeader {
            service: name.to_owned(),
        })?;
        if is_transport_header(parsed.as_str()) {
            return Err(ForwardConfigError::TransportHeader {
                service: name.to_owned(),
                header: parsed.to_string(),
            });
        }
    }
    Ok(())
}

fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "codex" | "openai" | "openai-codex" | "grok" | "xai" | "kiro" | "vercel" | "anthropic"
        )
}

fn valid_model_id(model: &str) -> bool {
    !model.is_empty()
        && !model.starts_with('/')
        && !model
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
}

fn validated_base_url(
    base_url: &str,
    service: &str,
) -> std::result::Result<Url, ForwardConfigError> {
    let invalid = || ForwardConfigError::InvalidBaseUrl {
        service: service.to_owned(),
    };
    let url = Url::parse(base_url).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || base_url.trim() != base_url
    {
        return Err(invalid());
    }
    Ok(url)
}

/// Identifies HTTP transport headers that cannot safely be copied through the proxy.
#[must_use]
pub fn is_transport_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Returns the namespaced model IDs advertised for a configured service.
#[must_use]
pub fn model_ids(name: &str, config: &ForwardConfig) -> Vec<String> {
    config
        .models
        .iter()
        .map(|model| format!("{name}/{model}"))
        .collect()
}

/// Resolves an advertised model ID to its service and unchanged upstream model ID.
///
/// Only models explicitly declared by the service are eligible for forwarding.
#[must_use]
pub fn resolve_model<'a>(
    forwards: &'a BTreeMap<String, ForwardConfig>,
    model: &'a str,
) -> Option<(&'a ForwardConfig, &'a str)> {
    let (name, upstream_model) = model.split_once('/')?;
    let config = forwards.get(name)?;
    config
        .models
        .iter()
        .any(|candidate| candidate == upstream_model)
        .then_some((config, upstream_model))
}

/// Collects warnings for `model_aliases` that do not resolve to a forward model.
///
/// These are treated as warnings, not hard errors, so a misconfigured forward
/// does not prevent the daemon from starting.
#[must_use]
pub fn collect_forward_alias_warnings(
    forwards: &BTreeMap<String, ForwardConfig>,
    aliases: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (alias, target) in aliases {
        if target.contains('/') && resolve_model(forwards, target).is_none() {
            warnings.push(format!(
                "model_alias {alias:?} points to unknown forward model {target:?}"
            ));
        }
    }
    warnings
}

/// Appends the request path and query to a service root, preserving its path prefix.
///
/// A base URL ending in `/v1` consumes the incoming `/v1` prefix once.
///
/// # Errors
///
/// Returns an error if the service root or request path is invalid.
pub fn upstream_url(base_url: &str, path_and_query: &str) -> Result<Url> {
    let mut url = validated_base_url(base_url, "upstream")?;
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, None), |(path, query)| (path, Some(query)));
    if !path.starts_with('/') || path_and_query.contains('#') {
        return Err(ForwardConfigError::InvalidRequestPath.into());
    }
    let base_path = url.path().trim_end_matches('/');
    let suffix = if base_path.ends_with("/v1") && (path == "/v1" || path.starts_with("/v1/")) {
        path.strip_prefix("/v1").unwrap_or(path)
    } else {
        path
    };
    let joined_path = format!("{base_path}{suffix}");
    url.set_path(&joined_path);
    url.set_query(query);
    Ok(url)
}

#[cfg(test)]
mod tests;
