//! Revoking the credentials that deliberately outlive a session.
//!
//! Removing someone from a company rotates their JWT secret, which kills every
//! session they hold. That is enough for anything gated by a session — but not
//! for a company **model channel token**, whose whole point is to survive JWT
//! rotation (membership changes rotate the secret routinely, and a provisioned
//! channel must not break every time someone joins a project group).
//!
//! So the one credential that outlives the session has to be revoked
//! explicitly, at the same moment and by the same code path. Doing it here
//! rather than leaving it to a UI offboarding flow is deliberate: a leaver
//! keeping a working key to the company's models is precisely the failure the
//! channel design exists to prevent, and a step a caller can forget is a step
//! that will be forgotten.
//!
//! one-org and one-devops are the same layer, so this goes through a trait the
//! app wires up — the same arrangement `one_sso::EnterpriseSync` uses.

use async_trait::async_trait;

/// Revokes non-session credentials belonging to a user.
///
/// Best-effort by contract: an implementation swallows its own errors and the
/// removal proceeds regardless. A member who could not be fully de-provisioned
/// must still be removed — leaving them in the company because a side effect
/// failed would be the worse outcome, and the failure is logged for follow-up.
#[async_trait]
pub trait CredentialRevoker: Send + Sync {
    async fn revoke_for_user(&self, user_id: &str);
}

/// Default when nothing is wired: personal and standalone installs have no
/// company channels, so there is nothing to revoke.
pub struct NoopCredentialRevoker;

#[async_trait]
impl CredentialRevoker for NoopCredentialRevoker {
    async fn revoke_for_user(&self, _user_id: &str) {}
}
