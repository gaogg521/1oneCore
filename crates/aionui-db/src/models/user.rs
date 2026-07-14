use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `users` table.
///
/// All fields match the SQLite column names and types exactly.
/// Optional fields correspond to nullable columns.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub avatar_path: Option<String>,
    pub jwt_secret: Option<String>,
    /// Data-at-rest encryption secret, independent of `jwt_secret`.
    ///
    /// Used to derive the AES key for provider/team/mcp/channel API keys.
    /// Unlike `jwt_secret`, it is never rotated by auth/session-invalidation
    /// flows, so those flows no longer orphan encrypted data. Nullable for
    /// rows created before migration 025; backfilled at boot.
    pub data_secret: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub last_login: Option<TimestampMs>,
}
