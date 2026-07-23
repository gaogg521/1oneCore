//! Admin role gate for `one_sso_admin_routes`.
//!
//! Mirrors `one_org::rbac::RequireOrgAdmin` — same `one_user_org` table,
//! same role semantics — but reimplemented here rather than imported: `one-sso`
//! and `one-org` are same-layer domain crates and must not depend on each
//! other (see workspace `AGENTS.md` § Crate Hierarchy & Dependencies).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use aionui_auth::CurrentUser;

use crate::error::SsoError;
use crate::state::OneSsoRouterState;

/// Desktop-operator sentinel user id — defaults to system_admin when it has
/// no explicit `one_user_org` row. Matches `one_org::models::SYSTEM_DEFAULT_USER_ID`.
pub const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";
pub const ROLE_SYSTEM_ADMIN: &str = "system_admin";
pub const ROLE_ORG_ADMIN: &str = "org_admin";
pub const ROLE_MEMBER: &str = "member";

/// `admin` is the legacy alias kept for parity with the 1ONE TS role model
/// (matches `one_org::models::is_admin_role`).
pub fn is_admin_role(role: &str) -> bool {
    role == ROLE_SYSTEM_ADMIN || role == ROLE_ORG_ADMIN || role == "admin"
}

/// Authenticated user + resolved role, required on every `/api/one/admin/sso/*`
/// handler. Rejects with 403 for non-admins instead of letting any logged-in
/// member read/write the enterprise SSO config.
#[derive(Debug, Clone)]
pub struct RequireSsoAdmin {
    pub user_id: String,
}

impl FromRequestParts<OneSsoRouterState> for RequireSsoAdmin {
    type Rejection = SsoError;

    async fn from_request_parts(parts: &mut Parts, state: &OneSsoRouterState) -> Result<Self, Self::Rejection> {
        let user = parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or_else(|| SsoError::Forbidden("Authentication required".into()))?;
        // Direction B: SSO config (企业认证) is a company-level policy, so a
        // company administrator may manage it. Accept them first when the bridge
        // is wired.
        if let Some(check) = state.company_admin_check.as_ref() {
            if check.is_company_admin(&user.id).await {
                return Ok(Self { user_id: user.id });
            }
        }
        // Fallback: the project-group system_admin / org_admin (this also keeps
        // `system_default_user → system_admin` working for local / personal SSO
        // config, so standalone behaviour is unchanged).
        let role = state.service.effective_role(&user.id).await?;
        if !is_admin_role(&role) {
            return Err(SsoError::Forbidden("Administrator role required".into()));
        }
        Ok(Self { user_id: user.id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_admin_role_accepts_system_and_org_admin_and_legacy_alias() {
        assert!(is_admin_role(ROLE_SYSTEM_ADMIN));
        assert!(is_admin_role(ROLE_ORG_ADMIN));
        assert!(is_admin_role("admin"));
    }

    #[test]
    fn is_admin_role_rejects_member_and_unknown_roles() {
        assert!(!is_admin_role(ROLE_MEMBER));
        assert!(!is_admin_role(""));
        assert!(!is_admin_role("owner"));
    }
}
