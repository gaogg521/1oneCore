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
//! RAG: `one_rag_documents` + `one_rag_chunks` + `one_rag_config` back a
//! chunk → embed → store → retrieve pipeline over an OpenAI-compatible
//! embedding endpoint (see `embedding` module).
//!
//! Retrieval is hybrid (see `retrieval`): the dense vector ranking is fused
//! with a BM25 ranking from SQLite's built-in FTS5. The lexical index is a
//! derived structure that can be rebuilt from `one_rag_chunks` at any time,
//! which is what makes the startup backfill and any re-index safe.

pub mod breakdown;
pub mod dlp_service;
pub mod embedding;
pub mod error;
pub mod migrate;
pub mod model_proxy;
pub mod models;
pub mod provider_channel;
pub mod retrieval;
pub mod routes;
pub mod service;
pub mod state;

pub use error::DevopsError;
pub use migrate::run_one_devops_migrations;
pub use model_proxy::model_proxy_routes;
pub use routes::one_devops_routes;
pub use service::DevopsService;
pub use state::OneDevopsRouterState;
