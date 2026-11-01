//! Model discovery shared by startup and reload.
use super::UpstreamState;
use crate::{
    Result,
    config::{AppConfig, Provider},
    models::resolve_model_list_for_provider_with_config,
    openai::response::ModelList,
};
use std::collections::HashSet;

/// Resolves served model lists, giving explicit config precedence over live discovery.
///
/// # Errors
/// Returns an error when configured model lists cannot be resolved.
pub async fn resolve_model_list_for_upstreams(
    upstreams: &[UpstreamState],
    config: Option<&AppConfig>,
) -> Result<ModelList> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    for upstream in upstreams {
        let models = if config.is_some_and(|config| config.models.contains_key(&upstream.provider))
        {
            resolve_model_list_for_provider_with_config(upstream.provider, config)?
        } else if upstream.provider == Provider::Vercel {
            match upstream.token_manager.credentials().await {
                Ok(credentials) => match upstream.client.list_vercel_models(&credentials).await {
                    Ok(models) => models,
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "falling back to built-in Vercel AI Gateway model list"
                        );
                        resolve_model_list_for_provider_with_config(upstream.provider, config)?
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "falling back to built-in Vercel AI Gateway model list"
                    );
                    resolve_model_list_for_provider_with_config(upstream.provider, config)?
                }
            }
        } else {
            resolve_model_list_for_provider_with_config(upstream.provider, config)?
        };

        for model in models.data {
            if seen.insert(model.id.clone()) {
                ids.push((model.id, model.owned_by));
            }
        }
    }

    Ok(ModelList::from_id_owners(ids))
}
