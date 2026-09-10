fn models(provider: Option<String>) -> Result<()> {
    let providers = match provider {
        Some(provider) => vec![provider.parse()?],
        None => vec![Provider::Codex, Provider::Grok, Provider::Kiro],
    };
    print!("{}", format_models(&providers));
    Ok(())
}

fn format_models(providers: &[Provider]) -> String {
    let groups = providers
        .iter()
        .copied()
        .map(|provider| (provider, provider_model_ids(provider)))
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
    }
}

fn provider_model_ids(provider: Provider) -> Vec<String> {
    resolve_model_ids_for_provider(provider)
}

fn model_lines(models: Vec<String>) -> Vec<String> {
    models
        .into_iter()
        .map(|model| format!("  {model}"))
        .collect()
}
