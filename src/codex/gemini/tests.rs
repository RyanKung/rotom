use super::*;
use crate::codex::client::CodexClient;
use crate::config::{Provider, now_unix};
use bytes::Bytes;
use serde_json::json;
use std::fmt::Write;

fn test_events(
    chunks: &[Value],
    cache: ReplayCache,
) -> Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>> {
    let data = chunks.iter().fold(String::new(), |mut data, v| {
        write!(data, "data: {v}\r\n\r\n").unwrap();
        data
    });
    // Split every byte, including multi-byte UTF-8, to exercise the SSE decoder.
    let bytes = data
        .into_bytes()
        .into_iter()
        .map(|b| Ok(Bytes::from(vec![b])))
        .collect::<Vec<_>>();
    stream::events(
        Box::pin(futures_util::stream::iter(bytes)),
        json!({"model":"gemini-2.5-pro"}),
        "project".into(),
        cache,
    )
}

#[tokio::test]
async fn signed_tool_calls_roundtrip_through_opaque_short_ids() {
    let cache = ReplayCache::default();
    let part = json!({"functionCall":{"name":"weather","args":{"city":"上海"}},"thoughtSignature":"opaque-signature"});
    let response = collect(test_events(
        &[
            json!({"response":{"candidates":[{"content":{"parts":[part]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":4}}}),
        ],
        cache.clone(),
    ))
    .await
    .unwrap();
    let call = response["output"][0].clone();
    let id = call["call_id"].as_str().unwrap();
    assert!(id.len() < 64);
    assert_eq!(response["usage"]["output_tokens"], 6);
    let input = json!({"model":"gemini/gemini-2.5-pro","input":[call,{"type":"function_call_output","call_id":id,"output":"sunny"}]});
    let payload = request::payload(&input, "project", &cache).await.unwrap();
    assert_eq!(payload["request"]["contents"][0]["parts"][0], part);
    assert_eq!(
        payload["request"]["contents"][1]["parts"][0]["functionResponse"]["name"],
        "weather"
    );
    assert!(
        request::payload(&input, "another-project", &cache)
            .await
            .is_err()
    );
    assert!(
        request::payload(&input, "project", &ReplayCache::default())
            .await
            .is_err()
    );
    let mut changed = input;
    changed["input"][0]["arguments"] = json!("{\"city\":\"Beijing\"}");
    assert!(request::payload(&changed, "project", &cache).await.is_err());
}

#[tokio::test]
async fn text_stream_counts_usage_and_rejects_truncation_or_safety_failure() {
    let first = json!({"response":{"candidates":[{"content":{"parts":[{"text":"你好"}]}}]}});
    let last = json!({"response":{"candidates":[{"content":{"parts":[{"text":"世界"}]},"finishReason":"MAX_TOKENS"}]}});
    let response = collect(test_events(&[first.clone(), last], ReplayCache::default()))
        .await
        .unwrap();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(response["output"][0]["content"][0]["text"], "你好世界");
    assert!(
        collect(test_events(&[first], ReplayCache::default()))
            .await
            .is_err()
    );
    for chunk in [
        json!({"error":{"message":"denied"}}),
        json!({"response":{"promptFeedback":{"blockReason":"SAFETY"}}}),
        json!({"response":{"candidates":[{"finishReason":"SAFETY"}]}}),
    ] {
        assert!(
            collect(test_events(&[chunk], ReplayCache::default()))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn request_preserves_images_tools_limits_and_system_instructions() {
    let payload = request::payload(&json!({"model":"gemini-2.5-flash","instructions":"be helpful","max_output_tokens":123,
        "temperature":0.4,"tool_choice":{"type":"function","name":"lookup"},
        "tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}],
        "input":[{"role":"user","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"data:image/png;base64,YQ=="}]}]}),"project",&ReplayCache::default()).await.unwrap();
    assert_eq!(
        payload["request"]["systemInstruction"]["parts"][0]["text"],
        "be helpful"
    );
    assert_eq!(
        payload["request"]["generationConfig"]["maxOutputTokens"],
        123
    );
    assert_eq!(
        payload["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
    assert_eq!(
        payload["request"]["contents"][0]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
}

#[tokio::test]
async fn native_responses_chat_and_streaming_share_subscription_transport() {
    use axum::{Json, Router, routing::post};
    async fn handler(
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        assert_eq!(headers.get("authorization").unwrap(), "Bearer access");
        assert_eq!(body["project"], "project");
        assert_eq!(body["model"], "gemini-2.5-pro");
        (
            [("content-type", "text/event-stream")],
            format!(
                "data: {}\n\n",
                json!({"response":{"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}})
            ),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1internal", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1internal:streamGenerateContent", post(handler)),
        )
        .await
        .unwrap();
    });
    let client =
        CodexClient::new_for_provider_base_url(reqwest::Client::new(), Provider::Gemini, base);
    let credentials = Credentials {
        provider: Provider::Gemini,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: now_unix() + 3600,
        account_id: "antigravity:project".into(),
    };
    let native = client
        .complete_response(json!({"model":"gemini-2.5-pro","input":"Hi"}), &credentials)
        .await
        .unwrap();
    assert_eq!(native["output"][0]["content"][0]["text"], "hello");
    let chat = serde_json::from_value(
        json!({"model":"gemini/gemini-2.5-pro","messages":[{"role":"user","content":"Hi"}]}),
    )
    .unwrap();
    let result = client.complete_chat(chat, &credentials).await.unwrap();
    assert_eq!(result.choices[0].message.content, "hello");
    let chat = serde_json::from_value(
        json!({"model":"gemini-2.5-pro","messages":[{"role":"user","content":"Hi"}],"stream":true}),
    )
    .unwrap();
    let mut stream = client.stream_chat(chat, &credentials).await.unwrap();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(serde_json::to_value(chunk.unwrap()).unwrap());
    }
    assert!(
        chunks
            .iter()
            .any(|v| v.pointer("/choices/0/delta/content") == Some(&json!("hello")))
    );
    task.abort();
}

#[tokio::test]
async fn antigravity_envelope_preserves_session_and_uses_unique_request_ids() {
    let cache = ReplayCache::default();
    let first = json!({"model":"gemini/gemini-3-flash-preview","input":[{"role":"user","content":"hello"}]});
    let second = json!({"model":"gemini-3-flash-preview","input":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"continue"}]});
    let a = request::payload(&first, "project", &cache).await.unwrap();
    let b = request::payload(&second, "project", &cache).await.unwrap();
    assert_eq!(a["userAgent"], "antigravity");
    assert_eq!(a["requestType"], "agent");
    assert_eq!(a["model"], "gemini-3-flash-preview");
    assert_eq!(a["request"]["sessionId"], b["request"]["sessionId"]);
    assert_ne!(a["requestId"], b["requestId"]);
}

#[tokio::test]
async fn chat_and_responses_formats_preserve_strict_schema_constraints() {
    let schema = json!({"type":"object","properties":{
        "items":{"type":"array","items":{"$ref":"#/$defs/entry"}},
        "optional":{"type":["string","null"]}},
        "required":["items","optional"],"additionalProperties":false,
        "$defs":{"entry":{"type":"object","properties":{"code":{"type":"string","enum":["ONLY"]}},
        "required":["code"],"additionalProperties":false}}});
    for strict in [true, false] {
        let chat = serde_json::from_value(json!({"model":"gemini-3.8-flash-low",
            "messages":[{"role":"user","content":"Return the structured result."}],
            "response_format":{"type":"json_schema","json_schema":{
                "name":"probe","strict":strict,"schema":schema}}}))
        .unwrap();
        let normalized = crate::codex::convert::to_codex_request(&chat).unwrap();
        let direct = json!({"model":"gemini-3.8-flash-low","input":"Return the structured result.",
            "text":{"format":{"type":"json_schema","name":"probe","strict":strict,"schema":schema}}});
        for body in [normalized, direct] {
            let payload = request::payload(&body, "project", &ReplayCache::default())
                .await
                .unwrap();
            assert_eq!(
                payload["request"]["generationConfig"]["responseMimeType"],
                "application/json"
            );
            assert_eq!(
                payload["request"]["generationConfig"]["responseSchema"],
                super::schema::convert(&schema).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn json_mode_and_plain_text_do_not_invent_schema_constraints() {
    for format in [
        Value::Null,
        json!({"type":"text"}),
        json!({"type":"json_object"}),
    ] {
        let body = json!({"model":"gemini-3.8-flash-low","input":"hello",
            "temperature":0.2,"text":{"format":format}});
        let payload = request::payload(&body, "project", &ReplayCache::default())
            .await
            .unwrap();
        let config = &payload["request"]["generationConfig"];
        assert_eq!(config["temperature"], 0.2);
        assert!(config.get("responseSchema").is_none());
        assert_eq!(
            config.get("responseMimeType").is_some(),
            format["type"] == "json_object"
        );
    }
}

#[tokio::test]
async fn invalid_response_formats_fail_instead_of_silently_weakening_constraints() {
    for format in [
        json!({"type":"unknown"}),
        json!({}),
        json!({"type":"json_schema"}),
        json!({"type":"json_schema","schema":null}),
        json!({"type":"json_schema","schema":{},"strict":"true"}),
    ] {
        let body = json!({"model":"gemini-3.8-flash-low","input":"hello","text":{"format":format}});
        let error = request::payload(&body, "project", &ReplayCache::default())
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), reqwest::StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn strict_stream_never_releases_invalid_json_or_constraint_violations() {
    let schema = json!({"type":"object","properties":{"n":{"type":"integer","minimum":7,"maximum":7}},
        "required":["n"],"additionalProperties":false});
    for (text, finish, valid) in [
        ("{\"n\":7}", "STOP", true),
        ("{\"n\":8}", "STOP", false),
        ("{\"n\":7,\"extra\":true}", "STOP", false),
        ("{}", "STOP", false),
        ("not JSON", "STOP", false),
        ("{\"n\":7}", "MAX_TOKENS", false),
    ] {
        let chunk = json!({"response":{"candidates":[{"content":{"parts":[{"text":text}]},"finishReason":finish}]}});
        let bytes = Bytes::from(format!("data: {chunk}\n\n"));
        let mut events = stream::events(
            Box::pin(futures_util::stream::iter([Ok(bytes)])),
            json!({"model":"gemini-3.8-flash-low","text":{"format":{"type":"json_schema","strict":true,"schema":schema}}}),
            "project".into(),
            ReplayCache::default(),
        );
        let mut released_text = false;
        let mut completed = false;
        let mut failed = false;
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    released_text |= event.value["type"] == "response.output_text.delta";
                    completed |= event.value["type"] == "response.completed";
                }
                Err(_) => failed = true,
            }
        }
        assert_eq!(
            (released_text, completed, failed),
            (valid, valid, !valid),
            "{text}"
        );
    }
}
