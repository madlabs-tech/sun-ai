//! Maps `sun_core::Error` to HTTP: `{"error": {code, message}}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sun_core::Error;

/// A handler error: status, machine code and a message the UI shows as is.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub(crate) fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Sign in with the token `sun serve` printed.",
        )
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let (status, code) = match &error {
            Error::InvalidInput(_) | Error::Refused { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
            }
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Error::PlanLimit(_) => (StatusCode::FORBIDDEN, "plan_limit"),
            Error::InvalidTransition { .. } => (StatusCode::CONFLICT, "conflict"),
            Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Error::Protocol(_) => (StatusCode::BAD_GATEWAY, "protocol"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}
