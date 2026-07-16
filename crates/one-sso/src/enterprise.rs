//! Auto-join hook for the SSO "real enterprise" tier.
//!
//! Kept as a trait so one-sso does not take a hard dependency on one-org
//! (same-layer domain crates interact through traits only). The app layer
//! implements it over `OrgService::auto_provision_enterprise`. When no joiner
//! is wired (personal edition, unit tests) SSO login behaves exactly as before
//! — authenticate only, no membership change.

use async_trait::async_trait;

#[async_trait]
pub trait EnterpriseAutoJoiner: Send + Sync {
    /// Called after a successful SSO login that carried a company identifier
    /// (Feishu `tenant_key` etc.). Implementations join the user to the
    /// enterprise bound to that company, if one exists.
    ///
    /// Best-effort and **must never fail the login**: a user who authenticated
    /// correctly should still get a session even if membership can't be
    /// resolved. Implementations swallow their own errors; the return value is
    /// only whether a membership was written.
    async fn try_auto_join(&self, user_id: &str, provider: &str, org_external_id: &str) -> bool;
}
