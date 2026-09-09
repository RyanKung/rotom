fn resolve_model_fallback(
    cli_or_option: Option<String>,
    config: Option<&AppConfig>,
) -> Option<String> {
    cli_or_option
        .or_else(|| config_string(config, |item| item.model_fallback.clone()))
        .or_else(|| Some(DEFAULT_MODEL_FALLBACK.to_owned()))
}

fn resolve_login_provider(
    store: &AuthStore,
    provider: Option<String>,
    kiro: bool,
) -> Result<Provider> {
    if kiro {
        return Ok(Provider::Kiro);
    }
    provider.map_or_else(|| prompt_login_provider(store), |value| value.parse())
}

fn prompt_login_provider(store: &AuthStore) -> Result<Provider> {
    let credentials = store.load_all()?;
    println!("Select OAuth provider:");
    for (index, provider) in LOGIN_PROVIDERS.iter().enumerate() {
        println!(
            "[{}] {}",
            index + 1,
            format_login_provider_choice(*provider, &credentials)
        );
    }
    print!("Provider [1]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    parse_login_provider_choice(input.trim())
}

const LOGIN_PROVIDERS: [Provider; 3] = [Provider::Codex, Provider::Grok, Provider::Kiro];

fn parse_login_provider_choice(value: &str) -> Result<Provider> {
    match value {
        "" | "1" => Ok(Provider::Codex),
        "2" => Ok(Provider::Grok),
        "3" => Ok(Provider::Kiro),
        other => other.parse(),
    }
}

fn format_login_provider_choice(provider: Provider, credentials: &[Credentials]) -> String {
    let label = login_provider_label(provider);
    credentials
        .iter()
        .find(|item| item.provider == provider)
        .map_or_else(
            || label.to_owned(),
            |item| format!("{label} ({})", login_provider_status(item)),
        )
}

const fn login_provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "openai",
        Provider::Grok => "grok",
        Provider::Kiro => "kiro",
    }
}

fn login_provider_status(credentials: &Credentials) -> String {
    let remaining_secs = credentials.expires_at.saturating_sub(now_unix());
    if remaining_secs == 0 {
        "logged in, expired".to_owned()
    } else {
        format!("logged in, expires in {}", format_duration(remaining_secs))
    }
}

fn resolve_served_providers(
    store: &AuthStore,
    cli_or_option: Option<String>,
    config: Option<&AppConfig>,
) -> Result<Vec<Provider>> {
    if let Some(value) = cli_or_option {
        return Ok(vec![value.parse()?]);
    }

    if let Some(provider) = config.and_then(|item| item.provider) {
        return Ok(vec![provider]);
    }

    let mut providers = store
        .load_all()?
        .into_iter()
        .map(|credentials| credentials.provider)
        .collect::<Vec<_>>();
    if providers.is_empty() {
        providers.push(config.and_then(|item| item.provider).unwrap_or_default());
    }
    providers.sort_unstable();
    providers.dedup();
    Ok(providers)
}

fn reset_config(store: &AppConfigStore) -> Result<()> {
    store.delete()?;
    println!("removed runtime config at {}", store.path().display());
    Ok(())
}

/// Runs the interactive OAuth login flow and persists the resulting credentials.
async fn login(store: AuthStore, provider: Provider, originator: &str) -> Result<()> {
    let http = Client::new();
    let existing_providers = store
        .load_all()?
        .into_iter()
        .map(|credentials| credentials.provider)
        .collect::<Vec<_>>();
    let is_new_provider = !existing_providers.contains(&provider);
    let show_daemon_restart_hint = is_new_provider && !existing_providers.is_empty();
    if provider == Provider::Kiro {
        return login_kiro(store, http, show_daemon_restart_hint).await;
    }
    let flow = match provider {
        Provider::Codex => create_authorization_flow(originator)?,
        Provider::Grok => {
            GrokOAuthClient::default()
                .create_authorization_flow()
                .await?
        }
        Provider::Kiro => unreachable!("Kiro login is handled before generic OAuth flow"),
    };
    println!(
        "Open this URL to authenticate with {}:\n{}\n",
        provider.display_name(),
        flow.authorize_url
    );
    println!(
        "After login, your browser may fail to load the localhost callback. Copy the full address from the browser address bar and paste it here."
    );

    let code = prompt_authorization_code(&flow.state)?;
    let credentials = match provider {
        Provider::Codex => {
            CodexOAuthClient::new(http)
                .exchange_authorization_code(&code, &flow.verifier)
                .await?
        }
        Provider::Grok => {
            GrokOAuthClient::default()
                .exchange_authorization_code(&code, &flow.verifier)
                .await?
        }
        Provider::Kiro => unreachable!("Kiro login is handled before generic OAuth flow"),
    };
    store.save(&credentials)?;
    let subject = credential_subject(&credentials);
    println!(
        "logged in {subject} and saved credentials to {}",
        store.path().display()
    );
    if show_daemon_restart_hint {
        println!("{}", new_provider_daemon_restart_hint(provider));
    }
    Ok(())
}

async fn login_kiro(store: AuthStore, http: Client, show_daemon_restart_hint: bool) -> Result<()> {
    let flow = KiroOAuthClient::create_authorization_flow()?;
    println!(
        "Open this URL to authenticate with Kiro:\n{}\n",
        flow.authorize_url
    );
    println!(
        "After login, paste the full Kiro callback URL from the browser address bar, including login_option and code."
    );

    let callback = prompt_kiro_authorization_callback(&flow.state)?;
    let credentials = KiroOAuthClient::new(http)
        .exchange_authorization_callback(&callback, &flow.verifier)
        .await?;
    store.save(&credentials)?;
    let subject = credential_subject(&credentials);
    println!(
        "logged in {subject} and saved credentials to {}",
        store.path().display()
    );
    if show_daemon_restart_hint {
        println!("{}", new_provider_daemon_restart_hint(Provider::Kiro));
    }
    Ok(())
}

fn prompt_kiro_authorization_callback(expected_state: &str) -> Result<KiroAuthorizationCallback> {
    print!("Paste the full Kiro callback URL: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let callback = parse_kiro_authorization_callback(&input)?;
    if callback
        .state
        .as_deref()
        .is_some_and(|state| state != expected_state)
    {
        return Err(Error::oauth("state mismatch"));
    }
    Ok(callback)
}

fn new_provider_daemon_restart_hint(provider: Provider) -> String {
    format!(
        "If rotom daemon is already running, run `rotom daemon restart` to serve newly logged-in {} models.",
        provider.display_name()
    )
}

/// Forces a refresh of the saved OAuth credentials and writes them back to disk.
async fn refresh(store: AuthStore, provider: Option<String>) -> Result<()> {
    let credentials = if let Some(provider) = provider {
        let provider = provider.parse::<Provider>()?;
        store.load_provider(provider)?.ok_or_else(|| {
            Error::config(format!(
                "not logged in for {provider}; run `rotom login --provider {provider}` first"
            ))
        })?
    } else {
        let all = store.load_all()?;
        if all.is_empty() {
            return Err(Error::config("not logged in; run `rotom login` first"));
        }
        let mut failed = false;
        for credentials in all {
            match refresh_credentials(&credentials).await {
                Ok(refreshed) => {
                    store.save(&refreshed)?;
                    println!("refreshed {}", credential_subject(&refreshed));
                }
                Err(error) => {
                    failed = true;
                    eprintln!(
                        "warning: failed to refresh {}: {error}",
                        credential_subject(&credentials)
                    );
                }
            }
        }
        if failed {
            return Err(Error::oauth(
                "one or more provider refreshes failed; rerun `rotom refresh --provider <name>` to retry the failed account",
            ));
        }
        return Ok(());
    };

    let refreshed = refresh_credentials(&credentials).await?;
    store.save(&refreshed)?;
    println!("refreshed {}", credential_subject(&refreshed));
    Ok(())
}

async fn refresh_credentials(credentials: &Credentials) -> Result<Credentials> {
    match credentials.provider {
        Provider::Codex => {
            CodexOAuthClient::default()
                .refresh_token(&credentials.refresh_token)
                .await
        }
        Provider::Grok => {
            GrokOAuthClient::default()
                .refresh_token(&credentials.refresh_token)
                .await
        }
        Provider::Kiro => {
            KiroOAuthClient::default()
                .refresh_token(&credentials.refresh_token)
                .await
        }
    }
}

fn kiro_command(command: KiroCommand) -> Result<()> {
    match command {
        KiroCommand::Import {
            auth_file,
            source,
            path,
        } => import_kiro(auth_file, &source, path.as_deref()),
    }
}

fn import_kiro(
    auth_file: Option<PathBuf>,
    source: &str,
    path: Option<&std::path::Path>,
) -> Result<()> {
    let store = auth_store(auth_file)?;
    let (credentials, imported_from) = import_kiro_credentials(source, path)?;
    store.save(&credentials)?;
    println!(
        "imported Kiro credentials from {imported_from} and saved them to {}",
        store.path().display()
    );
    println!("Kiro credentials are ready for refresh, status, model listing, and API serving.");
    Ok(())
}

fn import_kiro_credentials(
    source: &str,
    path: Option<&std::path::Path>,
) -> Result<(Credentials, String)> {
    match source.trim().to_ascii_lowercase().as_str() {
        "auto" => {
            if path.is_some() {
                return Err(Error::config(
                    "use --from cli or --from desktop when passing an explicit Kiro credential path",
                ));
            }
            let cli_path = default_cli_database_path()?;
            if cli_path.exists() {
                return Ok((
                    KiroOAuthClient::import_cli_database(&cli_path)?,
                    cli_path.display().to_string(),
                ));
            }
            let desktop_path = default_desktop_token_path()?;
            if desktop_path.exists() {
                return Ok((
                    KiroOAuthClient::import_desktop_file(&desktop_path)?,
                    desktop_path.display().to_string(),
                ));
            }
            Err(Error::config(
                "no Kiro CLI or desktop credential store found; pass --from cli --path ... or --from desktop --path ...",
            ))
        }
        "cli" => {
            let owned;
            let path = if let Some(path) = path {
                path
            } else {
                owned = default_cli_database_path()?;
                &owned
            };
            Ok((
                KiroOAuthClient::import_cli_database(path)?,
                path.display().to_string(),
            ))
        }
        "desktop" | "ide" => {
            let owned;
            let path = if let Some(path) = path {
                path
            } else {
                owned = default_desktop_token_path()?;
                &owned
            };
            Ok((
                KiroOAuthClient::import_desktop_file(path)?,
                path.display().to_string(),
            ))
        }
        other => Err(Error::config(format!(
            "unknown Kiro credential source: {other}; expected auto, cli, or desktop"
        ))),
    }
}
