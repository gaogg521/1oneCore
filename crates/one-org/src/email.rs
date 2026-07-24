//! Invite-email sending seam (P2-4 onboarding).
//!
//! No SMTP client library is wired in here — this is the "reserved adapter"
//! pattern used elsewhere in the app (e.g. `one_billing::BillingProvider` for
//! payment): a config store (see `OrgService::{get,set}_smtp_config`) plus a
//! pluggable `EmailSender` trait. `StubEmailSender` is the default and reports
//! "not configured"; when an operator has real SMTP credentials, a real
//! implementation (e.g. wrapping the `lettre` crate) can be dropped in at the
//! app layer without touching this crate.

use async_trait::async_trait;

/// Outcome of an invite-email send attempt, shaped like
/// `one_billing::CheckoutResultDto` for the same "not configured yet" UX.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendEmailResult {
    /// `"not_configured"` (stub) or `"sent"` (a real sender accepted it).
    pub status: String,
    pub message: String,
}

#[async_trait]
pub trait EmailSender: Send + Sync {
    /// Send an invite email. `to` is the recipient address; `invite_code` is
    /// the display-formatted code; `tenant_name` names the project group.
    async fn send_invite(&self, to: &str, invite_code: &str, tenant_name: &str) -> SendEmailResult;
}

/// No SMTP wired: every send reports back that email delivery is not
/// configured yet, so the admin UI can surface a clear "set up SMTP first"
/// message instead of silently doing nothing.
pub struct StubEmailSender;

#[async_trait]
impl EmailSender for StubEmailSender {
    async fn send_invite(&self, _to: &str, _invite_code: &str, _tenant_name: &str) -> SendEmailResult {
        SendEmailResult {
            status: "not_configured".to_owned(),
            message: "SMTP is not configured. Save an SMTP config, or share the invite code directly.".to_owned(),
        }
    }
}
