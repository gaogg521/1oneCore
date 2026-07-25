//! one-platform error type. Mirrors one-org's code/status mapping so clients
//! handle platform-config errors the same way as org-admin errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("Not currently in an enterprise")]
    NotInEnterprise,

    #[error("{0}")]
    Forbidden(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl PlatformError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotInEnterprise => "NOT_IN_ENTERPRISE",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl IntoResponse for PlatformError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "one-platform internal error");
        }
        (status, Json(ErrorResponse::new(self.to_string(), self.code()))).into_response()
    }
}

impl From<sqlx::Error> for PlatformError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}
