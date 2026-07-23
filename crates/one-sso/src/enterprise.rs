//! Enterprise-org sync hook for SSO login.
//!
//! Kept as a trait so one-sso does not take a hard dependency on one-enterprise
//! (same-layer domain crates interact through traits only). The app layer
//! implements it over `one_enterprise::EnterpriseService::sync_member`. When no
//! sync is wired (personal edition, unit tests) SSO login behaves exactly as
//! before — authenticate only, nothing else.
//!
//! This is the "enterprise org" dimension, deliberately SEPARATE from project
//! groups: it reflects the user's real SSO company + department and never
//! touches `one_tenants` / project-group membership.

use async_trait::async_trait;

/// Cross-tier check: is a user a company (真实企业) administrator? Wired by the
/// app layer over `one_enterprise::EnterpriseService::is_company_admin`.
///
/// SSO provider config (企业认证) is a company-level policy in Direction B, so
/// `RequireSsoAdmin` accepts a company admin. Kept as a trait because one-sso
/// and one-enterprise are same-layer domain crates (no direct dependency). When
/// unwired (personal edition), the check is unavailable and gating falls back to
/// the project-group `one_user_org` role — the standalone behaviour is unchanged.
#[async_trait]
pub trait CompanyAdminCheck: Send + Sync {
    async fn is_company_admin(&self, user_id: &str) -> bool;
}

#[async_trait]
pub trait EnterpriseSync: Send + Sync {
    /// Called after a successful SSO login that carried a company identifier
    /// (Feishu `tenant_key` etc.). Implementations upsert the SSO company and
    /// the user's membership in it (their own name / department / job title).
    ///
    /// Best-effort and **must never fail the login**: a user who authenticated
    /// correctly should still get a session even if the sync can't complete.
    /// Implementations swallow their own errors.
    async fn sync_member(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
    );
}
