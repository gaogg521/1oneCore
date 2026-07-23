use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `claude_bridge_config` table.
///
/// Single-row table (id is always 1). Selects which saved
/// [`crate::models::Provider`] and model the built-in Claude Code ACP agent
/// is launched against, in place of Anthropic's own account/API key.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ClaudeBridgeConfig {
    pub id: i64,
    pub enabled: bool,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
