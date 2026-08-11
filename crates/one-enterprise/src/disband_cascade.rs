//! Deleting a company must also delete what it owns elsewhere.
//!
//! A company (`one_enterprises`) sits at the top of "企业 ⊃ 项目组 ⊃ 个人" —
//! every project group under it (`one_tenants.enterprise_id`, owned by
//! one-org) and every billing/usage record attributed to it
//! (`one_usage_events` and friends, owned by one-billing) only makes sense as
//! long as the company exists. one-enterprise/one-org/one-billing are the
//! same layer, so this goes through a trait the app wires up — the same
//! arrangement as `SessionRevoker` and `one_org::CredentialRevoker`.

use async_trait::async_trait;

/// Deletes everything a disbanded company owns outside one-enterprise
/// itself: every project group (one-org) and every enterprise-scoped
/// billing/usage record (one-billing).
///
/// Best-effort by contract, same reasoning as `SessionRevoker`: a company
/// the operator asked to disband must actually go away even if one side's
/// cleanup hits an error — leaving the company record dangling because a
/// downstream delete failed would be worse than an implementation quietly
/// logging and moving on. Returns the ids of every project group it
/// deleted, purely for the caller to report back ("N project groups
/// removed").
#[async_trait]
pub trait CompanyDisbandCascade: Send + Sync {
    async fn disband(&self, enterprise_id: &str) -> Vec<String>;
}

/// Default when nothing is wired: personal/standalone installs never have a
/// company to disband, and instances that never serve the disband route
/// (e.g. the directory-sink instance) don't need a real implementation.
pub struct NoopCompanyDisbandCascade;

#[async_trait]
impl CompanyDisbandCascade for NoopCompanyDisbandCascade {
    async fn disband(&self, _enterprise_id: &str) -> Vec<String> {
        Vec::new()
    }
}
