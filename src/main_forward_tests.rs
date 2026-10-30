use super::{format_forward_models, format_forward_status, resolve_status_selection};
use rotom::config::{AppConfig, Credentials, Provider};
use serde_json::json;

fn forwarding_config() -> AppConfig {
    serde_json::from_value(json!({
        "forwards": {
            "deepseek": {
                "base_url": "https://api.deepseek.com/anthropic",
                "api_key": "secret-upstream-key",
                "headers": {"x-custom-auth": "secret-header-value"},
                "models": ["deepseek-chat", "deepseek-reasoner"]
            },
            "local": {
                "base_url": "http://127.0.0.1:9999/v1",
                "models": ["custom/model"]
            }
        },
        "model_aliases": {
            "anthropic/my-deepseek": "deepseek/deepseek-chat",
            "unused-alias": "deepseek/missing-model",
            "local-alias": "local/custom/model",
            "builtin-alias": "gpt-6-astra"
        }
    }))
    .unwrap()
}

fn codex_credentials() -> Credentials {
    Credentials {
        provider: Provider::Codex,
        access_token: "unused".into(),
        refresh_token: "unused".into(),
        expires_at: 0,
        account_id: String::new(),
    }
}

#[test]
fn configured_forwards_allow_status_without_oauth_credentials() {
    let config = forwarding_config();
    assert!(
        resolve_status_selection(Vec::new(), None, Some(&config))
            .unwrap()
            .is_empty()
    );
    assert!(
        resolve_status_selection(Vec::new(), Some("deepseek"), Some(&config))
            .unwrap()
            .is_empty()
    );
    assert!(resolve_status_selection(Vec::new(), Some("codex"), Some(&config)).is_err());
    assert!(resolve_status_selection(Vec::new(), Some("unknown"), Some(&config)).is_err());
    assert!(resolve_status_selection(Vec::new(), None, None).is_err());
}

#[test]
fn forwarding_status_selection_preserves_builtin_provider_filters() {
    let config = forwarding_config();
    for filter in [None, Some("codex")] {
        assert_eq!(
            resolve_status_selection(vec![codex_credentials()], filter, Some(&config)).unwrap(),
            vec![Provider::Codex]
        );
    }
    assert!(
        resolve_status_selection(vec![codex_credentials()], Some("deepseek"), Some(&config))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn forwarding_status_lists_urls_models_and_valid_aliases_without_secrets() {
    let config = forwarding_config();
    let output = format_forward_status(Some(&config), None);
    assert!(output.contains("provider: deepseek (forward)\nstatus: configured\n"));
    assert!(output.contains("base_url: https://api.deepseek.com/anthropic\n"));
    assert!(output.contains("  deepseek/deepseek-chat\n"));
    assert!(output.contains("  anthropic/my-deepseek -> deepseek/deepseek-chat\n"));
    assert!(output.contains("provider: local (forward)"));
    for hidden in [
        "secret-upstream-key",
        "secret-header-value",
        "x-custom-auth",
        "unused-alias",
        "builtin-alias",
        "authenticated",
    ] {
        assert!(!output.contains(hidden));
    }
}

#[test]
fn forwarding_cli_output_obeys_provider_selection() {
    let config = forwarding_config();
    let status = format_forward_status(Some(&config), Some("deepseek"));
    let models = format_forward_models(Some(&config), Some("deepseek"));
    for output in [&status, &models] {
        assert!(output.contains("deepseek/deepseek-chat"));
        assert!(output.contains("anthropic/my-deepseek -> deepseek/deepseek-chat"));
        assert!(!output.contains("local/custom/model"));
        assert!(!output.contains("local-alias"));
    }
    assert!(format_forward_status(Some(&config), Some("codex")).is_empty());
    assert!(format_forward_models(Some(&config), Some("codex")).is_empty());
    assert!(format_forward_status(None, None).is_empty());
    assert!(format_forward_models(None, None).is_empty());
    assert!(format_forward_models(Some(&config), None).contains("local/custom/model"));
}
