//! Project-group (one-org) auto-join hook for SSO login (P2-4 onboarding).
//!
//! Kept as a trait so one-sso does not take a hard dependency on one-org
//! (same-layer domain crates interact through traits only). The app layer
//! implements it over `one_org::OrgService::auto_join_by_email`. When no hook
//! is wired (personal edition, unit tests, or a login with no usable email)
//! SSO login behaves exactly as before — authenticate only.
//!
//! Deliberately SEPARATE from `EnterpriseSync`: this joins a `one_tenants`
//! project group by email-domain policy, never the SSO "real company".

use async_trait::async_trait;

#[async_trait]
pub trait OrgAutoJoin: Send + Sync {
    /// Called after a successful SSO login when the IdP profile yields a
    /// usable email address. Implementations look up any project group whose
    /// `allowed_email_domains` matches the email's domain and join the user to
    /// it (idempotent — already-a-member is a no-op).
    ///
    /// Best-effort and **must never fail the login**: implementations swallow
    /// their own errors.
    async fn auto_join_by_email(&self, user_id: &str, email: &str);
}
