//! Integration connector seam (P2-1 reserved framework).
//!
//! No connector client (Octocrab / gitlab / jira / Feishu SDK) is wired in
//! here — this is the same "reserved adapter" pattern used for invite email
//! (`EmailSender`) and billing (`one_billing::BillingProvider`): a config store
//! (see `OrgService::{list,get,set}_integration`) plus a pluggable
//! `IntegrationProvider` trait. `StubIntegrationProvider` is the default and
//! reports "not configured"; when real credentials and a real client are
//! available, a concrete implementation can be dropped in at the app layer via
//! `OrgService::with_integration_provider` without touching this crate.

use async_trait::async_trait;

/// The connector providers the config layer knows about. Kept as a small,
/// explicit set so the admin UI can enumerate them; the storage layer itself
/// treats `provider` as free text, so adding one here (plus a UI card) is all
/// that a new connector needs before its real sync is built.
pub const KNOWN_PROVIDERS: &[&str] = &["github", "gitlab", "jira", "feishu"];

/// Outcome of a connector "test connection" attempt, shaped like
/// `SendEmailResult` for the same "not configured yet" UX.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationTestResult {
    /// `"not_configured"` (stub), `"ok"` (a real provider reached the system),
    /// or `"error"` (a real provider tried and failed).
    pub status: String,
    pub message: String,
}

/// Non-secret + secret connector configuration handed to a provider for a
/// live operation. `secret` is already decrypted; never log it.
pub struct IntegrationCredentials<'a> {
    pub provider: &'a str,
    pub base_url: Option<&'a str>,
    /// Non-secret provider-specific fields (org / project / repo / board ...).
    pub config: &'a serde_json::Value,
    pub secret: Option<&'a str>,
}

#[async_trait]
pub trait IntegrationProvider: Send + Sync {
    /// Probe whether the saved connector config can reach its external system.
    async fn test_connection(&self, creds: IntegrationCredentials<'_>) -> IntegrationTestResult;
}

/// No connector wired: every probe reports that live syncing is not configured
/// yet, so the admin UI can surface a clear "saved, but sync isn't available
/// yet" message instead of implying the connector is live.
pub struct StubIntegrationProvider;

#[async_trait]
impl IntegrationProvider for StubIntegrationProvider {
    async fn test_connection(&self, _creds: IntegrationCredentials<'_>) -> IntegrationTestResult {
        IntegrationTestResult {
            status: "not_configured".to_owned(),
            message: "Connector sync is not wired in yet. The configuration is saved and will be used once live \
                      syncing is available."
                .to_owned(),
        }
    }
}
