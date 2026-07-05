//! one-employee error type; wire shape matches upstream `ErrorResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum EmployeeError {
    #[error("Digital employee not found")]
    NotFound,

    #[error("Run not found")]
    RunNotFound,

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl EmployeeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "EMPLOYEE_NOT_FOUND",
            Self::RunNotFound => "RUN_NOT_FOUND",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound | Self::RunNotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for EmployeeError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "one-employee internal error");
        }
        (status, Json(ErrorResponse::new(self.to_string(), self.code()))).into_response()
    }
}

impl From<sqlx::Error> for EmployeeError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_db::DbError> for EmployeeError {
    fn from(e: aionui_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}
