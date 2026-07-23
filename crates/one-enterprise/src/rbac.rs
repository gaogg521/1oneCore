//! RBAC extractor for company-tier (真实企业) admin routes.
//!
//! Layers on top of upstream `auth_middleware` (which injects
//! `aionui_auth::CurrentUser`): resolves the caller's company membership and
//! requires the company `admin` role. Distinct from one-org's project-group
//! admin — a company administrator governs the company, not a single project
//! group.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use aionui_auth::CurrentUser;

use crate::error::EnterpriseError;
use crate::state::OneEnterpriseRouterState;

/// Authenticated caller resolved as an admin of their own company.
#[derive(Debug, Clone)]
pub struct RequireCompanyAdmin {
    pub user_id: String,
    pub enterprise_id: String,
}

impl FromRequestParts<OneEnterpriseRouterState> for RequireCompanyAdmin {
    type Rejection = EnterpriseError;

    async fn from_request_parts(parts: &mut Parts, state: &OneEnterpriseRouterState) -> Result<Self, Self::Rejection> {
        let user = parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or_else(|| EnterpriseError::Forbidden("Authentication required".into()))?;
        let enterprise_id = state
            .service
            .company_of(&user.id)
            .await?
            .ok_or_else(|| EnterpriseError::Forbidden("Company administrator role required".into()))?;
        if !state.service.is_company_admin_of(&user.id, &enterprise_id).await? {
            return Err(EnterpriseError::Forbidden("Company administrator role required".into()));
        }
        Ok(Self {
            user_id: user.id,
            enterprise_id,
        })
    }
}
