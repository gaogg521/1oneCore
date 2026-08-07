#![warn(clippy::disallowed_types)]

//! User-authored assistant management.
//!
//! Owns the `assistants` and `assistant_overrides` tables, built-in
//! assistant loading from on-disk manifest, and merge logic for
//! `GET /api/assistants` across builtin + user + extension sources.

pub mod agent_catalog;
pub mod builtin;
pub mod error;
pub mod marketplace;
pub mod routes;
pub mod service;
pub mod state;

pub use agent_catalog::AssistantAgentCatalogPort;
pub use builtin::{AvatarAsset, BuiltinAssistant, BuiltinAssistantRegistry};
pub use error::AssistantError;
pub use marketplace::{
    MarketplacePersona, load_marketplace_manifest, materialize_marketplace_personas,
    refresh_unedited_installed_personas, snapshot_catalog_rules,
};
pub use routes::{AssistantRouterState, assistant_routes};
pub use service::AssistantService;
