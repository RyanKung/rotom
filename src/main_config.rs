/// Handles interactive configuration, inspection, and reset commands.
fn config_command(command: Option<&ConfigCommand>) -> Result<()> {
    let store = app_config_store()?;
    match command {
        None => configure(&store),
        Some(ConfigCommand::Show) => show_config(&store),
        Some(ConfigCommand::Reset) => reset_config(&store),
    }
}

/// Maps daemon-specific CLI requests to the platform-specific service helpers.
fn daemon_command(command: DaemonCommand, verbosity: u8) -> Result<()> {
    match command {
        DaemonCommand::Install(options) => {
            daemon::install(&resolve_daemon_install_options(options, verbosity)?)
        }
        DaemonCommand::Reinstall(options) => {
            daemon::reinstall(&resolve_daemon_install_options(options, verbosity)?)
        }
        DaemonCommand::Start => daemon::start(),
        DaemonCommand::Restart => daemon::restart(),
        DaemonCommand::Status => daemon::status(),
        DaemonCommand::Stop => daemon::stop(),
        DaemonCommand::Uninstall => daemon::uninstall(),
    }
}

/// Resolves service settings while preserving the origin of the model fallback.
///
/// Only an explicit CLI or environment fallback belongs in the service arguments.
/// Leaving it absent lets `serve` read the current config or built-in default on
/// every start, rather than freezing the fallback that existed at installation.
fn resolve_daemon_install_options(
    options: DaemonInstallCliOptions,
    verbosity: u8,
) -> Result<DaemonInstallOptions> {
    let config_store = app_config_store()?;
    let config = config_store.load()?;
    let explicit_api_key = options.api_key.clone();
    let effective_bind = if let Some(bind) = options.bind {
        bind.into_vec()
    } else {
        bind_from_config(config.as_ref())?.unwrap_or_else(default_binds)
    };
    let effective_auth_file = options
        .auth_file
        .or_else(|| config_auth_file(config.as_ref()));
    let effective_provider = options
        .provider
        .map(|provider| provider.parse::<Provider>())
        .transpose()?;
    if let Some(api_key) = explicit_api_key {
        persist_daemon_api_key(&config_store, config, api_key)?;
    }
    Ok(DaemonInstallOptions {
        executable: options.executable.map_or_else(std::env::current_exe, Ok)?,
        bind: format_bind_addresses(&effective_bind),
        auth_file: effective_auth_file,
        verbosity,
        provider: effective_provider,
        model_fallback: options.model_fallback,
    })
}

fn persist_daemon_api_key(
    store: &AppConfigStore,
    existing: Option<AppConfig>,
    api_key: String,
) -> Result<()> {
    let mut config = existing.unwrap_or_default();
    config.api_key = Some(api_key);
    store.save(&config)
}

/// Resolves the persisted runtime config store path.
fn app_config_store() -> Result<AppConfigStore> {
    AppConfigStore::from_default_path()
}

/// Loads the persisted runtime configuration when present.
fn load_app_config() -> Result<Option<AppConfig>> {
    app_config_store()?.load()
}

/// Interactively prompts for runtime defaults and saves them to disk.
fn configure(store: &AppConfigStore) -> Result<()> {
    let existing = store.load()?.unwrap_or_default();
    let bind_host = prompt_string(
        "Bind host(s)",
        existing.bind_host.as_deref().unwrap_or("127.0.0.1"),
    )?;
    let bind_port = prompt_port("Bind port", existing.bind_port.unwrap_or(14550))?;
    let api_key = prompt_optional_string(
        "Local API key (leave blank to disable)",
        existing.api_key.as_deref(),
    )?;
    let auth_file = prompt_optional_path(
        "Credential file path (leave blank for default ~/.rotom/auth.json)",
        existing.auth_file.as_deref(),
    )?;
    let provider = prompt_string("Provider", existing.provider.unwrap_or_default().as_str())?
        .parse::<Provider>()?;
    let model_fallback = prompt_optional_string(
        "Fallback model for unsupported Anthropic ids (leave blank for default gpt-6-sol)",
        existing
            .model_fallback
            .as_deref()
            .or(Some(DEFAULT_MODEL_FALLBACK)),
    )?;

    let config = AppConfig {
        bind_host: Some(bind_host),
        bind_port: Some(bind_port),
        auth_file,
        api_key,
        model_fallback,
        models: existing.models,
        provider: Some(provider),
    };
    store.save(&config)?;
    println!("saved runtime config to {}", store.path().display());
    Ok(())
}

fn show_config(store: &AppConfigStore) -> Result<()> {
    match store.load()? {
        Some(config) => {
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        None => Err(Error::config(format!(
            "no runtime config found at {}; run `rotom config` first",
            store.path().display()
        ))),
    }
}
