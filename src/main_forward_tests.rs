use super::{format_forward_models, format_forward_status, resolve_status_selection};
use rotom::config::{AppConfig, Credentials, Provider};
use serde_json::json;

fn forwarding_config() -> AppConfig {
    serde_json::from_value(json!({
        "forwards": {
            "deepseek": {
                "base_url": "https://api.deepseek.com/anthropic",
                "api_key": "secret-upstream-key",
                "models": ["deepseek-chat"]
            }
        },
        "model_aliases": {"anthropic/my-deepseek": "deepseek/deepseek-chat"}
    }))
    .unwrap()
}

#[test]
fn status_lists_forwards_without_oauth_or_secrets() {
    let config = forwarding_config();
    assert_eq!(
        resolve_status_selection(Vec::new(), None, Some(&config)).unwrap(),
        Vec::<Provider>::new()
    );
    let output = format_forward_status(Some(&config), None);
    assert!(output.contains("provider: deepseek (forward)"));
    assert!(output.contains("anthropic/my-deepseek -> deepseek/deepseek-chat"));
    assert!(!output.contains("secret-upstream-key"));
    assert_eq!(format_forward_status(Some(&config), Some("codex")), "");
    assert!(format_forward_models(Some(&config), None).contains("deepseek/deepseek-chat"));
    assert_eq!(
        resolve_status_selection(
            vec![Credentials {
                provider: Provider::Codex,
                access_token: "x".into(),
                refresh_token: "x".into(),
                expires_at: 0,
                account_id: String::new(),
            }],
            Some("deepseek"),
            Some(&config)
        )
        .unwrap(),
        Vec::<Provider>::new()
    );
}
