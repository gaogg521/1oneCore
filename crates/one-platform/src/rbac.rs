//! Admin extractor for platform-config routes. Reuses the upstream auth
//! middleware's `CurrentUser`; resolves enterprise membership/role from
//! one-org's `one_user_org` table via the shared pool (same cross-crate
//! precedent as `one_devops::user_org_role`), so one-platform needs no
//! dependency on one-org.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use aionui_auth::CurrentUser;

use crate::error::PlatformError;
use crate::service::PlatformActor;
use crate::state::OnePlatformRouterState;

/// Requires enterprise membership with an admin role. Personal edition (no
/// membership row) → `NotInEnterprise`; non-admin → `Forbidden`.
#[derive(Debug, Clone)]
pub struct RequirePlatformAdmin(pub PlatformActor);

impl FromRequestParts<OnePlatformRouterState> for RequirePlatformAdmin {
    type Rejection = PlatformError;

    async fn from_request_parts(parts: &mut Parts, state: &OnePlatformRouterState) -> Result<Self, Self::Rejection> {
        let user = parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or_else(|| PlatformError::Forbidden("Authentication required".into()))?;
        let actor = state.service.require_admin(&user.id).await?;
        Ok(Self(actor))
    }
}
