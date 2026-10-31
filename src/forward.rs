//! Configuration and routing for transparent third-party API forwarding.

use crate::{Error, Result};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

/// One third-party API whose request and response formats stay unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForwardConfig {
    /// HTTP(S) service root, optionally including a path prefix.
    pub base_url: String,
    /// Optional upstream bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Extra upstream headers such as `x-api-key`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Upstream model IDs exposed locally as `<service>/<model>`.
    pub models: Vec<String>,
}

/// Validates service names, URLs, models, and outbound headers.
///
/// # Errors
///
/// Returns a configuration error for malformed forwarding settings.
pub fn validate_forwards(forwards: &BTreeMap<String, ForwardConfig>) -> Result<()> {
    for (name, config) in forwards {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "codex"
                    | "openai"
                    | "openai-codex"
                    | "grok"
                    | "xai"
                    | "kiro"
                    | "vercel"
                    | "anthropic"
            )
        {
            return Err(Error::config(format!(
                "forward service {name:?} requires a non-reserved ASCII name using letters, digits, '-' or '_'"
            )));
        }
        parse_base_url(&config.base_url, name)?;
        let mut seen = BTreeSet::new();
        if config.models.is_empty()
            || config.models.iter().any(|model| {
                model.is_empty()
                    || model.starts_with('/')
                    || model
                        .chars()
                        .any(|ch| ch.is_whitespace() || ch.is_control())
                    || !seen.insert(model)
            })
        {
            return Err(Error::config(format!(
                "forward service {name:?} requires at least one non-empty model ID without whitespace or duplicate entries"
            )));
        }
        if let Some(api_key) = &config.api_key {
            if api_key.trim().is_empty()
                || HeaderValue::from_str(&format!("Bearer {api_key}")).is_err()
            {
                return Err(Error::config(format!(
                    "forward service {name:?} has an empty or invalid bearer api_key"
                )));
            }
        }
        for (header, value) in &config.headers {
            let parsed = HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
                Error::config(format!(
                    "forward service {name:?} has an invalid header name or value"
                ))
            })?;
            HeaderValue::from_str(value).map_err(|_| {
                Error::config(format!(
                    "forward service {name:?} has an invalid header name or value"
                ))
            })?;
            if is_hop_header(parsed.as_str()) {
                return Err(Error::config(format!(
                    "forward service {name:?} cannot configure transport header {}",
                    parsed.as_str()
                )));
            }
        }
    }
    Ok(())
}

fn parse_base_url(base_url: &str, service: &str) -> Result<Url> {
    let url = Url::parse(base_url).map_err(|_| invalid_base(service))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || base_url.trim() != base_url
    {
        return Err(invalid_base(service));
    }
    Ok(url)
}

fn invalid_base(service: &str) -> Error {
    Error::config(format!(
        "forward service {service:?} requires an HTTP(S) base_url without credentials, query or fragment"
    ))
}

fn is_hop_header(name: &str) -> bool {
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

/// Namespaced model IDs advertised for a service.
#[must_use]
pub fn model_ids(name: &str, config: &ForwardConfig) -> Vec<String> {
    config
        .models
        .iter()
        .map(|model| format!("{name}/{model}"))
        .collect()
}

/// Resolves `<service>/<model>` to the configured service and upstream model ID.
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

/// Warnings for aliases whose target names a configured forward but is not listed.
///
/// Namespaced builtin IDs such as `openai/gpt-6.1-sol` are not treated as forwards.
/// These never block daemon startup.
#[must_use]
pub fn collect_forward_alias_warnings(
    forwards: &BTreeMap<String, ForwardConfig>,
    aliases: &BTreeMap<String, String>,
) -> Vec<String> {
    aliases
        .iter()
        .filter(|(_, target)| {
            target
                .split_once('/')
                .is_some_and(|(service, _)| forwards.contains_key(service))
                && resolve_model(forwards, target).is_none()
        })
        .map(|(alias, target)| {
            format!("model_alias {alias:?} points to unknown forward model {target:?}")
        })
        .collect()
}

/// Joins `path_and_query` onto `base_url`, collapsing a duplicated `/v1` prefix.
///
/// # Errors
///
/// Returns an error if the service root or request path is invalid.
pub fn upstream_url(base_url: &str, path_and_query: &str) -> Result<Url> {
    let mut url = parse_base_url(base_url, "upstream")?;
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, None), |(path, query)| (path, Some(query)));
    if !path.starts_with('/') || path_and_query.contains('#') {
        return Err(Error::config(
            "forward request path must start with '/' and must not contain a fragment",
        ));
    }
    let base_path = url.path().trim_end_matches('/');
    let suffix = if base_path.ends_with("/v1") && (path == "/v1" || path.starts_with("/v1/")) {
        path.strip_prefix("/v1").unwrap_or(path)
    } else {
        path
    };
    url.set_path(&format!("{base_path}{suffix}"));
    url.set_query(query);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> ForwardConfig {
        ForwardConfig {
            base_url: "https://api.example.com/anthropic".into(),
            api_key: Some("k".into()),
            headers: BTreeMap::new(),
            models: vec!["chat".into()],
        }
    }

    #[test]
    fn resolve_join_and_alias_warnings() {
        let forwards = BTreeMap::from([("deepseek".into(), svc())]);
        let (cfg, model) = resolve_model(&forwards, "deepseek/chat").unwrap();
        assert_eq!(cfg.base_url, svc().base_url);
        assert_eq!(model, "chat");
        assert!(resolve_model(&forwards, "deepseek/missing").is_none());
        assert_eq!(
            upstream_url("https://api.example.com/anthropic/v1", "/v1/messages?q=1")
                .unwrap()
                .as_str(),
            "https://api.example.com/anthropic/v1/messages?q=1"
        );
        let aliases = BTreeMap::from([
            ("ok".into(), "deepseek/chat".into()),
            ("bad".into(), "deepseek/nope".into()),
            ("plain".into(), "gpt-6".into()),
            ("vercel".into(), "openai/gpt-6.1-sol".into()),
        ]);
        let warnings = collect_forward_alias_warnings(&forwards, &aliases);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("deepseek/nope"));
        assert!(validate_forwards(&forwards).is_ok());
        let mut bad = svc();
        bad.base_url = "file:///tmp".into();
        assert!(validate_forwards(&BTreeMap::from([("deepseek".into(), bad)])).is_err());
    }
}
