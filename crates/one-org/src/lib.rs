#![warn(clippy::disallowed_types)]

//! one-org: enterprise tenant / membership / invites / RBAC for the
//! 1ONE AionCore fork.
//!
//! Own-crate policy (see docs/tech/v2-m2-enterprise-crate-design.md in the
//! 1one-command repo): all enterprise state lives in `one_*` tables managed
//! by our own migrator; the only upstream touch points are workspace
//! membership, a route merge in aionui-app, and read-only use of public
//! aionui-auth / aionui-db APIs.

pub mod backup;
pub mod bridge;
pub mod credential_revoker;
pub mod directory_bridge;
pub mod email;
pub mod enterprise_hooks;
pub mod error;
pub mod integration;
pub mod migrate;
pub mod models;
pub mod rbac;
pub mod routes;
pub mod service;
pub mod state;

pub use bridge::CompanyAdminResolver;
pub use credential_revoker::{CredentialRevoker, NoopCredentialRevoker};
pub use directory_bridge::{DirectoryDepartmentRef, DirectoryTreeSource, NoopDirectoryTreeSource};
pub use email::{EmailSender, SendEmailResult, StubEmailSender};
pub use enterprise_hooks::CompanySeatSync;
pub use error::OrgError;
pub use integration::{
    IntegrationCredentials, IntegrationProvider, IntegrationTestResult, KNOWN_PROVIDERS, StubIntegrationProvider,
};
pub use migrate::run_one_migrations;
pub use rbac::{OrgActor, RequireOrgAdmin, RequireSystemAdmin};
pub use routes::one_org_routes;
pub use service::OrgService;
pub use state::OneOrgRouterState;
