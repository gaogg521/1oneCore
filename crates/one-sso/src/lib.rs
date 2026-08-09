//! one-sso: SSO providers (Feishu / DingTalk / WeCom / LDAP) + OAuth
//! callbacks + JIT user provisioning for the 1ONE AionCore fork.
//!
//! Design doc: docs/tech/v2-m2-enterprise-crate-design.md in the 1one-command
//! repo. Same own-crate policy as one-org/one-employee: all state in `one_*`
//! tables via our own migration ledger; upstream touch points are the route
//! merge in aionui-app and public upstream service APIs (IUserRepository,
//! JwtSecret, hash_password).

pub mod directory;
pub mod enterprise;
pub mod error;
pub mod migrate;
pub mod models;
pub mod org_hooks;
pub mod providers;
pub mod rbac;
pub mod routes;
pub mod service;
pub mod state;

pub use enterprise::{
    CompanyAdminCheck, DirectoryDepartmentPayload, DirectoryPersonPayload, DirectorySink, DirectorySnapshotPayload,
    EnterpriseSync,
};
pub use error::SsoError;
pub use migrate::run_one_sso_migrations;
pub use org_hooks::OrgAutoJoin;
pub use routes::{one_sso_admin_routes, one_sso_public_routes};
pub use service::SsoService;
pub use state::OneSsoRouterState;
