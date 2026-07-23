//! Local OpenAI Responses API compatibility bridge.
//!
//! Codex CLI's `model_providers` config only supports `wire_api = "responses"`
//! (recent Codex releases dropped `"chat"` support), and pointing it directly
//! at a Chat-Completions-shaped gateway therefore requires a translation
//! layer regardless of gateway compatibility quirks. This crate provides
//! that layer: it exposes a local-only `POST /v1/responses` endpoint that
//! Codex can be pointed at, and internally forwards through
//! `aion_providers::create_provider` — the same hardened Chat Completions
//! transport the built-in agent uses — so Codex gets the user's own
//! configured model (no OpenAI subscription needed) with all of that
//! transport's gateway-compat fixes (thinking-replay escalation, tool-call
//! history textualization, truncation recovery) applied for free.
//!
//! See `crates/aionui-ai-agent/src/factory/acp_launch_policy.rs` for how
//! Codex is pointed at this bridge at launch time, mirroring the existing
//! `cc_switch` pattern used for Claude Code.

mod encoder;
mod error;
mod protocol;
mod routes;
mod service;
mod state;

pub use error::BridgeError;
pub use protocol::{InputItem, ResponsesRequest};
pub use routes::{codex_bridge_config_routes, codex_bridge_public_routes};
pub use service::{CodexBridgeService, ResponsesOutcome};
pub use state::CodexBridgeRouterState;
