#![warn(clippy::disallowed_types)]

//! one-employee: digital employee definitions + run orchestration for the
//! 1ONE AionCore fork.
//!
//! Design doc: docs/tech/v2-m3-employee-design.md in the 1one-command repo.
//! Same own-crate policy as one-org: all state in `one_*` tables via our own
//! migration ledger; upstream touch points are workspace membership, the
//! route merge in aionui-app, and public upstream service APIs
//! (ConversationService::create / run_agent_turn).

pub mod error;
pub mod migrate;
pub mod models;
pub mod routes;
pub mod service;
pub mod state;
pub mod tenant;

pub use error::EmployeeError;
pub use migrate::run_one_employee_migrations;
pub use routes::one_employee_routes;
pub use service::{EmployeeService, RunReply};
pub use state::OneEmployeeRouterState;
pub use tenant::{DEFAULT_TENANT, TenantResolver};
