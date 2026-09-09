use crate::anthropic::{
    MessageBatchRequest, MessageBatchResult, MessageBatchResultType, error_body,
    from_openai_response_value,
};
use crate::codex::convert::responses_to_upstream_request;
use crate::server::handlers::responses::{
    anthropic_responses_request, collect_response_input_items,
};
use crate::{Error, models::provider_for_model, server::UpstreamState};
use axum::http::{HeaderMap, header::HOST};
use std::sync::Arc;

pub(in crate::server::handlers) fn build_batch_id() -> String {
    format!(
        "msgbatch_{}_{:08x}",
        crate::config::now_unix(),
        rand::random::<u32>()
    )
}

pub(in crate::server::handlers) fn batch_results_url(
    headers: &HeaderMap,
    batch_id: &str,
) -> String {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:14550");
    format!("http://{host}/v1/messages/batches/{batch_id}/results")
}

pub(in crate::server::handlers) async fn run_message_batch_worker(
    batches: crate::server::store::BatchStore,
    upstreams: Arc<Vec<UpstreamState>>,
    batch_id: String,
    requests: Vec<MessageBatchRequest>,
) {
    let mut requests = std::collections::VecDeque::from(requests);
    while let Some(item) = requests.pop_front() {
        if batches.cancel_requested(&batch_id).await.unwrap_or(false) {
            let mut canceled = vec![MessageBatchResult {
                custom_id: item.custom_id,
                result: MessageBatchResultType::Canceled,
            }];
            canceled.extend(requests.into_iter().map(|pending| MessageBatchResult {
                custom_id: pending.custom_id,
                result: MessageBatchResultType::Canceled,
            }));
            finalize_canceled_batch(&batches, &batch_id, canceled).await;
            return;
        }

        let result = run_message_batch_item(&upstreams, item).await;

        batches
            .update(&batch_id, move |stored| {
                stored.results.push(result.clone());
                match &result.result {
                    MessageBatchResultType::Succeeded { .. } => {
                        stored.batch.request_counts.succeeded =
                            stored.batch.request_counts.succeeded.saturating_add(1);
                    }
                    MessageBatchResultType::Errored { .. } => {
                        stored.batch.request_counts.errored =
                            stored.batch.request_counts.errored.saturating_add(1);
                    }
                    MessageBatchResultType::Canceled => {
                        stored.batch.request_counts.canceled =
                            stored.batch.request_counts.canceled.saturating_add(1);
                    }
                }
                stored.batch.request_counts.processing =
                    stored.batch.request_counts.processing.saturating_sub(1);
            })
            .await;
    }

    let _ = batches
        .update(&batch_id, |stored| {
            stored.batch.processing_status = "ended";
            stored.batch.ended_at = Some(chrono::Utc::now().to_rfc3339());
            stored.cancel_requested = false;
        })
        .await;
}

async fn run_message_batch_item(
    upstreams: &[UpstreamState],
    item: MessageBatchRequest,
) -> MessageBatchResult {
    let custom_id = item.custom_id;
    match anthropic_responses_request(&item.params).and_then(|response_request| {
        collect_response_input_items(&response_request, None)
            .map(|input_items| (response_request, input_items))
    }) {
        Ok((response_request, input_items)) => {
            run_message_batch_upstream(upstreams, custom_id, response_request, input_items).await
        }
        Err(error) => errored_batch_result(custom_id, &error),
    }
}

async fn run_message_batch_upstream(
    upstreams: &[UpstreamState],
    custom_id: String,
    response_request: crate::openai::types::ResponsesRequest,
    input_items: Vec<serde_json::Value>,
) -> MessageBatchResult {
    let Some(upstream) = upstream_for_model(upstreams, &response_request.model) else {
        return errored_batch_result(
            custom_id,
            &Error::config(format!(
                "not logged in for model {}",
                response_request.model
            )),
        );
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return errored_batch_result(custom_id, &error),
    };
    let body =
        match responses_to_upstream_request(upstream.provider, &response_request, &input_items) {
            Ok(body) => body,
            Err(error) => return errored_batch_result(custom_id, &error),
        };
    match upstream.client.complete_response(body, &credentials).await {
        Ok(response) => MessageBatchResult {
            custom_id,
            result: MessageBatchResultType::Succeeded {
                message: from_openai_response_value(&response, &response_request.model),
            },
        },
        Err(error) => errored_batch_result(custom_id, &error),
    }
}

fn errored_batch_result(custom_id: String, error: &crate::Error) -> MessageBatchResult {
    MessageBatchResult {
        custom_id,
        result: MessageBatchResultType::Errored {
            error: error_body(error),
        },
    }
}

fn upstream_for_model<'a>(
    upstreams: &'a [UpstreamState],
    model: &str,
) -> Option<&'a UpstreamState> {
    let provider = provider_for_model(model)?;
    upstreams
        .iter()
        .find(|upstream| upstream.provider == provider)
        .or_else(|| upstreams.first())
}

async fn finalize_canceled_batch(
    batches: &crate::server::store::BatchStore,
    batch_id: &str,
    canceled_results: Vec<MessageBatchResult>,
) {
    let remaining = u32::try_from(canceled_results.len()).unwrap_or(u32::MAX);
    let _ = batches
        .update(batch_id, move |stored| {
            stored.results.extend(canceled_results);
            stored.batch.request_counts.canceled = stored
                .batch
                .request_counts
                .canceled
                .saturating_add(remaining);
            stored.batch.request_counts.processing = stored
                .batch
                .request_counts
                .processing
                .saturating_sub(remaining);
            stored.batch.processing_status = "ended";
            stored.batch.ended_at = Some(chrono::Utc::now().to_rfc3339());
            stored.cancel_requested = false;
        })
        .await;
}
