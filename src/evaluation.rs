//! Typed request structures and upstream protocol helpers for Rotom's evaluation endpoint.
//!
//! Evaluation models answer narrow typed questions about shared state. The
//! public request keeps a `model` selector for Rotom routing, while Vercel AI
//! Gateway receives that model id through a header and only receives the state,
//! questions, and optional provider options in the JSON body.

use crate::{Error, Result, config::Credentials};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Root URL for hosted Vercel AI Gateway traffic.
const VERCEL_AI_GATEWAY_ROOT_URL: &str = "https://ai-gateway.vercel.sh";
/// Path segment for the OpenAI-compatible Vercel Gateway API.
const VERCEL_AI_GATEWAY_OPENAI_PATH: &str = "/v1";
/// Path segment for Vercel's AI SDK provider protocol.
const VERCEL_AI_GATEWAY_PROVIDER_PATH: &str = "/v4/ai";
/// Terminal route segment for Vercel evaluation models.
const VERCEL_EVALUATION_MODEL_ROUTE: &str = "evaluation-model";
/// Wire-protocol version expected by Vercel AI Gateway.
const VERCEL_GATEWAY_PROTOCOL_VERSION: &str = "0.0.1";
/// Evaluation-model specification version expected by Vercel AI Gateway.
const VERCEL_EVALUATION_MODEL_SPECIFICATION_VERSION: &str = "4";

/// Default Vercel AI Gateway base URL for AI SDK provider protocol requests.
pub const DEFAULT_VERCEL_AI_GATEWAY_EVALUATION_BASE_URL: &str =
    "https://ai-gateway.vercel.sh/v4/ai";

/// Request body accepted by `POST /v1/evaluations`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvaluationRequest {
    /// Evaluation model identifier, such as `typesafe-ai/jev`.
    pub model: String,
    /// Shared state every question evaluates.
    pub state: Value,
    /// Named typed questions evaluated independently against the shared state.
    pub questions: BTreeMap<String, EvaluationQuestion>,
    /// Optional provider-specific controls forwarded to the Gateway evaluation model.
    #[serde(
        default,
        rename = "providerOptions",
        alias = "provider_options",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_options: Option<Value>,
}

impl EvaluationRequest {
    /// Returns the model id Vercel Gateway expects in the `ai-model-id` header.
    #[must_use]
    pub fn vercel_model_id(&self) -> &str {
        strip_vercel_model_prefix(&self.model)
    }

    /// Builds the upstream Gateway JSON body without the Rotom routing `model`.
    #[must_use]
    pub(crate) const fn gateway_body(&self) -> GatewayEvaluationBody<'_> {
        GatewayEvaluationBody {
            state: &self.state,
            questions: &self.questions,
            provider_options: self.provider_options.as_ref(),
        }
    }
}

/// Resolves a configured Vercel AI Gateway base URL into the evaluation endpoint.
///
/// Rotom's user-facing Vercel base URL is normally the OpenAI-compatible
/// Gateway `/v1` surface. Evaluation models use Vercel's AI SDK provider
/// protocol instead, so the default Gateway root is mapped to `/v4/ai`.
#[must_use]
pub fn resolve_vercel_evaluation_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with(VERCEL_EVALUATION_MODEL_ROUTE) {
        return normalized.to_owned();
    }
    if normalized.ends_with(VERCEL_AI_GATEWAY_PROVIDER_PATH) {
        return format!("{normalized}/{VERCEL_EVALUATION_MODEL_ROUTE}");
    }
    if let Some(root) = normalized
        .strip_suffix("/v1/responses")
        .or_else(|| normalized.strip_suffix("/v1/models"))
        .or_else(|| normalized.strip_suffix(VERCEL_AI_GATEWAY_OPENAI_PATH))
    {
        return format!("{root}{VERCEL_AI_GATEWAY_PROVIDER_PATH}/{VERCEL_EVALUATION_MODEL_ROUTE}");
    }
    if normalized == VERCEL_AI_GATEWAY_ROOT_URL {
        return format!(
            "{DEFAULT_VERCEL_AI_GATEWAY_EVALUATION_BASE_URL}/{VERCEL_EVALUATION_MODEL_ROUTE}"
        );
    }
    format!("{normalized}/{VERCEL_EVALUATION_MODEL_ROUTE}")
}

/// Builds HTTP headers for authenticated Vercel AI Gateway evaluation requests.
///
/// # Errors
///
/// Returns an error when the bearer token or model id cannot be represented as
/// a header.
pub(crate) fn vercel_evaluation_headers(
    credentials: &Credentials,
    model_id: &str,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("rotom"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        HeaderName::from_static("ai-gateway-protocol-version"),
        HeaderValue::from_static(VERCEL_GATEWAY_PROTOCOL_VERSION),
    );
    headers.insert(
        HeaderName::from_static("ai-gateway-auth-method"),
        HeaderValue::from_static("api-key"),
    );
    headers.insert(
        HeaderName::from_static("ai-evaluation-model-specification-version"),
        HeaderValue::from_static(VERCEL_EVALUATION_MODEL_SPECIFICATION_VERSION),
    );
    headers.insert(
        HeaderName::from_static("ai-model-id"),
        header_value(model_id)?,
    );
    Ok(headers)
}

/// Single typed question for an evaluation model.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EvaluationQuestion {
    /// Yes/no probability question.
    Boolean {
        /// Natural-language or structured instruction describing the yes/no condition.
        instructions: Value,
        /// Optional true/false criteria that define the probability endpoints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<Value>,
    },
    /// Closed-set selection question.
    Choice {
        /// Natural-language or structured instruction describing the choice.
        instructions: Value,
        /// Option descriptions keyed by the possible choices.
        criteria: Value,
    },
    /// Ordered-rubric scoring question.
    Score {
        /// Natural-language or structured instruction describing the score.
        instructions: Value,
        /// Ordered rubric levels, from lowest to highest.
        criteria: Value,
    },
}

/// Vercel Gateway evaluation body sent after Rotom removes routing-only fields.
#[derive(Debug, Serialize)]
pub(crate) struct GatewayEvaluationBody<'a> {
    /// Shared state every question evaluates.
    state: &'a Value,
    /// Named typed questions evaluated independently against the shared state.
    questions: &'a BTreeMap<String, EvaluationQuestion>,
    /// Optional provider-specific controls forwarded to the Gateway evaluation model.
    #[serde(rename = "providerOptions", skip_serializing_if = "Option::is_none")]
    provider_options: Option<&'a Value>,
}

/// Strips Rotom's explicit Vercel provider prefix from a model id.
#[must_use]
pub fn strip_vercel_model_prefix(model: &str) -> &str {
    model
        .strip_prefix("vercel/")
        .filter(|stripped| !stripped.is_empty())
        .unwrap_or(model)
}

/// Converts a dynamic string into a safe HTTP header value.
fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_explicit_vercel_model_prefix() {
        assert_eq!(
            strip_vercel_model_prefix("vercel/typesafe-ai/jev"),
            "typesafe-ai/jev"
        );
        assert_eq!(
            strip_vercel_model_prefix("typesafe-ai/jev"),
            "typesafe-ai/jev"
        );
        assert_eq!(strip_vercel_model_prefix("vercel/"), "vercel/");
    }

    #[test]
    fn gateway_body_omits_routing_model() {
        let request = EvaluationRequest {
            model: "vercel/typesafe-ai/jev".to_owned(),
            state: json!({"message": "The build failed."}),
            questions: serde_json::from_value(json!({
                "passed": {
                    "type": "boolean",
                    "instructions": "Did the build pass?"
                }
            }))
            .unwrap(),
            provider_options: Some(json!({"gateway": {"tags": ["test"]}})),
        };

        let body = serde_json::to_value(request.gateway_body()).unwrap();

        assert!(body.get("model").is_none());
        assert_eq!(body["providerOptions"]["gateway"]["tags"][0], "test");
        assert_eq!(request.vercel_model_id(), "typesafe-ai/jev");
    }

    #[test]
    fn resolves_vercel_evaluation_gateway_urls() {
        assert_eq!(
            resolve_vercel_evaluation_url("https://ai-gateway.vercel.sh/v1"),
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model"
        );
        assert_eq!(
            resolve_vercel_evaluation_url("https://ai-gateway.vercel.sh/v1/responses"),
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model"
        );
        assert_eq!(
            resolve_vercel_evaluation_url("https://ai-gateway.vercel.sh/v4/ai"),
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model"
        );
        assert_eq!(
            resolve_vercel_evaluation_url("https://ai-gateway.vercel.sh/v4/ai/evaluation-model"),
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model"
        );
    }

    #[test]
    fn builds_vercel_evaluation_headers() {
        let credentials = Credentials {
            provider: crate::config::Provider::Vercel,
            access_token: "token".into(),
            refresh_token: String::new(),
            expires_at: 1,
            account_id: String::new(),
        };

        let headers = vercel_evaluation_headers(&credentials, "typesafe-ai/jev").unwrap();

        assert_eq!(headers["authorization"], "Bearer token");
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["ai-gateway-protocol-version"], "0.0.1");
        assert_eq!(headers["ai-gateway-auth-method"], "api-key");
        assert_eq!(headers["ai-evaluation-model-specification-version"], "4");
        assert_eq!(headers["ai-model-id"], "typesafe-ai/jev");
    }
}
