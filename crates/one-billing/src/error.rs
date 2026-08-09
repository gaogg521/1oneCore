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
    #[error("The team's usage budget for this period has been reached")]
    BudgetExceeded,
    #[error("This department's usage budget for this period has been reached")]
    DepartmentBudgetExceeded,
    #[error("Model '{0}' is not allowed by the team's policy")]
    ModelNotAllowed(String),
    #[error("Upgrading the plan requires activating a license key")]
    UpgradeRequiresLicense,
    #[error("{0}")]
    InvalidLicenseKey(String),
}

impl From<crate::license_key::LicenseKeyError> for BillingError {
    fn from(e: crate::license_key::LicenseKeyError) -> Self {
        Self::InvalidLicenseKey(e.to_string())
    }
}

impl BillingError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Internal(_) => "INTERNAL_ERROR",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::EnterpriseNotFound => "ENTERPRISE_NOT_FOUND",
            Self::SeatLimitExceeded => "SEAT_LIMIT_EXCEEDED",
            Self::BudgetExceeded => "BUDGET_EXCEEDED",
            Self::DepartmentBudgetExceeded => "DEPARTMENT_BUDGET_EXCEEDED",
            Self::ModelNotAllowed(_) => "MODEL_NOT_ALLOWED",
            Self::UpgradeRequiresLicense => "UPGRADE_REQUIRES_LICENSE",
            Self::InvalidLicenseKey(_) => "INVALID_LICENSE_KEY",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::EnterpriseNotFound => StatusCode::NOT_FOUND,
            Self::SeatLimitExceeded
            | Self::BudgetExceeded
            | Self::DepartmentBudgetExceeded
            | Self::ModelNotAllowed(_) => StatusCode::CONFLICT,
            // The request was well-formed and authorized; it is the *plan* that
            // forbids it — same 409 family as the other entitlement refusals.
            Self::UpgradeRequiresLicense => StatusCode::CONFLICT,
            Self::InvalidLicenseKey(_) => StatusCode::BAD_REQUEST,
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
