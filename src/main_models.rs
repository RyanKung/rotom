fn models(provider: Option<String>) -> Result<()> {
    let config = load_app_config()?;
    if provider
        .as_deref()
        .is_some_and(|name| has_forward(config.as_ref(), name))
    {
        print!(
            "{}",
            format_forward_models(config.as_ref(), provider.as_deref())
        );
        return Ok(());
    }
    let providers = match provider.as_deref() {
        Some(provider) => vec![provider.parse()?],
        None => vec![
            Provider::Codex,
            Provider::Grok,
            Provider::Kiro,
            Provider::Vercel,
        ],
    };
    print!("{}", format_models(&providers, config.as_ref()));
    let forwards = format_forward_models(config.as_ref(), provider.as_deref());
    if !forwards.is_empty() {
        print!("\n{forwards}");
    }
    Ok(())
}

/// Formats the model lists selected by runtime config for each provider.
fn format_models(providers: &[Provider], config: Option<&AppConfig>) -> String {
    let groups = providers
        .iter()
        .copied()
        .map(|provider| (provider, provider_model_ids(provider, config)))
        .collect::<Vec<_>>();
    format_model_groups(&groups)
}

fn format_model_groups(groups: &[(Provider, Vec<String>)]) -> String {
    groups
        .iter()
        .map(|(provider, models)| {
            let models = model_lines(models.clone()).join("\n");
            format!(
                "{} ({provider})\n{models}\n",
                model_provider_label(*provider)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const fn model_provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "OpenAI",
        Provider::Grok => "Grok",
        Provider::Kiro => "Kiro",
        Provider::Vercel => "Vercel AI Gateway",
    }
}

/// Resolves IDs from a provider's configured replacement or built-in defaults.
fn provider_model_ids(provider: Provider, config: Option<&AppConfig>) -> Vec<String> {
    resolve_model_ids_for_provider_with_config(provider, config)
}

fn model_lines(models: Vec<String>) -> Vec<String> {
    models
        .into_iter()
        .map(|model| format!("  {model}"))
        .collect()
}
