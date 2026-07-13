//! one-org error type. Error codes mirror the 1ONE ClaudeCode TS
//! `EnterpriseJoinError` codes verbatim so existing clients keep working.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum OrgError {
    #[error("Invalid invite code")]
    InvalidCode,

    #[error("Already joined an enterprise")]
    AlreadyInEnterprise,

    #[error("Not currently in an enterprise")]
    NotInEnterprise,

    #[error("Tenant not found")]
    TenantNotFound,

    #[error("{0}")]
    Forbidden(String),

    #[error("This server already hosts an enterprise; a server can host only one. Members should join via invite code.")]
    AlreadyHostsEnterprise,

    #[error("Enterprise name is required")]
    NameRequired,

    #[error("The enterprise has not configured an exit password. Contact your administrator.")]
    NoExitPasswordSet,

    #[error("Incorrect exit code")]
    WrongExitCode,

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl OrgError {
    /// Stable machine-readable code (TS `EnterpriseJoinErrorCode` parity).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidCode => "INVALID_CODE",
            Self::AlreadyInEnterprise => "ALREADY_IN_ENTERPRISE",
            Self::NotInEnterprise => "NOT_IN_ENTERPRISE",
            Self::TenantNotFound => "TENANT_NOT_FOUND",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::AlreadyHostsEnterprise => "ALREADY_HOSTS_ENTERPRISE",
            Self::NameRequired => "NAME_REQUIRED",
            Self::NoExitPasswordSet => "NO_EXIT_PASSWORD_SET",
            Self::WrongExitCode => "WRONG_EXIT_CODE",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::AlreadyHostsEnterprise => StatusCode::FORBIDDEN,
            Self::TenantNotFound => StatusCode::NOT_FOUND,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl IntoResponse for OrgError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "one-org internal error");
        }
        (status, Json(ErrorResponse::new(self.to_string(), self.code()))).into_response()
    }
}

impl From<sqlx::Error> for OrgError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_db::DbError> for OrgError {
    fn from(e: aionui_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<aionui_auth::AuthError> for OrgError {
    fn from(e: aionui_auth::AuthError) -> Self {
        Self::Internal(format!("auth error: {e}"))
    }
}
