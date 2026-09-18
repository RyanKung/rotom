#[cfg(unix)]
use super::parse_bind_addresses;
use super::{
    AppConfig, Cli, Command, DEFAULT_MODEL_FALLBACK, Ipv4Cidr, bind_from_config,
    build_update_command, credential_subject, daemon_endpoint_lines, format_bind_addresses,
    format_login_provider_choice, format_models, new_provider_daemon_restart_hint,
    parse_bind_hosts, parse_login_provider_choice, resolve_model_fallback,
    resolve_served_providers, resolve_status_providers, rotom_version_line, token_expiry_message,
    vercel_credentials,
};
use clap::Parser;
use rotom::config::{AuthStore, Credentials, Provider, now_unix};
use rotom::timefmt::format_duration;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct TestAuthStore {
    store: AuthStore,
    path: PathBuf,
}

impl TestAuthStore {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rotom-main-test-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self {
            store: AuthStore::new(path.clone()),
            path,
        }
    }
}

impl Drop for TestAuthStore {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn reuses_shared_duration_formatting() {
    assert_eq!(format_duration(90_061), "1d 01h 01m 01s");
}

#[test]
fn builds_bind_address_from_config() {
    let config = AppConfig {
        bind_host: Some("127.0.0.1".into()),
        bind_port: Some(14550),
        model_fallback: None,
        ..AppConfig::default()
    };

    let addrs = bind_from_config(Some(&config)).unwrap().unwrap();
    assert_eq!(format_bind_addresses(&addrs), "127.0.0.1:14550");
}

#[test]
fn builds_multiple_bind_addresses_from_config() {
    let config = AppConfig {
        bind_host: Some("127.0.0.1,0.0.0.0".into()),
        bind_port: Some(14550),
        model_fallback: None,
        ..AppConfig::default()
    };

    let addrs = bind_from_config(Some(&config)).unwrap().unwrap();
    assert_eq!(
        format_bind_addresses(&addrs),
        "127.0.0.1:14550,0.0.0.0:14550"
    );
}

#[test]
fn parses_comma_separated_cli_bind_addresses() {
    let cli =
        Cli::try_parse_from(["rotom", "serve", "--bind", "127.0.0.1:14550,0.0.0.0:14550"]).unwrap();

    let Command::Serve { bind, .. } = cli.command else {
        panic!("expected serve command");
    };
    assert_eq!(
        format_bind_addresses(&bind.unwrap().into_vec()),
        "127.0.0.1:14550,0.0.0.0:14550"
    );
}

#[test]
fn parses_bind_cidr_membership() {
    let cidr = Ipv4Cidr::parse("192.168.1.0/24").unwrap();

    assert!(cidr.contains("192.168.1.1".parse().unwrap()));
    assert!(cidr.contains("192.168.1.255".parse().unwrap()));
    assert!(!cidr.contains("192.168.2.1".parse().unwrap()));
}

#[test]
#[cfg(unix)]
fn expands_loopback_cidr_bind_host_to_local_interface() {
    let addrs = parse_bind_hosts("127.0.0.0/8", 14550).unwrap();

    assert!(
        addrs
            .iter()
            .any(|addr| addr.to_string() == "127.0.0.1:14550")
    );
}

#[test]
#[cfg(unix)]
fn parses_cidr_cli_bind_address() {
    let addrs = parse_bind_addresses("127.0.0.0/8:14550").unwrap();

    assert!(
        addrs
            .iter()
            .any(|addr| addr.to_string() == "127.0.0.1:14550")
    );
}

#[test]
#[cfg(not(unix))]
fn rejects_cidr_bind_hosts_without_interface_enumeration() {
    let error = parse_bind_hosts("127.0.0.0/8", 14550).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("CIDR bind selectors require Unix local interface enumeration")
    );
}

#[test]
fn builds_update_command_for_latest_release() {
    let command = build_update_command(None);
    let args = command
        .get_args()
        .map(|item| item.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(args, ["install", "--locked", "--force", "rotom"]);
}

#[test]
fn builds_update_command_for_specific_version() {
    let command = build_update_command(Some("0.3.3"));
    let args = command
        .get_args()
        .map(|item| item.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        args,
        [
            "install",
            "--locked",
            "--force",
            "rotom",
            "--version",
            "0.3.3"
        ]
    );
}

#[test]
fn uses_default_model_fallback_when_unset() {
    assert_eq!(
        resolve_model_fallback(None, None),
        Some(DEFAULT_MODEL_FALLBACK.to_owned())
    );
}

#[test]
fn prefers_explicit_model_fallback_over_default() {
    let config = AppConfig {
        model_fallback: Some("gpt-5.4".into()),
        ..AppConfig::default()
    };

    assert_eq!(
        resolve_model_fallback(Some("gpt-5.3-codex".into()), Some(&config)),
        Some("gpt-5.3-codex".into())
    );
    assert_eq!(
        resolve_model_fallback(None, Some(&config)),
        Some("gpt-5.4".into())
    );
}

#[test]
fn served_provider_prefers_cli_provider() {
    let auth = TestAuthStore::new();
    save_provider_credentials(&auth.store, Provider::Codex);
    save_provider_credentials(&auth.store, Provider::Kiro);
    let config = AppConfig {
        provider: Some(Provider::Kiro),
        ..AppConfig::default()
    };

    let providers =
        resolve_served_providers(&auth.store, Some("grok".into()), Some(&config)).unwrap();

    assert_eq!(providers, [Provider::Grok]);
}

#[test]
fn served_provider_prefers_config_provider_over_saved_providers() {
    let auth = TestAuthStore::new();
    save_provider_credentials(&auth.store, Provider::Codex);
    save_provider_credentials(&auth.store, Provider::Kiro);
    let config = AppConfig {
        provider: Some(Provider::Codex),
        ..AppConfig::default()
    };

    let providers = resolve_served_providers(&auth.store, None, Some(&config)).unwrap();

    assert_eq!(providers, [Provider::Codex]);
}

#[test]
fn served_provider_uses_saved_providers_without_selection() {
    let auth = TestAuthStore::new();
    save_provider_credentials(&auth.store, Provider::Kiro);
    save_provider_credentials(&auth.store, Provider::Codex);
    save_provider_credentials(&auth.store, Provider::Vercel);

    let providers = resolve_served_providers(&auth.store, None, None).unwrap();

    assert_eq!(
        providers,
        [Provider::Codex, Provider::Kiro, Provider::Vercel]
    );
}

fn save_provider_credentials(store: &AuthStore, provider: Provider) {
    store
        .save(&Credentials {
            provider,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 90,
            account_id: String::new(),
        })
        .unwrap();
}

#[test]
fn formats_models_grouped_by_provider() {
    let output = format_models(&[Provider::Codex, Provider::Grok, Provider::Vercel]);

    assert!(output.contains("OpenAI (codex)\n  gpt-5.1"));
    assert!(output.contains("  gpt-6-astra\n"));
    assert!(output.contains("  gpt-5.6-sol\n"));
    assert!(output.contains("Grok (grok)\n  grok-4.6"));
    assert!(output.contains("Vercel AI Gateway (vercel)\n  openai/gpt-6-astra"));
    assert!(!output.contains("Kiro (kiro)"));
    assert!(output.contains("\n\nGrok (grok)"));
}

#[test]
fn formats_single_provider_models() {
    let output = format_models(&[Provider::Grok]);

    assert!(output.starts_with("Grok (grok)\n"));
    assert!(output.contains("  grok-4.3\n"));
    assert!(!output.contains("gpt-5.6-sol"));
}

#[test]
fn formats_kiro_models() {
    let output = format_models(&[Provider::Kiro]);

    assert!(output.starts_with("Kiro (kiro)\n  auto\n"));
    assert!(output.contains("  claude-sonnet-4.5\n"));
    assert!(output.contains("  qwen3-coder-next\n"));
}

#[test]
fn formats_vercel_models() {
    let output = format_models(&[Provider::Vercel]);

    assert!(output.starts_with("Vercel AI Gateway (vercel)\n"));
    assert!(output.contains("  openai/gpt-6-astra\n"));
    assert!(output.contains("  anthropic/claude-sonnet-5\n"));
}

#[test]
fn formats_status_version_line() {
    assert_eq!(
        rotom_version_line(),
        format!("rotom: {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn parses_login_provider_choices() {
    assert_eq!(parse_login_provider_choice("").unwrap(), Provider::Codex);
    assert_eq!(parse_login_provider_choice("1").unwrap(), Provider::Codex);
    assert_eq!(parse_login_provider_choice("2").unwrap(), Provider::Grok);
    assert_eq!(parse_login_provider_choice("3").unwrap(), Provider::Kiro);
    assert_eq!(parse_login_provider_choice("4").unwrap(), Provider::Vercel);
    assert_eq!(
        parse_login_provider_choice("openai").unwrap(),
        Provider::Codex
    );
    assert_eq!(parse_login_provider_choice("grok").unwrap(), Provider::Grok);
    assert_eq!(parse_login_provider_choice("kiro").unwrap(), Provider::Kiro);
    assert_eq!(
        parse_login_provider_choice("vercel").unwrap(),
        Provider::Vercel
    );
    assert!(parse_login_provider_choice("5").is_err());
    assert!(parse_login_provider_choice("cursor").is_err());
}

#[test]
fn parses_kiro_login_flag() {
    let cli = Cli::try_parse_from(["rotom", "login", "--kiro"]).unwrap();

    let Command::Login { kiro, provider, .. } = cli.command else {
        panic!("expected login command");
    };
    assert!(kiro);
    assert!(provider.is_none());
}

#[test]
fn rejects_kiro_login_flag_with_provider() {
    assert!(Cli::try_parse_from(["rotom", "login", "--kiro", "--provider", "grok"]).is_err());
}

#[test]
fn removed_cursor_login_flag_is_rejected() {
    assert!(Cli::try_parse_from(["rotom", "login", "--cursor"]).is_err());
}

#[test]
fn formats_login_provider_choice_with_status() {
    let credentials = vec![Credentials {
        provider: Provider::Codex,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: now_unix() + 90,
        account_id: "account".into(),
    }];

    let openai = format_login_provider_choice(Provider::Codex, &credentials);
    let grok = format_login_provider_choice(Provider::Grok, &credentials);
    let kiro = format_login_provider_choice(Provider::Kiro, &credentials);
    let vercel = format_login_provider_choice(Provider::Vercel, &credentials);

    assert!(openai.starts_with("openai (logged in, expires in "));
    assert_eq!(grok, "grok");
    assert_eq!(kiro, "kiro");
    assert_eq!(vercel, "vercel");
}

#[test]
fn credential_subject_does_not_include_account_id() {
    let credentials = Credentials {
        provider: Provider::Codex,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: now_unix() + 90,
        account_id: "account-secret".into(),
    };

    assert_eq!(credential_subject(&credentials), "Codex credentials");
    let token_message = token_expiry_message(&credentials);
    assert!(token_message.contains("(Codex credentials)"));
    assert!(!token_message.contains("account-secret"));
}

#[test]
fn vercel_credentials_store_static_api_key_without_expiry_message() {
    let credentials = vercel_credentials("gateway-key".into());

    assert_eq!(credentials.provider, Provider::Vercel);
    assert_eq!(credentials.access_token, "gateway-key");
    assert!(credentials.refresh_token.is_empty());
    assert_eq!(
        token_expiry_message(&credentials),
        "token does not expire automatically (Vercel credentials)"
    );
}

#[test]
fn resolves_all_saved_status_providers_by_default() {
    let providers = resolve_status_providers(
        vec![
            Credentials {
                provider: Provider::Grok,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: String::new(),
            },
            Credentials {
                provider: Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: "account".into(),
            },
            Credentials {
                provider: Provider::Vercel,
                access_token: "access".into(),
                refresh_token: String::new(),
                expires_at: now_unix() + 90,
                account_id: String::new(),
            },
        ],
        None,
    )
    .unwrap();

    assert_eq!(
        providers,
        [Provider::Codex, Provider::Grok, Provider::Vercel]
    );
}

#[test]
fn filters_status_provider_when_requested() {
    let providers = resolve_status_providers(
        vec![
            Credentials {
                provider: Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: "account".into(),
            },
            Credentials {
                provider: Provider::Grok,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: String::new(),
            },
        ],
        Some(Provider::Grok),
    )
    .unwrap();

    assert_eq!(providers, [Provider::Grok]);
}

#[test]
fn rejects_missing_status_provider() {
    let error = resolve_status_providers(
        vec![Credentials {
            provider: Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 90,
            account_id: "account".into(),
        }],
        Some(Provider::Grok),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "configuration error: not logged in for provider grok; run `rotom login --provider grok` first"
    );
}

#[test]
fn rejects_status_without_any_saved_provider() {
    let error = resolve_status_providers(Vec::new(), Some(Provider::Grok)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "configuration error: not logged in; run `rotom login` first"
    );
}

#[test]
fn formats_daemon_endpoint_lines_with_base_url() {
    let lines = daemon_endpoint_lines("http://127.0.0.1:14550");

    assert!(lines.contains(&"  GET        http://127.0.0.1:14550/health".to_owned()));
    assert!(lines.contains(&"  POST       http://127.0.0.1:14550/v1/chat/completions".to_owned()));
    assert!(lines.contains(&"  GET,POST   http://127.0.0.1:14550/v1/messages/batches".to_owned()));
    assert!(
        lines.contains(&"  POST       http://127.0.0.1:14550/v1/images/generations".to_owned())
    );
    assert!(lines.contains(&"  GET,POST   http://127.0.0.1:14550/v1/tts".to_owned()));
    assert!(lines.contains(&"  GET        http://127.0.0.1:14550/v1/tts/voices".to_owned()));
}

#[test]
fn formats_new_provider_daemon_restart_hint() {
    assert_eq!(
        new_provider_daemon_restart_hint(Provider::Grok),
        "If rotom daemon is already running, run `rotom daemon restart` to serve newly logged-in Grok models."
    );
}
