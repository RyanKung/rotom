//! Account and rate-limit status client.

mod client;
mod model;
mod parse;

pub use client::StatusClient;
pub use model::{AccountStatus, RateLimitWindow, StatusSnapshot};

const ACCOUNT_CHECK_VERSION: &str = "v4-2023-04-27";

#[cfg(test)]
mod tests {
    use super::{
        ACCOUNT_CHECK_VERSION, StatusClient,
        parse::{backend_api_base_url, parse_account_status, parse_usage_status},
    };
    use crate::config::Credentials;
    use axum::{Json, Router, routing::get};
    use reqwest::Client;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    fn sample_credentials() -> Credentials {
        Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 1,
            account_id: "acc_1".into(),
        }
    }

    #[test]
    fn strips_codex_suffix_from_base_url() {
        assert_eq!(
            backend_api_base_url("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            backend_api_base_url("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            backend_api_base_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api"
        );
    }

    #[test]
    fn parses_account_status() {
        let account = parse_account_status(&json!({
            "accounts": {
                "default": {
                    "account": {
                        "name": "Personal",
                        "structure": "personal"
                    },
                    "entitlement": {
                        "subscription_plan": "chatgptplus",
                        "has_active_subscription": true,
                        "expires_at": "2026-05-01T00:00:00Z"
                    }
                }
            }
        }))
        .unwrap();

        assert_eq!(account.plan.as_deref(), Some("chatgptplus"));
        assert_eq!(account.structure.as_deref(), Some("personal"));
        assert_eq!(account.email, None);
        assert_eq!(
            account.subscription_expires_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
    }

    #[test]
    fn parses_usage_status() {
        let usage = parse_usage_status(&json!({
            "email": "test@example.com",
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12,
                    "reset_at": "2026-04-27T12:00:00Z"
                },
                "secondary_window": {
                    "remaining_percent": 94,
                    "reset_at": "2026-05-01T00:00:00Z"
                }
            },
            "credits": { "balance": "7.5" }
        }))
        .unwrap();

        assert_eq!(usage.account.unwrap().plan.as_deref(), Some("pro"));
        assert_eq!(usage.credits_balance, Some(7.5));
        assert_eq!(usage.rate_limits.len(), 2);
        assert!((usage.rate_limits[0].remaining_percent - 88.0).abs() < f64::EPSILON);
        assert!((usage.rate_limits[1].remaining_percent - 94.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn fetches_partial_status_with_warnings() {
        async fn account_handler() -> Json<Value> {
            Json(json!({
                "accounts": {
                    "default": {
                        "account": { "structure": "personal" },
                        "entitlement": {
                            "subscription_plan": "chatgptplus",
                            "has_active_subscription": true
                        }
                    }
                }
            }))
        }

        async fn usage_handler() -> Json<Value> {
            Json(json!({
                "email": "test@example.com",
                "plan_type": "pro",
                "rate_limit": {
                    "primary_window": { "used_percent": 10 },
                    "secondary_window": { "remaining_percent": 90 }
                },
                "credits": { "balance": 1 }
            }))
        }

        let app = Router::new()
            .route(
                &format!("/accounts/check/{ACCOUNT_CHECK_VERSION}"),
                get(account_handler),
            )
            .route("/wham/usage", get(usage_handler));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = StatusClient::new(Client::new(), base_url);
        let snapshot = client.fetch_status(&sample_credentials()).await;

        assert_eq!(snapshot.warnings, Vec::<String>::new());
        assert_eq!(
            snapshot.account.unwrap().plan.as_deref(),
            Some("chatgptplus")
        );
        assert_eq!(snapshot.rate_limits.len(), 2);
        assert_eq!(snapshot.credits_balance, Some(1.0));
    }
}
