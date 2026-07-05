//! one-sso error type; wire shape matches upstream `ErrorResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum SsoError {
    #[error("SSO provider not configured: {0}")]
    ProviderNotConfigured(String),

    #[error("SSO provider disabled: {0}")]
    ProviderDisabled(String),

    #[error("Invalid OAuth state")]
    InvalidState,

    #[error("Missing OAuth code")]
    MissingCode,

    #[error("SSO identity missing external id")]
    IdentityMissing,

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl SsoError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ProviderNotConfigured(_) => "SSO_PROVIDER_NOT_CONFIGURED",
            Self::ProviderDisabled(_) => "SSO_PROVIDER_DISABLED",
            Self::InvalidState => "SSO_INVALID_STATE",
            Self::MissingCode => "SSO_MISSING_CODE",
            Self::IdentityMissing => "SSO_IDENTITY_MISSING",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::ProviderNotConfigured(_) | Self::ProviderDisabled(_) => StatusCode::NOT_FOUND,
            Self::InvalidState | Self::MissingCode | Self::IdentityMissing | Self::BadRequest(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for SsoError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "one-sso internal error");
        }
        (status, Json(ErrorResponse::new(self.to_string(), self.code()))).into_response()
    }
}

impl From<sqlx::Error> for SsoError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_db::DbError> for SsoError {
    fn from(e: aionui_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_auth::AuthError> for SsoError {
    fn from(e: aionui_auth::AuthError) -> Self {
        Self::Internal(format!("auth error: {e}"))
    }
}

impl From<reqwest::Error> for SsoError {
    fn from(e: reqwest::Error) -> Self {
        Self::Internal(format!("http error: {e}"))
    }
}
