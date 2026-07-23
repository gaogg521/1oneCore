use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `codex_bridge_config` table.
///
/// Single-row table (id is always 1). Selects which saved
/// [`crate::models::Provider`] and model the local Codex-compatibility
/// bridge forwards `/v1/responses` requests to. `bearer_token` gates the
/// endpoint since it isn't protected by the app's session-cookie auth
/// (Codex is an external process, not a logged-in browser session).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CodexBridgeConfig {
    pub id: i64,
    pub enabled: bool,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub bearer_token: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
