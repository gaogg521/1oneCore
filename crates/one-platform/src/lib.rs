//! one-platform: deployment/platform infrastructure config for the 1ONE
//! AionCore fork — reserved adapters for containerized execution (P1-3) and
//! realtime collaboration (P2-2).
//!
//! Both are "reserved framework" layers: an admin-managed, per-project-group
//! config store plus a pluggable trait (`ContainerRuntime` /
//! `CollaborationProvider`) defaulting to a Noop that reports "not configured".
//! A real implementation is dropped in at the app layer via
//! `PlatformService::with_*` — no schema change needed. Kept out of one-org so
//! that crate stays focused on membership/RBAC.

pub mod collaboration;
pub mod container;
pub mod error;
pub mod ip_allowlist;
pub mod migrate;
pub mod models;
pub mod rbac;
pub mod routes;
pub mod service;
pub mod siem;
pub mod state;

pub use collaboration::{CollaborationProvider, CollaborationSettings, CollaborationStatus, NoopCollaborationProvider};
pub use container::{ContainerRuntime, ContainerSettings, ContainerStatus, NoopContainerRuntime};
pub use error::PlatformError;
pub use migrate::run_one_platform_migrations;
pub use rbac::RequirePlatformAdmin;
pub use routes::one_platform_routes;
pub use service::{PlatformActor, PlatformService};
pub use siem::{NoopSiemExporter, SiemExporter, SiemSettings, SiemStatus};
pub use state::OnePlatformRouterState;
