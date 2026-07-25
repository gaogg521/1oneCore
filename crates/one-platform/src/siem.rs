//! SIEM audit-log export seam (P1-4 reserved framework).
//!
//! No exporter (Splunk HEC / syslog / HTTP client) is wired in here — reserved
//! adapter pattern: a config store (see `PlatformService::{get,set}_siem_config`)
//! plus a pluggable `SiemExporter` trait. `NoopSiemExporter` is the default and
//! reports "not configured"; a real exporter is dropped in at the app layer via
//! `PlatformService::with_siem_exporter`.

use async_trait::async_trait;

/// Outcome of a SIEM export/probe.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiemStatus {
    /// `"not_configured"` (stub), `"ok"` (endpoint reachable), or `"error"`.
    pub status: String,
    pub message: String,
}

/// Non-secret + secret SIEM config handed to an exporter for a probe.
/// `secret` is already decrypted; never log it.
pub struct SiemSettings<'a> {
    pub kind: Option<&'a str>,
    pub endpoint: Option<&'a str>,
    pub secret: Option<&'a str>,
}

#[async_trait]
pub trait SiemExporter: Send + Sync {
    /// Probe whether the configured SIEM endpoint can be reached.
    async fn probe(&self, settings: SiemSettings<'_>) -> SiemStatus;
}

/// No exporter wired: every probe reports that SIEM export is not configured.
pub struct NoopSiemExporter;

#[async_trait]
impl SiemExporter for NoopSiemExporter {
    async fn probe(&self, _settings: SiemSettings<'_>) -> SiemStatus {
        SiemStatus {
            status: "not_configured".to_owned(),
            message: "SIEM export is not wired in yet. The configuration is saved and will be used once log \
                      forwarding is available."
                .to_owned(),
        }
    }
}
