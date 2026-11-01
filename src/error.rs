use axum::{Json, http::StatusCode, response::IntoResponse};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
/// Application error type used across OAuth, upstream API, and HTTP server layers.
pub enum Error {
    /// Filesystem or local OS error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP client or transport error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization or parsing error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// URL parsing error.
    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),

    /// OAuth-specific validation or protocol failure.
    #[error("OAuth error: {0}")]
    OAuth(String),

    /// Local configuration problem.
    #[error("configuration error: {0}")]
    Config(String),

    /// Error returned by an upstream dependency or API.
    #[error("upstream error: {message}")]
    Upstream {
        /// HTTP status surfaced to downstream clients for this upstream error.
        status: StatusCode,
        /// Human-readable upstream error text.
        message: String,
    },

    /// Runtime reload validation or control protocol failure.
    #[error(transparent)]
    Reload(#[from] crate::server::reload::ReloadError),

    /// Authentication failure for an incoming API request.
    #[error("unauthorized")]
    Unauthorized,
}

/// Convenience result alias for fallible operations in this crate.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Creates an OAuth error with the provided message.
    pub fn oauth(message: impl Into<String>) -> Self {
        Self::OAuth(message.into())
    }

    /// Creates a configuration error with the provided message.
    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Creates an upstream error with the provided message.
    pub fn upstream(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Upstream {
            status: infer_upstream_status(&message),
            message,
        }
    }

    /// Creates an upstream error with an explicit downstream HTTP status code.
    pub fn upstream_with_status(status: StatusCode, message: impl Into<String>) -> Self {
        Self::Upstream {
            status,
            message: message.into(),
        }
    }

    /// Maps an application error to the HTTP status code exposed by the server.
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Config(_) | Self::OAuth(_) | Self::Reload(_) => StatusCode::BAD_REQUEST,
            Self::Upstream { status, .. } => *status,
            Self::Http(_) => StatusCode::BAD_GATEWAY,
            Self::Io(_) | Self::Json(_) | Self::Url(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn client_message(&self) -> String {
        match self {
            Self::Upstream { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }

    fn error_type(&self) -> &'static str {
        match self.status_code() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "authentication_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            status if status.is_server_error() || status == StatusCode::BAD_GATEWAY => {
                "upstream_error"
            }
            _ => "invalid_request_error",
        }
    }
}

fn infer_upstream_status(message: &str) -> StatusCode {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("input exceeds")
        && (normalized.contains("context window") || normalized.contains("context length"))
    {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    if normalized.contains("context window") && normalized.contains("too large") {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }

    StatusCode::BAD_GATEWAY
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        tracing::error!(status = status.as_u16(), error = %self);
        // Mirror the OpenAI-style error envelope expected by API clients.
        let body = Json(json!({
            "error": {
                "message": self.client_message(),
                "type": self.error_type(),
            }
        }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_context_window_errors_are_payload_too_large() {
        let error = Error::upstream(
            "Your input exceeds the context window of this model. Please adjust your input and try again.",
        );

        assert_eq!(error.status_code(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn upstream_unknown_errors_remain_bad_gateway() {
        let error = Error::upstream("temporary upstream outage");

        assert_eq!(error.status_code(), StatusCode::BAD_GATEWAY);
    }
}
