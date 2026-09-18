//! Typed request structures for Rotom's evaluation endpoint.
//!
//! Evaluation models answer narrow typed questions about shared state. The
//! public request keeps a `model` selector for Rotom routing, while Vercel AI
//! Gateway receives that model id through a header and only receives the state,
//! questions, and optional provider options in the JSON body.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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
}
