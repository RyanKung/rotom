/// Builds the initial configuration snapshot and starts reloadable listeners.
async fn serve_command(
    bind: Option<BindAddresses>,
    auth_file: Option<PathBuf>,
    api_key: Option<String>,
    provider: Option<String>,
    model_fallback: Option<String>,
    log_level: LogLevel,
) -> Result<()> {
    logging::init(log_level)?;
    let config = load_app_config()?;
    let effective_bind = if let Some(bind) = bind {
        bind.into_vec()
    } else {
        bind_from_config(config.as_ref())?.unwrap_or_else(default_binds)
    };
    let effective_auth_file = auth_file.or_else(|| config_auth_file(config.as_ref()));
    let reload_settings = ReloadSettings {
        store: app_config_store()?,
        startup_config: config.clone().unwrap_or_default(),
        api_key_override: api_key.clone(),
        fallback_override: model_fallback.clone(),
        default_fallback: DEFAULT_MODEL_FALLBACK.to_owned(),
    };
    let effective_api_key =
        api_key.or_else(|| config_string(config.as_ref(), |item| item.api_key.clone()));
    let effective_model_fallback = resolve_model_fallback(model_fallback, config.as_ref());
    let http = Client::new();
    let store = auth_store(effective_auth_file)?;
    let has_forwards = config
        .as_ref()
        .is_some_and(|item| !item.forwards.is_empty());
    let explicitly_selected_provider = provider.is_some()
        || (!has_forwards && config.as_ref().and_then(|item| item.provider).is_some());
    let providers = resolve_served_providers(&store, provider, config.as_ref())?;
    let mut upstreams = Vec::new();
    for provider in &providers {
        let token_manager = TokenManager::new_for_provider(store.clone(), *provider, http.clone());
        if let Err(error) = token_manager.credentials().await {
            if explicitly_selected_provider {
                return Err(error);
            }
            eprintln!(
                "warning: skipping provider {provider} during multi-provider startup: {error}"
            );
            continue;
        }
        upstreams.push(UpstreamState {
            provider: *provider,
            token_manager,
            client: CodexClient::new_for_provider(http.clone(), *provider),
        });
    }
    let model_list = resolve_model_list_for_upstreams(&upstreams, config.as_ref()).await?;
    println!("listening on {}", format_bind_urls(&effective_bind));
    for upstream in &upstreams {
        spawn_token_expiry_display(upstream.token_manager.clone());
    }
    serve_reloadable(
        &effective_bind,
        AppState::new_multi_with_forwards(
            upstreams,
            effective_api_key,
            model_list,
            effective_model_fallback,
            config
                .as_ref()
                .map_or_else(Default::default, |item| item.model_aliases.clone()),
            config.map_or_else(Default::default, |item| item.forwards),
        )?,
        reload_settings,
    )
    .await
}
