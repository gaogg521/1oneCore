//! one-enterprise error type; wire shape matches upstream `ErrorResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum EnterpriseError {
    #[error("Internal error: {0}")]
    Internal(String),
}

impl EnterpriseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for EnterpriseError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "one-enterprise internal error");
        }
        (status, Json(ErrorResponse::new(self.to_string(), self.code()))).into_response()
    }
}

impl From<sqlx::Error> for EnterpriseError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_db::DbError> for EnterpriseError {
    fn from(e: aionui_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}
