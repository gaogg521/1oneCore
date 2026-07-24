//! one-enterprise error type; wire shape matches upstream `ErrorResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aionui_api_types::ErrorResponse;

#[derive(Debug, thiserror::Error)]
pub enum EnterpriseError {
    #[error("Internal error: {0}")]
    Internal(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("Company name is required")]
    NameRequired,
    #[error("This server already hosts a company")]
    CompanyExists,
    #[error("No company has been set up on this server")]
    CompanyNotFound,
    #[error("User is not a member of this company")]
    MemberNotFound,
    #[error("Invalid role: {0}")]
    InvalidRole(String),
    #[error("Cannot remove the last company administrator")]
    LastCompanyAdmin,
    #[error("Seat limit reached for the current plan")]
    SeatLimitExceeded,
}

impl EnterpriseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Internal(_) => "INTERNAL_ERROR",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::NameRequired => "COMPANY_NAME_REQUIRED",
            Self::CompanyExists => "COMPANY_ALREADY_EXISTS",
            Self::CompanyNotFound => "COMPANY_NOT_FOUND",
            Self::MemberNotFound => "COMPANY_MEMBER_NOT_FOUND",
            Self::InvalidRole(_) => "INVALID_ROLE",
            Self::LastCompanyAdmin => "LAST_COMPANY_ADMIN",
            Self::SeatLimitExceeded => "SEAT_LIMIT_EXCEEDED",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NameRequired | Self::InvalidRole(_) => StatusCode::BAD_REQUEST,
            Self::CompanyExists | Self::LastCompanyAdmin | Self::SeatLimitExceeded => StatusCode::CONFLICT,
            Self::CompanyNotFound | Self::MemberNotFound => StatusCode::NOT_FOUND,
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
