#![warn(clippy::disallowed_types)]

//! one-enterprise: the SSO-company / "enterprise org" dimension for the 1ONE
//! AionCore fork — a user's real company (Feishu tenant_key etc.) + department
//! / job title / membership.
//!
//! Kept **independent of one-org's project-group tenants**: enterprise
//! membership and invite-code project groups are orthogonal, so each owns its
//! own tables. Populated at SSO login via a sync hook wired in aionui-app.
//! Own-crate policy mirrors one-org / one-sso: all state in `one_*` tables via
//! our own migration ledger (prefix `enterprise_`), upstream touch points are
//! only the route merge in aionui-app and public aionui-auth / aionui-db APIs.

pub mod directory;
pub mod disband_cascade;
pub mod error;
pub mod migrate;
pub mod models;
pub mod rbac;
pub mod routes;
pub mod service;
pub mod session_revoker;
pub mod state;

pub use disband_cascade::{CompanyDisbandCascade, NoopCompanyDisbandCascade};
pub use error::EnterpriseError;
pub use migrate::run_one_enterprise_migrations;
pub use models::{CompanyMemberDto, CompanyOverviewDto, DisbandCompanyResult, EnterpriseIdentityDto};
pub use routes::one_enterprise_routes;
pub use service::EnterpriseService;
pub use session_revoker::{NoopSessionRevoker, SessionRevoker};
pub use state::OneEnterpriseRouterState;
