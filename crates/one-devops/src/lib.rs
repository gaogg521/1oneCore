#![warn(clippy::disallowed_types)]

//! one-devops: requirements board (issues) + enterprise collaboration
//! registries (skills / MCP / RAG document metadata) for the 1ONE AionCore
//! fork.
//!
//! Rebuild of the 1ONE ClaudeCode DevOps slice that backs the
//! superAssistant `IssuesWorkbench` and `EnterpriseCollaborationContext`
//! panels. Same own-crate policy as one-org: all state lives in `one_*`
//! tables managed by our own migrator; the only upstream touch point is a
//! route merge in aionui-app.
//!
//! Scope note: `one_rag_documents` is a metadata registry only — the
//! chunking / embedding / search pipeline is a separate work item (it
//! needs an embedding-provider decision first).

pub mod error;
pub mod migrate;
pub mod models;
pub mod routes;
pub mod service;
pub mod state;

pub use error::DevopsError;
pub use migrate::run_one_devops_migrations;
pub use routes::one_devops_routes;
pub use service::DevopsService;
pub use state::OneDevopsRouterState;
