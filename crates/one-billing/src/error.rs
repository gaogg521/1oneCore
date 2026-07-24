//! one-billing error type; wire shape matches upstream `ErrorResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("Internal error: {0}")]
    Internal(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("No company has been set up on this server")]
    EnterpriseNotFound,
    #[error("Seat limit reached for the current plan")]
    SeatLimitExceeded,
}

impl BillingError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Internal(_) => "INTERNAL_ERROR",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::EnterpriseNotFound => "ENTERPRISE_NOT_FOUND",
            Self::SeatLimitExceeded => "SEAT_LIMIT_EXCEEDED",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::EnterpriseNotFound => StatusCode::NOT_FOUND,
            Self::SeatLimitExceeded => StatusCode::CONFLICT,
        }
    }
}

impl IntoResponse for BillingError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "one-billing internal error");
        }
        (status, Json(ErrorResponse::new(self.to_string(), self.code()))).into_response()
    }
}

impl From<sqlx::Error> for BillingError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_db::DbError> for BillingError {
    fn from(e: aionui_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}
