use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("OAuth authorization failed: {code}")]
    OAuth { code: &'static str, retryable: bool },
    #[error("message content is unavailable: {0}")]
    ContentUnavailable(String),
    #[error("provider request throttled")]
    Throttled(u64),
    #[error(
        "message exceeds the supported {backend} sending limit of {maximum_mime_bytes} MIME bytes (actual: {actual_mime_bytes})"
    )]
    SendLimitExceeded {
        maximum_mime_bytes: usize,
        actual_mime_bytes: usize,
        backend: &'static str,
    },
    #[error(
        "{backend} {component} exceeds its supported limit of {maximum_bytes} bytes (actual: {actual_bytes})"
    )]
    SendComponentLimitExceeded {
        backend: &'static str,
        component: &'static str,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    #[error("provider resource not found (code: {code}, request: {request_id})")]
    ProviderNotFound { code: String, request_id: String },
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("operation outcome is unknown; reconcile before retrying")]
    UnknownOutcome,
    #[error("unauthorized")]
    Unauthorized,
    #[error("event cursor expired; resynchronize metadata")]
    CursorExpired {
        minimum_cursor: u64,
        current_cursor: u64,
    },
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("internal service error")]
    Internal(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::OAuth { code, retryable } => (
                if *retryable {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::CONFLICT
                },
                *code,
            ),
            Self::ContentUnavailable(_) => (StatusCode::GONE, "content_unavailable"),
            Self::Throttled(_) => (StatusCode::TOO_MANY_REQUESTS, "provider_throttled"),
            Self::SendLimitExceeded { .. } => {
                (StatusCode::PAYLOAD_TOO_LARGE, "send_limit_exceeded")
            }
            Self::SendComponentLimitExceeded { .. } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "send_component_limit_exceeded",
            ),
            Self::ProviderNotFound { .. } => {
                (StatusCode::BAD_GATEWAY, "provider_resource_not_found")
            }
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Self::Unsupported(_) => (StatusCode::UNPROCESSABLE_ENTITY, "unsupported"),
            Self::UnknownOutcome => (StatusCode::BAD_GATEWAY, "unknown_outcome"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::CursorExpired { .. } => (StatusCode::GONE, "cursor_expired"),
            Self::Database(_) | Self::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };
        let mut body = json!({"error":{"code":code,"message":self.to_string()}});
        if let Self::SendComponentLimitExceeded {
            backend,
            component,
            maximum_bytes,
            actual_bytes,
        } = &self
        {
            body["error"]["backend"] = json!(backend);
            body["error"]["component"] = json!(component);
            body["error"]["maximum_bytes"] = json!(maximum_bytes);
            body["error"]["actual_bytes"] = json!(actual_bytes);
        }
        if let Self::SendLimitExceeded {
            maximum_mime_bytes,
            actual_mime_bytes,
            backend,
        } = &self
        {
            body["error"]["maximum_mime_bytes"] = json!(maximum_mime_bytes);
            body["error"]["actual_mime_bytes"] = json!(actual_mime_bytes);
            body["error"]["backend"] = json!(backend);
        }
        if let Self::CursorExpired {
            minimum_cursor,
            current_cursor,
        } = self
        {
            body["error"]["minimum_cursor"] = json!(minimum_cursor);
            body["error"]["current_cursor"] = json!(current_cursor);
        }
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn expired_cursor_exposes_recovery_boundary() {
        let response = Error::CursorExpired {
            minimum_cursor: 40,
            current_cursor: 100,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::GONE);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["minimum_cursor"], 40);
        assert_eq!(value["error"]["current_cursor"], 100);
    }
    #[tokio::test]
    async fn send_limit_error_exposes_the_enforced_backend_limit() {
        let response = Error::SendLimitExceeded {
            maximum_mime_bytes: 3_000_000,
            actual_mime_bytes: 3_000_001,
            backend: "microsoft",
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "send_limit_exceeded");
        assert_eq!(value["error"]["maximum_mime_bytes"], 3_000_000);
        assert_eq!(value["error"]["actual_mime_bytes"], 3_000_001);
        assert_eq!(value["error"]["backend"], "microsoft");
    }
}
