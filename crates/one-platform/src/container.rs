//! Container runtime seam (P1-3 reserved framework).
//!
//! No container client (Docker / Kubernetes / Podman) is wired in here — this
//! is the "reserved adapter" pattern used across the app (SMTP `EmailSender`,
//! `IntegrationProvider`, billing `BillingProvider`): a config store (see
//! `PlatformService::{get,set}_container_config`) plus a pluggable
//! `ContainerRuntime` trait. `NoopContainerRuntime` is the default and reports
//! "not configured"; a real runtime can be dropped in at the app layer via
//! `PlatformService::with_container_runtime` without touching this crate.

use async_trait::async_trait;

/// Outcome of a container-runtime probe, shaped like `one_org`'s
/// `IntegrationTestResult` for the same "not configured yet" UX.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatus {
    /// `"not_configured"` (stub), `"ok"` (runtime reachable), or `"error"`.
    pub status: String,
    pub message: String,
}

/// Non-secret + secret container config handed to a runtime for a live probe.
/// `registry_secret` is already decrypted; never log it.
pub struct ContainerSettings<'a> {
    pub runtime_kind: Option<&'a str>,
    pub endpoint: Option<&'a str>,
    pub default_image: Option<&'a str>,
    pub registry: Option<&'a str>,
    pub registry_secret: Option<&'a str>,
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Probe whether the configured runtime can be reached / is usable.
    async fn probe(&self, settings: ContainerSettings<'_>) -> ContainerStatus;
}

/// No runtime wired: every probe reports that containerized execution is not
/// configured yet, so the admin UI can surface a clear "saved, but not live
/// yet" message instead of implying containers are running.
pub struct NoopContainerRuntime;

#[async_trait]
impl ContainerRuntime for NoopContainerRuntime {
    async fn probe(&self, _settings: ContainerSettings<'_>) -> ContainerStatus {
        ContainerStatus {
            status: "not_configured".to_owned(),
            message: "Container runtime is not wired in yet. The configuration is saved and will be used once \
                      containerized execution is available."
                .to_owned(),
        }
    }
}
