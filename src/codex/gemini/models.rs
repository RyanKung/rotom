//! Live, account-scoped Gemini model discovery through Antigravity.
use crate::{
    Error, Result, config::Credentials, oauth::GeminiOAuthClient, openai::response::ModelList,
};
use serde_json::{Value, json};
use std::time::Duration;

pub(in crate::codex) async fn discover(
    http: &reqwest::Client,
    base: &str,
    credentials: &Credentials,
) -> Result<ModelList> {
    let project = GeminiOAuthClient::project_id(credentials)?;
    if project.is_empty() {
        return Err(Error::config("Antigravity project setup is pending"));
    }
    let load_base = if base == super::BASE_URL {
        "https://cloudcode-pa.googleapis.com/v1internal"
    } else {
        base
    };
    let load = post(
        http,
        load_base,
        "loadCodeAssist",
        credentials,
        &json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
    )
    .await?;
    if !load.status().is_success() {
        return Err(Error::upstream_with_status(
            load.status(),
            "Antigravity account lookup failed during model discovery",
        ));
    }
    let loaded: Value = load.json().await?;
    let mut tiers = Vec::new();
    for field in ["paidTier", "currentTier"] {
        if let Some(tier) = loaded
            .get(field)
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if !tiers.contains(&tier) {
                tiers.push(tier);
            }
        }
    }
    tiers.push("");
    for tier in tiers {
        let body = if tier.is_empty() {
            json!({"project":project})
        } else {
            json!({"project":project,"entitlement":{"userTier":tier}})
        };
        let response = post(http, base, "fetchAvailableModels", credentials, &body).await?;
        if !tier.is_empty() && response.status() == reqwest::StatusCode::BAD_REQUEST {
            continue;
        }
        if !response.status().is_success() {
            return Err(Error::upstream_with_status(
                response.status(),
                "Antigravity model discovery failed",
            ));
        }
        return parse_models(&response.json::<Value>().await?);
    }
    Err(Error::upstream("Antigravity returned no model catalog"))
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    method: &str,
    credentials: &Credentials,
    body: &Value,
) -> Result<reqwest::Response> {
    Ok(http
        .post(format!("{}:{method}", base.trim_end_matches('/')))
        .timeout(Duration::from_secs(30))
        .bearer_auth(&credentials.access_token)
        .header("User-Agent", crate::oauth::gemini::user_agent())
        .json(body)
        .send()
        .await?)
}

fn parse_models(value: &Value) -> Result<ModelList> {
    let models = value
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::upstream("Antigravity returned an invalid model catalog"))?;
    // This provider exposes Gemini; other vendors need separate routing/capabilities.
    let ids = models
        .iter()
        .filter(|(id, model)| {
            id.starts_with("gemini-")
                && model.get("disabled").and_then(Value::as_bool) != Some(true)
        })
        .map(|(id, _)| (id.clone(), "gemini"))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(Error::upstream(
            "Antigravity offered this account no enabled Gemini models",
        ));
    }
    Ok(ModelList::from_id_owners(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_keeps_upstream_names_and_excludes_disabled_or_other_vendors() {
        let models=parse_models(&json!({"models":{"gemini-new-high":{},"gemini-disabled":{"disabled":true},"claude-other":{}}})).unwrap();
        assert_eq!(models.data.len(), 1);
        assert_eq!(models.data[0].id, "gemini-new-high");
        assert!(parse_models(&json!({"models":{}})).is_err());
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn discovery_tries_paid_then_current_tier_and_keeps_native_ids() {
        async fn load() -> Json<Value> {
            Json(json!({"paidTier":{"id":"paid"},"currentTier":{"id":"free"}}))
        }
        async fn models(
            State(seen): State<Arc<Mutex<Vec<Value>>>>,
            Json(body): Json<Value>,
        ) -> (reqwest::StatusCode, Json<Value>) {
            seen.lock().await.push(body.clone());
            assert_eq!(body["project"], "project");
            if body["entitlement"]["userTier"] == "paid" {
                return (reqwest::StatusCode::BAD_REQUEST, Json(json!({})));
            }
            (
                reqwest::StatusCode::OK,
                Json(json!({"models":{"gemini-live-high":{},"gemini-disabled":{"disabled":true}}})),
            )
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1internal", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1internal:loadCodeAssist", post(load))
            .route("/v1internal:fetchAvailableModels", post(models))
            .with_state(seen.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let credentials = Credentials {
            provider: crate::config::Provider::Gemini,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: crate::config::now_unix() + 600,
            account_id: "antigravity:project".into(),
        };
        let list = discover(&reqwest::Client::new(), &base, &credentials)
            .await
            .unwrap();
        assert_eq!(list.data.len(), 1);
        assert_eq!(list.data[0].id, "gemini-live-high");
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1]["entitlement"]["userTier"], "free");
        drop(seen);
        server.abort();
    }
}
