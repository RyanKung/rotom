use super::*;
use crate::evaluation::EvaluationRequest;

/// Captured Vercel evaluation request data from the fake Gateway server.
#[derive(Default)]
struct CapturedEvaluation {
    /// JSON body sent by Rotom to the Gateway provider protocol endpoint.
    body: Option<Value>,
    /// Bearer authorization header forwarded to Gateway.
    authorization: Option<String>,
    /// Gateway model id header forwarded to Gateway.
    model_id: Option<String>,
    /// Evaluation specification version header forwarded to Gateway.
    specification_version: Option<String>,
    /// Gateway protocol version header forwarded to Gateway.
    protocol_version: Option<String>,
    /// Gateway authentication method header forwarded to Gateway.
    auth_method: Option<String>,
}

/// Extracts a UTF-8 header value from the request header map.
fn captured_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// Fake Gateway evaluation endpoint that records the incoming request.
async fn vercel_evaluation_handler(
    State(captured): State<Arc<Mutex<CapturedEvaluation>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    {
        let mut captured = captured.lock().await;
        captured.body = Some(body);
        captured.authorization = captured_header(&headers, "authorization");
        captured.model_id = captured_header(&headers, "ai-model-id");
        captured.specification_version =
            captured_header(&headers, "ai-evaluation-model-specification-version");
        captured.protocol_version = captured_header(&headers, "ai-gateway-protocol-version");
        captured.auth_method = captured_header(&headers, "ai-gateway-auth-method");
    }

    Json(json!({
        "answers": {
            "passed": {
                "type": "boolean",
                "probability": 0.01
            }
        },
        "usage": {
            "inputTokens": 42,
            "outputTokens": 3
        }
    }))
}

/// Starts a fake Vercel Gateway server for evaluation requests.
async fn spawn_vercel_evaluation_server(captured: Arc<Mutex<CapturedEvaluation>>) -> String {
    let app = Router::new()
        .route("/v4/ai/evaluation-model", post(vercel_evaluation_handler))
        .with_state(captured);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

/// Builds `AppState` with static Vercel Gateway credentials and a fake base URL.
fn vercel_evaluation_state(store: AuthStore, base_url: &str, api_key: &str) -> AppState {
    let http = Client::new();
    AppState::new_multi_with_model_fallback(
        vec![UpstreamState {
            provider: Provider::Vercel,
            token_manager: TokenManager::new_for_provider(store, Provider::Vercel, http.clone()),
            client: CodexClient::new_for_provider_base_url(
                http,
                Provider::Vercel,
                format!("{base_url}/v1"),
            ),
        }],
        Some(api_key.to_owned()),
        ModelList::from_ids(["typesafe-ai/jev"]),
        None,
    )
}

/// Saves static Vercel Gateway credentials for the evaluation tests.
fn save_vercel_credentials(store: &AuthStore) {
    store
        .save(&Credentials {
            provider: Provider::Vercel,
            access_token: "gateway-key".into(),
            refresh_token: String::new(),
            expires_at: now_unix() + 600,
            account_id: String::new(),
        })
        .unwrap();
}

/// Parses a test JSON value into an `EvaluationRequest`.
fn evaluation_request(value: Value) -> EvaluationRequest {
    serde_json::from_value(value).unwrap()
}

/// Rotom forwards evaluation requests to Vercel's provider protocol endpoint.
#[tokio::test]
async fn evaluations_forward_to_vercel_gateway_provider_protocol() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    save_vercel_credentials(&store);
    let captured = Arc::new(Mutex::new(CapturedEvaluation::default()));
    let gateway_base_url = spawn_vercel_evaluation_server(captured.clone()).await;
    let state = vercel_evaluation_state(store, &gateway_base_url, "secret");
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    let request = evaluation_request(json!({
        "model": "vercel/typesafe-ai/jev",
        "state": {
            "build": {
                "exit_code": 1
            }
        },
        "questions": {
            "passed": {
                "type": "boolean",
                "instructions": "Did the build pass?",
                "criteria": {
                    "true": "exit code 0",
                    "false": "non-zero exit code"
                }
            }
        },
        "providerOptions": {
            "gateway": {
                "tags": ["rotom-test"]
            }
        }
    }));

    let response = handlers::evaluations(axum::extract::State(state), headers, Json(request)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["answers"]["passed"]["probability"], 0.01);
    assert_eq!(value["usage"]["inputTokens"], 42);

    let (
        upstream_body,
        authorization,
        model_id,
        specification_version,
        protocol_version,
        auth_method,
    ) = {
        let captured = captured.lock().await;
        (
            captured.body.as_ref().unwrap().clone(),
            captured.authorization.clone(),
            captured.model_id.clone(),
            captured.specification_version.clone(),
            captured.protocol_version.clone(),
            captured.auth_method.clone(),
        )
    };
    assert!(upstream_body.get("model").is_none());
    assert_eq!(upstream_body["state"]["build"]["exit_code"], 1);
    assert_eq!(
        upstream_body["questions"]["passed"]["criteria"]["false"],
        "non-zero exit code"
    );
    assert_eq!(
        upstream_body["providerOptions"]["gateway"]["tags"][0],
        "rotom-test"
    );
    assert_eq!(authorization.as_deref(), Some("Bearer gateway-key"));
    assert_eq!(model_id.as_deref(), Some("typesafe-ai/jev"));
    assert_eq!(specification_version.as_deref(), Some("4"));
    assert_eq!(protocol_version.as_deref(), Some("0.0.1"));
    assert_eq!(auth_method.as_deref(), Some("api-key"));
}

/// Non-Gateway models are rejected before Rotom calls an evaluation upstream.
#[tokio::test]
async fn evaluations_reject_non_vercel_models() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    let request = evaluation_request(json!({
        "model": "gpt-test",
        "state": "The build failed.",
        "questions": {
            "passed": {
                "type": "boolean",
                "instructions": "Did the build pass?"
            }
        }
    }));

    let response = handlers::evaluations(axum::extract::State(state), headers, Json(request)).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requires a Vercel AI Gateway upstream")
    );
}
