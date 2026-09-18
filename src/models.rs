use crate::{Result, config::Provider, openai::response::ModelList};
use std::collections::HashSet;

/// Default OpenAI-compatible model identifiers exposed by the project.
pub const OPENCLAW_CODEX_MODELS: &[&str] = &[
    "gpt-5.1",
    "gpt-5.1-codex-max",
    "gpt-5.1-codex-mini",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-6-astra",
];

/// Default Grok model identifiers exposed by the project.
pub const GROK_MODELS: &[&str] = &["grok-4.6", "grok-4.5", "grok-4.3", "grok-build-0.1"];

/// Default Kiro model identifiers exposed by the local Kiro CLI model list.
pub const KIRO_MODELS: &[&str] = &[
    "auto",
    "claude-opus-4.8",
    "claude-opus-4.7",
    "claude-opus-4.6",
    "claude-sonnet-4.6",
    "claude-opus-4.5",
    "claude-sonnet-4.5",
    "claude-sonnet-4",
    "claude-haiku-4.5",
    "deepseek-3.2",
    "minimax-m2.5",
    "minimax-m2.1",
    "glm-5",
    "qwen3-coder-next",
];

/// Default Vercel AI Gateway model identifiers exposed by the project.
pub const VERCEL_MODELS: &[&str] = &[
    "openai/gpt-6-astra",
    "anthropic/claude-sonnet-5",
    "anthropic/claude-opus-5",
];

const HIGHLIGHT_MODEL_LIMIT: usize = 4;

/// Resolves the default model identifiers into a trimmed, de-duplicated list.
#[must_use]
pub fn resolve_model_ids() -> Vec<String> {
    resolve_model_ids_for_provider(Provider::Codex)
}

/// Resolves model identifiers for the selected provider.
#[must_use]
pub fn resolve_model_ids_for_provider(provider: Provider) -> Vec<String> {
    let ids = match provider {
        Provider::Codex => OPENCLAW_CODEX_MODELS,
        Provider::Grok => GROK_MODELS,
        Provider::Kiro => KIRO_MODELS,
        Provider::Vercel => VERCEL_MODELS,
    };
    normalize_model_ids(ids.iter().map(ToString::to_string))
}

/// Selects the strongest model identifiers for a provider from an available model list.
#[must_use]
pub fn highlight_model_ids_for_provider(provider: Provider, available: &[String]) -> Vec<String> {
    let normalized = normalize_model_ids(available.iter().cloned());
    let mut ranked = normalized
        .iter()
        .filter_map(|id| highlight_model_rank(provider, id).map(|rank| (id.clone(), rank)))
        .collect::<Vec<_>>();

    ranked.sort_by(|(left_id, left_rank), (right_id, right_rank)| {
        right_rank
            .cmp(left_rank)
            .then_with(|| left_id.cmp(right_id))
    });

    let mut highlights = Vec::new();
    for (id, _) in ranked {
        highlights.push(id);
        if highlights.len() >= HIGHLIGHT_MODEL_LIMIT {
            break;
        }
    }

    if highlights.is_empty() {
        normalized
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .take(HIGHLIGHT_MODEL_LIMIT)
            .collect()
    } else {
        highlights
    }
}

/// Builds a [`ModelList`] from the default model identifiers.
///
/// # Errors
///
/// This currently forwards construction through [`ModelList::from_ids`] and is
/// fallible only to preserve the crate-wide result-based call sites.
pub fn resolve_model_list() -> Result<ModelList> {
    resolve_model_list_for_provider(Provider::Codex)
}

/// Builds a [`ModelList`] for the selected provider.
///
/// # Errors
///
/// This currently forwards construction through [`ModelList::from_ids`] and is
/// fallible only to preserve the crate-wide result-based call sites.
pub fn resolve_model_list_for_provider(provider: Provider) -> Result<ModelList> {
    let owner = model_owner(provider);
    Ok(ModelList::from_id_owners(
        resolve_model_ids_for_provider(provider)
            .into_iter()
            .map(|id| (id, owner)),
    ))
}

/// Returns the supported provider implied by a model identifier.
///
/// Models that belonged to the removed Cursor integration return `None` so
/// callers cannot accidentally route them to a different upstream.
#[must_use]
pub fn provider_for_model(model: &str) -> Option<Provider> {
    let normalized = model.strip_prefix("openai-codex/").unwrap_or(model);
    if removed_cursor_model(normalized) {
        return None;
    }
    if is_explicit_vercel_model(normalized) || is_gateway_model(normalized) {
        return Some(Provider::Vercel);
    }
    let normalized = normalized
        .strip_prefix("xai/")
        .or_else(|| normalized.strip_prefix("grok/"))
        .or_else(|| normalized.strip_prefix("kiro/"))
        .unwrap_or(normalized);
    if normalized.starts_with("grok-") {
        Some(Provider::Grok)
    } else if normalized == "auto"
        || normalized.starts_with("claude-")
        || normalized.starts_with("deepseek-")
        || normalized.starts_with("minimax-")
        || normalized.starts_with("glm-")
        || normalized.starts_with("qwen")
    {
        Some(Provider::Kiro)
    } else {
        Some(Provider::Codex)
    }
}

fn removed_cursor_model(model: &str) -> bool {
    model.starts_with("cursor/") || model.starts_with("sonnet-") || model == "opus"
}

/// Returns whether a model id uses rotom's explicit Vercel routing prefix.
fn is_explicit_vercel_model(model: &str) -> bool {
    model
        .strip_prefix("vercel/")
        .is_some_and(|stripped| !stripped.is_empty())
}

/// Returns whether a model id uses Vercel AI Gateway's provider/model namespace shape.
fn is_gateway_model(model: &str) -> bool {
    let Some((namespace, name)) = model.split_once('/') else {
        return false;
    };
    !namespace.is_empty()
        && !name.is_empty()
        && !matches!(
            namespace,
            "cursor" | "grok" | "kiro" | "openai-codex" | "xai"
        )
}

/// Builds a [`ModelList`] containing all models for the supplied providers.
///
/// # Errors
///
/// This currently forwards construction through [`ModelList::from_ids`] and is
/// fallible only to preserve the crate-wide result-based call sites.
pub fn resolve_model_list_for_providers(providers: &[Provider]) -> Result<ModelList> {
    let mut seen = HashSet::new();
    let ids = providers
        .iter()
        .copied()
        .flat_map(|provider| {
            let owner = model_owner(provider);
            resolve_model_ids_for_provider(provider)
                .into_iter()
                .map(move |id| (id, owner))
        })
        .filter(|(id, _)| seen.insert(id.clone()))
        .collect::<Vec<_>>();
    Ok(ModelList::from_id_owners(ids))
}

const fn model_owner(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "openai-codex",
        Provider::Grok => "xai",
        Provider::Kiro => "kiro",
        Provider::Vercel => "vercel-ai-gateway",
    }
}

fn normalize_model_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn highlight_model_rank(provider: Provider, id: &str) -> Option<ModelRank> {
    let normalized = id
        .strip_prefix("openai-codex/")
        .or_else(|| id.strip_prefix("xai/"))
        .or_else(|| id.strip_prefix("grok/"))
        .or_else(|| id.strip_prefix("kiro/"))
        .or_else(|| id.strip_prefix("vercel/"))
        .unwrap_or(id);

    if is_lightweight_highlight_variant(normalized) {
        return None;
    }

    let (major, minor) = strongest_version(normalized)?;
    let family = match provider {
        Provider::Codex => codex_family_rank(normalized)?,
        Provider::Grok => {
            if normalized.starts_with("grok-") {
                80
            } else {
                return None;
            }
        }
        Provider::Kiro => kiro_family_rank(normalized)?,
        Provider::Vercel => vercel_family_rank(normalized)?,
    };
    let effort = effort_rank(normalized);

    Some(ModelRank {
        score: highlight_score(provider, major, minor, family, effort),
        major,
        minor,
        family,
        effort,
    })
}

fn strongest_version(id: &str) -> Option<(u16, u16)> {
    let bytes = id.as_bytes();
    let mut best = None;
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }

        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let major = id[start..index].parse::<u16>().ok()?;
        let mut minor = 0;
        if index < bytes.len()
            && (bytes[index] == b'.'
                || (bytes[index] == b'-'
                    && index + 1 < bytes.len()
                    && bytes[index + 1].is_ascii_digit()))
        {
            let minor_start = index + 1;
            let mut minor_end = minor_start;
            while minor_end < bytes.len() && bytes[minor_end].is_ascii_digit() {
                minor_end += 1;
            }
            if minor_end > minor_start {
                minor = id[minor_start..minor_end].parse::<u16>().ok()?;
                index = minor_end;
            }
        }

        best = Some(best.map_or((major, minor), |current| (major, minor).max(current)));
    }
    best
}

fn is_lightweight_highlight_variant(id: &str) -> bool {
    id == "auto"
        || id.contains("-mini")
        || id.contains("-haiku")
        || id.contains("-fast")
        || id.contains("-flash")
        || id.contains("-nano")
        || id.contains("-spark")
}

fn codex_family_rank(id: &str) -> Option<u16> {
    if !id.starts_with("gpt-") {
        return None;
    }

    if id.starts_with("gpt-5.6-sol") {
        Some(100)
    } else if id.starts_with("gpt-5.6-terra") {
        Some(95)
    } else if id.starts_with("gpt-5.6-luna") {
        Some(90)
    } else {
        Some(80)
    }
}

fn kiro_family_rank(id: &str) -> Option<u16> {
    if id.starts_with("claude-fable-") {
        Some(105)
    } else if id.starts_with("claude-opus-") || id == "opus" {
        Some(95)
    } else if id.starts_with("claude-sonnet-") {
        Some(85)
    } else if id.starts_with("qwen") && id.contains("coder") {
        Some(70)
    } else if id.starts_with("deepseek-") || id.starts_with("glm-") || id.starts_with("minimax-") {
        Some(60)
    } else {
        None
    }
}

/// Scores a Vercel model by applying the family rank to the model segment.
fn vercel_family_rank(id: &str) -> Option<u16> {
    let model = id.split_once('/').map_or(id, |(_, model)| model);
    codex_family_rank(model).or_else(|| kiro_family_rank(model))
}

fn effort_rank(id: &str) -> u16 {
    let mut rank: u16 = 10;
    if id.contains("medium") {
        rank = rank.max(20);
    }
    if id.contains("high") {
        rank = rank.max(40);
    }
    if id.contains("xhigh") {
        rank = rank.max(50);
    }
    if id.contains("pro") {
        rank = rank.max(55);
    }
    if id.contains("max") {
        rank = rank.max(60);
    }
    if id.contains("thinking") {
        rank += 5;
    }
    if id.contains("codex") {
        rank += 3;
    }
    rank
}

fn highlight_score(provider: Provider, major: u16, minor: u16, family: u16, effort: u16) -> u32 {
    let version_score = u32::from(major) * 100_000 + u32::from(minor) * 1_000;
    match provider {
        Provider::Kiro if family >= 85 => {
            10_000_000 + version_score + u32::from(family) * 10 + u32::from(effort)
        }
        Provider::Kiro => u32::from(family) * 1_000 + u32::from(major) * 10 + u32::from(minor),
        Provider::Codex | Provider::Grok | Provider::Vercel => {
            version_score + u32::from(family) * 10 + u32::from(effort)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ModelRank {
    score: u32,
    major: u16,
    minor: u16,
    family: u16,
    effort: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_openclaw_codex_models() {
        let ids = resolve_model_ids();
        let expected = OPENCLAW_CODEX_MODELS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert_eq!(ids, expected);
    }

    #[test]
    fn defaults_include_current_codex_models() {
        let ids = resolve_model_ids();

        assert!(ids.iter().any(|id| id == "gpt-6-astra"));
        assert!(ids.iter().any(|id| id == "gpt-5.6-sol"));
        assert!(ids.iter().any(|id| id == "gpt-5.6-terra"));
        assert!(ids.iter().any(|id| id == "gpt-5.6-luna"));
        assert!(!ids.iter().any(|id| id == "gpt-5.6"));
        assert!(!ids.iter().any(|id| id == "gpt-5.6-mini"));
    }

    #[test]
    fn grok_defaults_include_grok_models() {
        let ids = resolve_model_ids_for_provider(Provider::Grok);

        assert_eq!(
            ids,
            vec![
                "grok-4.6".to_owned(),
                "grok-4.5".to_owned(),
                "grok-4.3".to_owned(),
                "grok-build-0.1".to_owned(),
            ]
        );
        assert!(!ids.iter().any(|id| id == "gpt-5.6-sol"));
    }

    #[test]
    fn highlights_grok_models_by_version() {
        let ids = resolve_model_ids_for_provider(Provider::Grok);

        assert_eq!(
            highlight_model_ids_for_provider(Provider::Grok, &ids),
            vec![
                "grok-4.6".to_owned(),
                "grok-4.5".to_owned(),
                "grok-4.3".to_owned(),
                "grok-build-0.1".to_owned(),
            ]
        );
    }

    #[test]
    fn kiro_defaults_match_cli_models() {
        let ids = resolve_model_ids_for_provider(Provider::Kiro);

        assert!(ids.iter().any(|id| id == "auto"));
        assert!(ids.iter().any(|id| id == "claude-opus-4.8"));
        assert!(ids.iter().any(|id| id == "claude-sonnet-4.5"));
        assert!(ids.iter().any(|id| id == "qwen3-coder-next"));
    }

    #[test]
    fn vercel_defaults_include_ai_gateway_models() {
        let ids = resolve_model_ids_for_provider(Provider::Vercel);

        assert_eq!(
            ids,
            vec![
                "openai/gpt-6-astra".to_owned(),
                "anthropic/claude-sonnet-5".to_owned(),
                "anthropic/claude-opus-5".to_owned(),
            ]
        );
    }

    #[test]
    fn highlights_codex_models_by_version_and_filters_light_variants() {
        let ids = resolve_model_ids_for_provider(Provider::Codex);

        assert_eq!(
            highlight_model_ids_for_provider(Provider::Codex, &ids),
            vec![
                "gpt-6-astra".to_owned(),
                "gpt-5.6-sol".to_owned(),
                "gpt-5.6-terra".to_owned(),
                "gpt-5.6-luna".to_owned(),
            ]
        );
    }

    #[test]
    fn highlights_kiro_models_by_version_and_family() {
        let ids = [
            "auto",
            "claude-fable-5",
            "claude-opus-4.8",
            "claude-opus-4.7",
            "claude-opus-4.6",
            "claude-sonnet-4.6",
            "qwen3-coder-next",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

        assert_eq!(
            highlight_model_ids_for_provider(Provider::Kiro, &ids),
            vec![
                "claude-fable-5".to_owned(),
                "claude-opus-4.8".to_owned(),
                "claude-opus-4.7".to_owned(),
                "claude-opus-4.6".to_owned(),
            ]
        );
    }

    #[test]
    fn highlights_vercel_gateway_models_by_underlying_family() {
        let ids = resolve_model_ids_for_provider(Provider::Vercel);

        assert_eq!(
            highlight_model_ids_for_provider(Provider::Vercel, &ids),
            vec![
                "openai/gpt-6-astra".to_owned(),
                "anthropic/claude-opus-5".to_owned(),
                "anthropic/claude-sonnet-5".to_owned(),
            ]
        );
    }

    #[test]
    fn parses_decimal_and_dash_separated_model_versions() {
        assert_eq!(strongest_version("gpt-5.3-codex-xhigh"), Some((5, 3)));
        assert_eq!(
            strongest_version("claude-opus-4-8-thinking-max"),
            Some((4, 8))
        );
    }

    #[test]
    fn effort_rank_prefers_max_then_xhigh_then_high() {
        assert!(
            effort_rank("claude-opus-4-8-thinking-max")
                > effort_rank("claude-opus-4-8-thinking-xhigh")
        );
        assert!(
            effort_rank("claude-opus-4-8-thinking-xhigh")
                > effort_rank("claude-opus-4-8-thinking-high")
        );
    }

    #[test]
    fn provider_detection_handles_prefixed_grok_models() {
        assert_eq!(provider_for_model("grok-4.6"), Some(Provider::Grok));
        assert_eq!(provider_for_model("xai/grok-4.6"), Some(Provider::Grok));
        assert_eq!(provider_for_model("grok/grok-4.6"), Some(Provider::Grok));
        assert_eq!(
            provider_for_model("openai-codex/grok-4.3"),
            Some(Provider::Grok)
        );
        assert_eq!(
            provider_for_model("openai-codex/gpt-5.6-luna"),
            Some(Provider::Codex)
        );
        assert_eq!(provider_for_model("gpt-6-astra"), Some(Provider::Codex));
        assert_eq!(provider_for_model("kiro/auto"), Some(Provider::Kiro));
        assert_eq!(
            provider_for_model("claude-sonnet-4.5"),
            Some(Provider::Kiro)
        );
    }

    #[test]
    fn provider_detection_handles_vercel_gateway_models() {
        assert_eq!(
            provider_for_model("openai/gpt-6-astra"),
            Some(Provider::Vercel)
        );
        assert_eq!(
            provider_for_model("anthropic/claude-sonnet-5"),
            Some(Provider::Vercel)
        );
        assert_eq!(
            provider_for_model("vercel/xai/grok-4.6"),
            Some(Provider::Vercel)
        );
        assert_eq!(provider_for_model("xai/grok-4.6"), Some(Provider::Grok));
    }

    #[test]
    fn removed_cursor_models_do_not_route_to_another_provider() {
        assert_eq!(provider_for_model("cursor/gpt-5"), None);
        assert_eq!(provider_for_model("sonnet-4"), None);
        assert_eq!(provider_for_model("opus"), None);
    }

    #[test]
    fn model_list_owners_match_provider() {
        let models =
            resolve_model_list_for_providers(&[Provider::Codex, Provider::Grok, Provider::Kiro])
                .unwrap();

        let owner_for = |id: &str| {
            models
                .data
                .iter()
                .find(|model| model.id == id)
                .map(|model| model.owned_by)
        };

        assert_eq!(owner_for("gpt-6-astra"), Some("openai-codex"));
        assert_eq!(owner_for("grok-4.6"), Some("xai"));
        assert_eq!(owner_for("auto"), Some("kiro"));
        let models = resolve_model_list_for_provider(Provider::Vercel).unwrap();
        assert_eq!(models.data[0].owned_by, "vercel-ai-gateway");
    }
}
