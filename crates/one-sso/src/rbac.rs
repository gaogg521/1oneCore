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
        check_sso_admin(state, &user.id).await?;
        Ok(Self { user_id: user.id })
    }
}

/// The actual decision, pulled out of `from_request_parts` so it is testable
/// without building a full axum `Parts`/request harness.
async fn check_sso_admin(state: &OneSsoRouterState, user_id: &str) -> Result<(), SsoError> {
    // Direction B: SSO config (企业认证) is a company-level policy, so a
    // company administrator may manage it. Accept them first when the bridge
    // is wired.
    if let Some(check) = state.company_admin_check.as_ref() {
        if check.is_company_admin(user_id).await {
            return Ok(());
        }
        // A company exists but this caller isn't its admin — do NOT fall
        // through to the project-group role check below. That fallback
        // exists only for the standalone/no-company case; without this
        // guard, any project group's org_admin (a much lower-privilege
        // role, scoped to their own small group) could read/write the
        // WHOLE company's SSO identity config (OIDC/LDAP/Feishu secrets)
        // just by being an admin of some unrelated project group.
        if check.company_exists().await {
            return Err(SsoError::Forbidden("Company administrator role required".into()));
        }
    }
    // Fallback: the project-group system_admin / org_admin (this also keeps
    // `system_default_user → system_admin` working for local / personal SSO
    // config, so standalone behaviour is unchanged). Only reached when no
    // company exists at all — see the guard above.
    let role = state.service.effective_role(user_id).await?;
    if !is_admin_role(&role) {
        return Err(SsoError::Forbidden("Administrator role required".into()));
    }
    Ok(())
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

    // --- check_sso_admin ---

    use std::sync::Arc;

    use aionui_auth::{CookieConfig, JwtService};
    use aionui_db::IUserRepository;

    use crate::enterprise::CompanyAdminCheck;
    use crate::service::SsoService;

    struct FakeCompanyAdminCheck {
        admin_of: &'static str,
        exists: bool,
    }

    #[async_trait::async_trait]
    impl CompanyAdminCheck for FakeCompanyAdminCheck {
        async fn is_company_admin(&self, user_id: &str) -> bool {
            user_id == self.admin_of
        }
        async fn company_exists(&self) -> bool {
            self.exists
        }
    }

    async fn state_with(company_admin_check: Option<Arc<dyn CompanyAdminCheck>>) -> OneSsoRouterState {
        let db = aionui_db::init_database_memory().await.unwrap();
        // Same minimal cross-crate shape as `service.rs`'s own test helper —
        // one-sso doesn't own these tables, `effective_role` just reads them.
        sqlx::query(
            "CREATE TABLE one_user_org (\
                 user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', \
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (user_id, tenant_id))",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO one_user_org (user_id, tenant_id, role, created_at, updated_at) VALUES ('group_admin', 'tA', 'org_admin', 0, 0)")
            .execute(db.pool())
            .await
            .unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let service = Arc::new(SsoService::new(
            db.pool().clone(),
            user_repo,
            Arc::new(JwtService::new("test-secret".into())),
            Arc::new(CookieConfig {
                secure: false,
                same_site: "Lax",
            }),
        ));
        OneSsoRouterState {
            service,
            enterprise_sync: None,
            company_admin_check,
            org_auto_join: None,
            directory_sink: None,
        }
    }

    /// The vulnerability this fix closes: "group_admin" is org_admin of
    /// project group tA only — not the company's admin. Once a company
    /// exists, that lower-privilege role must not reach company-wide SSO
    /// config just by falling through the standalone fallback.
    #[tokio::test]
    async fn a_project_group_admin_is_rejected_once_a_company_exists() {
        let state = state_with(Some(Arc::new(FakeCompanyAdminCheck {
            admin_of: "someone_else",
            exists: true,
        })))
        .await;
        let err = check_sso_admin(&state, "group_admin").await.unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
    }

    #[tokio::test]
    async fn the_actual_company_admin_is_accepted() {
        let state = state_with(Some(Arc::new(FakeCompanyAdminCheck {
            admin_of: "company_admin",
            exists: true,
        })))
        .await;
        assert!(check_sso_admin(&state, "company_admin").await.is_ok());
    }

    /// Standalone behaviour must survive: no company at all (bridge wired
    /// but `company_exists()` false, e.g. a fresh deployment) still lets the
    /// project-group admin through — this is the fallback the fix must not
    /// break.
    #[tokio::test]
    async fn falls_back_to_project_group_role_when_no_company_exists() {
        let state = state_with(Some(Arc::new(FakeCompanyAdminCheck {
            admin_of: "nobody",
            exists: false,
        })))
        .await;
        assert!(check_sso_admin(&state, "group_admin").await.is_ok());
    }

    /// Personal edition / tests with no bridge wired at all — the original,
    /// pre-Direction-B behaviour, unaffected by this fix.
    #[tokio::test]
    async fn falls_back_to_project_group_role_when_no_bridge_is_wired() {
        let state = state_with(None).await;
        assert!(check_sso_admin(&state, "group_admin").await.is_ok());
    }

    #[tokio::test]
    async fn a_plain_member_is_rejected_in_every_case() {
        let state = state_with(None).await;
        let err = check_sso_admin(&state, "nobody_at_all").await.unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
    }
}
