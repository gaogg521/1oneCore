//! RBAC extractors layered on top of the upstream auth middleware.
//!
//! Upstream `auth_middleware` authenticates the request and injects
//! `aionui_auth::CurrentUser` into request extensions; these extractors add
//! tenant/role resolution from `one_user_org` without touching upstream code.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use aionui_auth::CurrentUser;

use crate::error::OrgError;
use crate::models::{is_admin_role, is_enterprise_tenant_id, is_system_admin_role};
use crate::state::OneOrgRouterState;

/// Authenticated user + resolved enterprise membership.
#[derive(Debug, Clone)]
pub struct OrgActor {
    pub user_id: String,
    pub username: String,
    pub tenant_id: String,
    pub role: String,
}

impl FromRequestParts<OneOrgRouterState> for OrgActor {
    type Rejection = OrgError;

    async fn from_request_parts(parts: &mut Parts, state: &OneOrgRouterState) -> Result<Self, Self::Rejection> {
        let user = parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or_else(|| OrgError::Forbidden("Authentication required".into()))?;
        let tenant_id = state.service.tenant_of(&user.id).await?;
        let role = state.service.effective_role(&user.id).await?;
        Ok(Self {
            user_id: user.id,
            username: user.username,
            tenant_id,
            role,
        })
    }
}

/// Requires enterprise membership with an admin role (org_admin /
/// system_admin / legacy admin).
#[derive(Debug, Clone)]
pub struct RequireOrgAdmin(pub OrgActor);

impl FromRequestParts<OneOrgRouterState> for RequireOrgAdmin {
    type Rejection = OrgError;

    async fn from_request_parts(parts: &mut Parts, state: &OneOrgRouterState) -> Result<Self, Self::Rejection> {
        let actor = OrgActor::from_request_parts(parts, state).await?;
        if !is_enterprise_tenant_id(&actor.tenant_id) {
            return Err(OrgError::NotInEnterprise);
        }
        if !is_admin_role(&actor.role) {
            return Err(OrgError::Forbidden("Administrator role required".into()));
        }
        Ok(Self(actor))
    }
}

/// Requires the instance-level system_admin role.
#[derive(Debug, Clone)]
pub struct RequireSystemAdmin(pub OrgActor);

impl FromRequestParts<OneOrgRouterState> for RequireSystemAdmin {
    type Rejection = OrgError;

    async fn from_request_parts(parts: &mut Parts, state: &OneOrgRouterState) -> Result<Self, Self::Rejection> {
        let actor = OrgActor::from_request_parts(parts, state).await?;
        if !is_system_admin_role(&actor.role) {
            return Err(OrgError::Forbidden("System administrator role required".into()));
        }
        Ok(Self(actor))
    }
}
