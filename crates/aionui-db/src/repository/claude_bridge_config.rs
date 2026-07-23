use crate::error::DbError;
use crate::models::ClaudeBridgeConfig;

/// Claude Code custom-provider bridge config data access abstraction.
///
/// The `claude_bridge_config` table holds a single row (id=1).
/// `get` returns `None` if no row exists yet (caller treats the bridge as
/// disabled). `upsert` inserts or replaces the single row.
#[async_trait::async_trait]
pub trait IClaudeBridgeConfigRepository: Send + Sync {
    /// Returns the config row, or `None` if never configured.
    async fn get(&self) -> Result<Option<ClaudeBridgeConfig>, DbError>;

    /// Inserts or replaces the single config row.
    async fn upsert(
        &self,
        enabled: bool,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ClaudeBridgeConfig, DbError>;
}
