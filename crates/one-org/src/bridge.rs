//! Cross-tier bridge to the company (真实企业 / one-enterprise) domain.
//!
//! one-org and one-enterprise are same-layer domain crates and must not depend
//! on each other (workspace `AGENTS.md` § Crate Hierarchy). This trait lets a
//! company administrator act on the project groups their company owns; the app
//! layer implements it over `one_enterprise::EnterpriseService`. When no bridge
//! is wired (personal edition / unit tests), company-scoped tenant management is
//! simply unavailable and the standalone invite-code paths are unaffected.

use async_trait::async_trait;

#[async_trait]
pub trait CompanyAdminResolver: Send + Sync {
    /// Whether `user_id` is an administrator of company `enterprise_id`.
    async fn is_company_admin(&self, user_id: &str, enterprise_id: &str) -> bool;
}
