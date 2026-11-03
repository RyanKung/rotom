//! Code Assist transport and lossless signed-tool replay for subscription accounts.
pub(super) mod models;
mod request;
mod schema;
mod stream;
#[cfg(test)]
mod tests;

use crate::{Error, Result, codex::sse::JsonSseEvent, config::Credentials};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::{collections::VecDeque, pin::Pin, sync::Arc};
use tokio::sync::Mutex;

pub(super) const BASE_URL: &str = crate::oauth::gemini::GENERATION_URL;
const REPLAY_CAPACITY: usize = 4096;

/// Clones share one bounded replay cache; signatures never cross project identities.
#[derive(Clone, Default)]
pub(super) struct ReplayCache(Arc<Mutex<VecDeque<(String, String, Value)>>>);

impl ReplayCache {
    async fn insert(&self, project: &str, part: Value) -> String {
        let id = format!("gemini_{:032x}", rand::random::<u128>());
        let mut entries = self.0.lock().await;
        if entries.len() >= REPLAY_CAPACITY {
            entries.pop_front();
        }
        entries.push_back((project.into(), id.clone(), part));
        id
    }

    async fn get(&self, project: &str, id: &str) -> Option<Value> {
        self.0
            .lock()
            .await
            .iter()
            .find(|(p, key, _)| p == project && key == id)
            .map(|(_, _, part)| part.clone())
    }
}

pub(super) async fn send(
    http: &reqwest::Client,
    base_url: &str,
    body: &Value,
    credentials: &Credentials,
    replay: &ReplayCache,
) -> Result<reqwest::Response> {
    let project = crate::oauth::GeminiOAuthClient::project_id(credentials)?;
    if project.is_empty() {
        return Err(Error::config(
            "Gemini project missing; run rotom login --provider gemini",
        ));
    }
    let payload = request::payload(body, project, replay).await?;
    let response = http
        .post(format!(
            "{}:streamGenerateContent?alt=sse",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(&credentials.access_token)
        .header("User-Agent", crate::oauth::gemini::user_agent())
        .json(&payload)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::Upstream {
            status: response.status(),
            message: format!(
                "Gemini via Antigravity returned {}; check account eligibility, model access, or quota",
                response.status()
            ),
        });
    }
    Ok(response)
}

pub(super) fn events(
    response: reqwest::Response,
    body: Value,
    project: String,
    replay: ReplayCache,
) -> Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>> {
    stream::events(Box::pin(response.bytes_stream()), body, project, replay)
}

pub(super) async fn collect(
    mut events: Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>>,
) -> Result<Value> {
    while let Some(event) = events.next().await {
        let value = event?.value;
        if crate::codex::events::is_done_event(&value) {
            return value
                .get("response")
                .cloned()
                .ok_or_else(|| Error::upstream("missing Gemini response envelope"));
        }
    }
    Err(Error::upstream("Gemini stream ended before completion"))
}
