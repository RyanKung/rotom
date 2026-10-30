use super::*;
use crate::{config::AppConfigStore, testsupport::TempDir};
use serde_json::json;
use std::fs;

fn service() -> ForwardConfig {
    ForwardConfig {
        base_url: "https://api.example.com/anthropic".into(),
        api_key: Some("upstream-secret".into()),
        headers: BTreeMap::new(),
        models: vec!["deepseek-chat".into(), "vendor/other-model".into()],
    }
}

fn services(config: ForwardConfig) -> BTreeMap<String, ForwardConfig> {
    BTreeMap::from([("deepseek".into(), config)])
}

#[test]
fn namespaced_routes_preserve_upstream_model_ids() {
    let forwards = services(service());
    let config = forwards.get("deepseek").unwrap();
    assert_eq!(
        model_ids("deepseek", config),
        ["deepseek/deepseek-chat", "deepseek/vendor/other-model"]
    );
    for upstream_model in &config.models {
        let exposed = format!("deepseek/{upstream_model}");
        let (resolved, model) = resolve_model(&forwards, &exposed).unwrap();
        assert_eq!(resolved, config);
        assert_eq!(model, upstream_model);
    }
    for unknown in [
        "deepseek-chat",
        "unknown/deepseek-chat",
        "deepseek/missing",
        "deepseek/",
    ] {
        assert!(resolve_model(&forwards, unknown).is_none());
    }
}

#[test]
fn endpoint_join_retains_prefix_and_query_without_duplicating_api_version() {
    for (base, path, expected) in [
        (
            "https://api.example.com/anthropic",
            "/v1/messages?beta=true",
            "https://api.example.com/anthropic/v1/messages?beta=true",
        ),
        (
            "https://api.example.com/anthropic/",
            "/v1/messages/count_tokens",
            "https://api.example.com/anthropic/v1/messages/count_tokens",
        ),
        (
            "https://api.example.com/anthropic/v1/",
            "/v1/messages?value=a%2Fb&other=hello+world",
            "https://api.example.com/anthropic/v1/messages?value=a%2Fb&other=hello+world",
        ),
        (
            "https://api.example.com",
            "/v1/responses",
            "https://api.example.com/v1/responses",
        ),
        (
            "https://api.example.com/v1",
            "/v1",
            "https://api.example.com/v1",
        ),
        (
            "https://api.example.com/v1",
            "/v10/messages",
            "https://api.example.com/v1/v10/messages",
        ),
        (
            "https://api.example.com/team%20space",
            "/v1/models/model%2Fname",
            "https://api.example.com/team%20space/v1/models/model%2Fname",
        ),
    ] {
        assert_eq!(upstream_url(base, path).unwrap().as_str(), expected);
    }
}

#[test]
fn endpoint_join_rejects_non_paths_and_fragments() {
    for path in [
        "https://other.example/messages",
        "v1/messages",
        "/v1/messages#fragment",
    ] {
        assert!(upstream_url("https://api.example.com", path).is_err());
    }
}

#[test]
fn validates_alternate_header_auth_and_no_auth_services() {
    let mut config = service();
    config.api_key = None;
    assert!(validate_forwards(&services(config.clone())).is_ok());
    config.headers.insert("x-api-key".into(), "secret".into());
    config
        .headers
        .insert("anthropic-version".into(), "2023-06-01".into());
    assert!(validate_forwards(&services(config)).is_ok());
}

#[test]
fn rejects_names_that_conflict_with_builtin_routing() {
    for name in [
        "",
        "deep/seek",
        "api.example",
        "hello world",
        "openai-codex",
        "ANTHROPIC",
        "xai",
        "vercel",
    ] {
        let forwards = BTreeMap::from([(name.into(), service())]);
        assert!(validate_forwards(&forwards).is_err(), "{name}");
    }
    for name in ["deepseek", "my-service_2", "local42"] {
        let forwards = BTreeMap::from([(name.into(), service())]);
        assert!(validate_forwards(&forwards).is_ok(), "{name}");
    }
}

#[test]
fn rejects_non_http_urls_and_urls_with_hidden_credentials() {
    for base_url in [
        "file:///tmp/socket",
        "https://secret@api.example.com",
        "https://user:secret@api.example.com",
        "https://api.example.com?token=secret",
        "https://api.example.com#secret",
        "https://api.example.com/ ",
        "missing-host",
    ] {
        let config = ForwardConfig {
            base_url: base_url.into(),
            ..service()
        };
        let error = validate_forwards(&services(config)).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn rejects_empty_invalid_or_duplicate_model_entries() {
    for models in [
        vec![],
        vec![""],
        vec!["a b"],
        vec!["model\n"],
        vec!["/model"],
        vec!["same", "same"],
    ] {
        let config = ForwardConfig {
            models: models.into_iter().map(str::to_owned).collect(),
            ..service()
        };
        assert!(validate_forwards(&services(config)).is_err());
    }
}

#[test]
fn rejects_invalid_and_transport_headers_without_exposing_values() {
    for (header, value) in [
        ("bad header", "secret"),
        ("x-key", "secret\r\ninjected: yes"),
        ("Connection", "secret"),
        ("Host", "secret"),
        ("Content-Length", "123"),
        ("Transfer-Encoding", "secret"),
        ("Proxy-Authorization", "secret"),
    ] {
        let config = ForwardConfig {
            headers: BTreeMap::from([(header.into(), value.into())]),
            ..service()
        };
        let error = validate_forwards(&services(config)).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn rejects_empty_or_injected_bearer_credentials() {
    for api_key in ["", " ", "secret\r\ninjected: yes"] {
        let config = ForwardConfig {
            api_key: Some(api_key.into()),
            ..service()
        };
        let error = validate_forwards(&services(config)).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn persisted_services_round_trip_with_aliases_and_optional_credentials() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "forwards": {
                "deepseek": {
                    "base_url": "https://api.example.com/anthropic",
                    "models": ["deepseek-chat"]
                }
            },
            "model_aliases": {"anthropic/custom": "deepseek/deepseek-chat"}
        }))
        .unwrap(),
    )
    .unwrap();
    let store = AppConfigStore::new(path);
    let original = store.load().unwrap().unwrap();
    let config = original.forwards.get("deepseek").unwrap();
    assert_eq!(config.api_key, None);
    assert!(config.headers.is_empty());
    store.save(&original).unwrap();
    assert_eq!(store.load().unwrap().unwrap(), original);
}

#[test]
fn invalid_forward_configuration_is_rejected_on_load_and_before_save() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let store = AppConfigStore::new(&path);
    let invalid_config = crate::config::AppConfig {
        forwards: services(ForwardConfig {
            base_url: "file:///tmp/service".into(),
            ..service()
        }),
        ..Default::default()
    };
    assert!(store.save(&invalid_config).is_err());
    assert!(!path.exists());
    fs::write(&path, serde_json::to_vec(&invalid_config).unwrap()).unwrap();
    assert!(store.load().is_err());
}

#[test]
fn old_config_defaults_to_no_forwards_and_does_not_emit_new_field() {
    let config: crate::config::AppConfig = serde_json::from_str("{}").unwrap();
    assert!(config.forwards.is_empty());
    let saved = serde_json::to_value(config).unwrap();
    assert!(saved.get("forwards").is_none());
}
