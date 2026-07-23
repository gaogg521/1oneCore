use crate::error::DbError;
use crate::models::CodexBridgeConfig;

/// Codex-compatibility bridge config data access abstraction.
///
/// The `codex_bridge_config` table holds a single row (id=1).
/// `get` returns `None` if no row exists yet (caller treats the bridge as
/// disabled). `upsert` inserts or replaces the single row.
#[async_trait::async_trait]
pub trait ICodexBridgeConfigRepository: Send + Sync {
    /// Returns the config row, or `None` if never configured.
    async fn get(&self) -> Result<Option<CodexBridgeConfig>, DbError>;

    /// Inserts or replaces the single config row. `bearer_token` is generated
    /// by the caller on first setup and preserved on subsequent updates.
    async fn upsert(
        &self,
        enabled: bool,
        provider_id: Option<&str>,
        model: Option<&str>,
        bearer_token: &str,
    ) -> Result<CodexBridgeConfig, DbError>;
}
