//! Company model policy as seen by the agent factory.
//!
//! The billing plane owns the model allowlist, but `one-billing` and this crate
//! sit at the same layer, so the factory cannot call it directly. Same shape as
//! [`aionui_auth::IpAllowlistGate`] / `aionui_conversation::SendGate`: the trait
//! lives here, `aionui-app` implements it over `BillingService`.
//!
//! Why the factory needs it at all: the allowlist used to be enforced only on
//! the *session* model — at send (`SendGate::check_send`) and at model switch
//! (`SendGate::check_model`). The vision delegate that `ReadImage` calls is a
//! second, independent model choice made here, so an admin who removed a model
//! from the allowlist could still have it invoked as a delegate.

use async_trait::async_trait;

/// Whether a model may be used under the caller's company policy.
///
/// Implementations must uphold the same red line as the rest of the billing
/// plane: a personal / no-company user, or a company with no allowlist
/// configured, returns `Ok(true)`.
///
/// `Err` means the check itself failed (a DB error, say) — it is NOT "allowed".
/// Callers fail closed on it, matching `BillingSendGate`'s `POLICY_CHECK_FAILED`.
#[async_trait]
pub trait ModelAllowlistGate: Send + Sync {
    async fn is_model_allowed(&self, user_id: &str, model: &str) -> Result<bool, String>;
}
