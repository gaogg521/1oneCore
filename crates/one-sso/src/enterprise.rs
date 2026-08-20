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

/// Where a completed directory pull goes (T6). Implemented by the app layer
/// over `one_enterprise::EnterpriseService::apply_directory_snapshot`.
///
/// A trait for the same reason as [`EnterpriseSync`]: one-sso knows how to talk
/// to Feishu, one-enterprise owns the company's tables, and they are the same
/// layer. Unwired (personal edition, tests) the pull simply has nowhere to go
/// and the sync is a no-op.
///
/// ⚠️ `complete` must be carried through faithfully. It is what tells the
/// storage side whether absence from `people` means "left the company" or
/// merely "we didn't manage to fetch them" — see
/// `one_enterprise::directory`'s module docs.
#[async_trait]
pub trait DirectorySink: Send + Sync {
    /// The company this deployment syncs into, or `None` when none is set up
    /// (which is also the signal that directory sync should not run at all).
    async fn enterprise_id(&self) -> Option<String>;

    async fn apply_snapshot(&self, enterprise_id: &str, snapshot: DirectorySnapshotPayload);
}

/// The provider-neutral payload handed across the seam. Mirrors
/// `one_enterprise::directory`'s input types without either crate depending on
/// the other.
///
/// Named fields rather than tuples on purpose: this crate has twice shipped
/// positional-argument bugs that compiled fine and silently did the wrong thing
/// (`upsert_dlp_rule`'s six `&str`s, `record_media_usage`'s seven mostly-`i64`
/// values). A person here is four optional-ish strings and a bool — exactly the
/// shape where a swapped pair is invisible.
#[derive(Debug, Clone)]
pub struct DirectorySnapshotPayload {
    pub provider: String,
    pub external_id_field: String,
    pub departments: Vec<DirectoryDepartmentPayload>,
    pub people: Vec<DirectoryPersonPayload>,
    pub complete: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DirectoryDepartmentPayload {
    pub external_id: String,
    pub parent_external_id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DirectoryPersonPayload {
    pub external_id: String,
    pub name: Option<String>,
    pub job_title: Option<String>,
    pub department_external_id: Option<String>,
    pub active: bool,
}

#[async_trait]
pub trait EnterpriseSync: Send + Sync {
    /// Called after a successful SSO login that carried a company identifier
    /// (Feishu `tenant_key` etc.). Implementations upsert the SSO company and
    /// the user's membership in it (their own name / department / job title).
    ///
    /// `external_id` is the **company's** IdP id (Feishu `tenant_key` etc,
    /// same value for every employee of that company) — do not confuse it
    /// with `personal_external_id`, the **individual's own** IdP id (Feishu
    /// `open_id`/`union_id`, unique per person), used to reconcile a pending
    /// company invite (see `EnterpriseService::create_invite`) against the
    /// person who just logged in.
    ///
    /// Best-effort and **must never fail the login**: a user who authenticated
    /// correctly should still get a session even if the sync can't complete.
    /// Implementations swallow their own errors.
    #[allow(clippy::too_many_arguments)]
    async fn sync_member(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
        personal_external_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
    );
}
