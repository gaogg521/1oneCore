//! Realtime collaboration seam (P2-2 reserved framework).
//!
//! No collaboration backend (presence relay / CRDT sync) is wired in here.
//! This is the "reserved adapter" pattern: a config store (see
//! `PlatformService::{get,set}_collaboration_config`) plus a pluggable
//! `CollaborationProvider` trait. `NoopCollaborationProvider` is the default
//! and reports "not configured"; a real provider can be dropped in at the app
//! layer via `PlatformService::with_collaboration_provider`.
//!
//! Distinct from `aionui-realtime` (the WebSocket *transport*): this layer is
//! the admin-configured collaboration *backend* (which relay, presence on/off,
//! auth token) — the transport is a separate concern that a real provider would
//! build on.

use async_trait::async_trait;

/// Outcome of a collaboration-backend probe.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationStatus {
    /// `"not_configured"` (stub), `"ok"` (backend reachable), or `"error"`.
    pub status: String,
    pub message: String,
}

/// Non-secret + secret collaboration config handed to a provider for a probe.
/// `secret` is already decrypted; never log it.
pub struct CollaborationSettings<'a> {
    pub provider: Option<&'a str>,
    pub endpoint: Option<&'a str>,
    pub secret: Option<&'a str>,
    pub presence: bool,
}

#[async_trait]
pub trait CollaborationProvider: Send + Sync {
    /// Probe whether the configured collaboration backend can be reached.
    async fn probe(&self, settings: CollaborationSettings<'_>) -> CollaborationStatus;
}

/// No backend wired: every probe reports that realtime collaboration is not
/// configured yet.
pub struct NoopCollaborationProvider;

#[async_trait]
impl CollaborationProvider for NoopCollaborationProvider {
    async fn probe(&self, _settings: CollaborationSettings<'_>) -> CollaborationStatus {
        CollaborationStatus {
            status: "not_configured".to_owned(),
            message: "Realtime collaboration is not wired in yet. The configuration is saved and will be used once \
                      the collaboration backend is available."
                .to_owned(),
        }
    }
}
