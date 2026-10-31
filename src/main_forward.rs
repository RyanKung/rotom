/// Selects credential-backed status groups while allowing configured forwards alone.
fn resolve_status_selection(
    credentials: Vec<Credentials>,
    requested_provider: Option<&str>,
    config: Option<&AppConfig>,
) -> Result<Vec<Provider>> {
    if requested_provider.is_some_and(|name| has_forward(config, name))
        || (requested_provider.is_none()
            && credentials.is_empty()
            && config.is_some_and(|item| !item.forwards.is_empty()))
    {
        return Ok(Vec::new());
    }
    let provider = requested_provider.map(str::parse).transpose()?;
    resolve_status_providers(credentials, provider)
}

/// Reports whether a name selects a configured forwarding service.
fn has_forward(config: Option<&AppConfig>, name: &str) -> bool {
    config.is_some_and(|item| item.forwards.contains_key(name))
}

/// Lists only forwards selected by the optional CLI provider filter.
fn selected_forwards<'a>(
    config: Option<&'a AppConfig>,
    requested_provider: Option<&str>,
) -> Vec<(&'a str, &'a rotom::forward::ForwardConfig)> {
    config
        .into_iter()
        .flat_map(|item| item.forwards.iter())
        .filter(|(name, _)| requested_provider.is_none_or(|requested| requested == name.as_str()))
        .map(|(name, forward)| (name.as_str(), forward))
        .collect()
}

/// Formats advertised model IDs and configured aliases without credential material.
fn forward_model_lines(
    name: &str,
    forward: &rotom::forward::ForwardConfig,
    config: &AppConfig,
) -> String {
    let models = rotom::forward::model_ids(name, forward);
    let native = models.iter().map(|model| format!("  {model}"));
    let aliases = config
        .model_aliases
        .iter()
        .filter(|(_, source)| models.contains(source))
        .map(|(alias, source)| format!("  {alias} -> {source}"));
    native.chain(aliases).collect::<Vec<_>>().join("\n")
}

/// Renders configured forwards without implying that their credentials were probed.
fn format_forward_status(config: Option<&AppConfig>, requested_provider: Option<&str>) -> String {
    let Some(config) = config else {
        return String::new();
    };
    let mut output = String::new();
    for (name, forward) in selected_forwards(Some(config), requested_provider) {
        let models = forward_model_lines(name, forward, config);
        let _ = write!(
            output,
            "\nprovider: {name} (forward)\nstatus: configured\nbase_url: {}\nmodels:\n{models}\n",
            forward.base_url
        );
    }
    output
}

/// Formats custom forwarding models alongside explicit aliases for model discovery.
fn format_forward_models(config: Option<&AppConfig>, requested_provider: Option<&str>) -> String {
    let Some(config) = config else {
        return String::new();
    };
    selected_forwards(Some(config), requested_provider)
        .into_iter()
        .map(|(name, forward)| {
            let models = forward_model_lines(name, forward, config);
            format!("{name} (forward)\n{models}\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
