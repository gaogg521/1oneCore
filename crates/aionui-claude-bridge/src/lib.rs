//! Settings-only backing for the Claude Code custom-provider bridge.
//!
//! Unlike the Codex bridge, this crate has no protocol-translation surface:
//! Claude Code already speaks the Anthropic Messages wire protocol, and the
//! app's `litellm-internal` gateway already exposes a compatible Anthropic
//! surface, so no local proxy is needed. This crate only owns the
//! settings CRUD (`GET`/`PUT /api/claude-bridge/config`); the actual
//! `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL` resolution
//! happens directly in `aionui-ai-agent`'s launch policy (see
//! `crates/aionui-ai-agent/src/factory/acp.rs::resolve_claude_bridge_env`),
//! which needs the decryption key and provider repository this crate
//! deliberately does not depend on.

mod error;
mod routes;
mod service;
mod state;

pub use error::ClaudeBridgeError;
pub use routes::claude_bridge_config_routes;
pub use service::ClaudeBridgeService;
pub use state::ClaudeBridgeRouterState;
