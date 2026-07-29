#![warn(clippy::disallowed_types)]

//! one-billing: the commercialization "billing plane" for the 1ONE AionCore
//! fork — subscription tier, seat cap enforcement, and per-turn usage metering,
//! plus a stubbed payment-provider seam (`BillingProvider`) so real payment can
//! drop in later without touching callers.
//!
//! License attaches to an SSO company (one-enterprise's `one_enterprises`).
//! Personal / standalone users have no company and are **outside** this system:
//! every entitlement check is permissive, seats are uncounted, and usage is
//! recorded with a NULL enterprise. Own-crate policy mirrors the other `one-*`
//! crates: all state in `one_*` tables via our own migration ledger (prefix
//! `billing_`); the only upstream touch points are the route merge + trait
//! adapters wired in aionui-app.

pub mod error;
pub mod license_key;
pub mod migrate;
pub mod models;
pub mod routes;
pub mod service;
pub mod state;

pub use error::BillingError;
pub use license_key::{LicenseKeyError, LicensePayload, verify_license_key};
pub use migrate::run_one_billing_migrations;
pub use models::{CheckoutResultDto, PlanDto, UsageSummaryDto};
pub use routes::one_billing_routes;
pub use service::{BillingProvider, BillingService, ManualBillingProvider};
pub use state::OneBillingRouterState;
