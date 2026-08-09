//! Ending a company membership has to end the access that came with it.
//!
//! Company membership is the seat AND the identity tier (企业 ⊃ 项目组), so
//! deleting the row without touching the session leaves a removed person fully
//! logged in — holding a valid JWT and a company model channel token — until
//! the token happens to expire. That is precisely the state the T6 departure
//! flow exists to end: the console would report "已移出企业" while the leaver
//! keeps working.
//!
//! The rotation itself lives in one-org (`OrgService`, which owns the user's
//! JWT secret and, through its own `CredentialRevoker`, the channel tokens).
//! one-org and one-enterprise are the same layer, so this goes through a trait
//! the app wires up — the same arrangement as `one_sso::EnterpriseSync` and
//! `one_org::CredentialRevoker`.

use async_trait::async_trait;

/// Invalidates every credential a user holds: session tokens and anything
/// deliberately outliving them.
///
/// Best-effort by contract: an implementation swallows its own errors and the
/// removal proceeds regardless. Refusing to remove somebody because the
/// revocation failed would leave them a *member* as well as logged in, which is
/// strictly worse; the failure is logged loudly for follow-up.
#[async_trait]
pub trait SessionRevoker: Send + Sync {
    async fn revoke_sessions(&self, user_id: &str);
}

/// Default when nothing is wired. Personal and standalone installs have no
/// company, so no company removal ever happens there and there is nothing to
/// revoke. The service that actually serves the removal route is built with a
/// real revoker in `aionui-app`; this default exists for the directory-sink
/// instance and for unit tests, neither of which remove members.
pub struct NoopSessionRevoker;

#[async_trait]
impl SessionRevoker for NoopSessionRevoker {
    async fn revoke_sessions(&self, _user_id: &str) {}
}
