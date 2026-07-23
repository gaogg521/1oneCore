use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::ClaudeBridgeConfig;
use crate::repository::IClaudeBridgeConfigRepository;

/// SQLite-backed implementation of [`IClaudeBridgeConfigRepository`].
#[derive(Clone, Debug)]
pub struct SqliteClaudeBridgeConfigRepository {
    pool: SqlitePool,
}

impl SqliteClaudeBridgeConfigRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IClaudeBridgeConfigRepository for SqliteClaudeBridgeConfigRepository {
    async fn get(&self) -> Result<Option<ClaudeBridgeConfig>, DbError> {
        let row = sqlx::query_as::<_, ClaudeBridgeConfig>("SELECT * FROM claude_bridge_config WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn upsert(
        &self,
        enabled: bool,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ClaudeBridgeConfig, DbError> {
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO claude_bridge_config \
                (id, enabled, provider_id, model, created_at, updated_at) \
             VALUES (1, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                enabled = excluded.enabled, \
                provider_id = excluded.provider_id, \
                model = excluded.model, \
                updated_at = excluded.updated_at",
        )
        .bind(enabled)
        .bind(provider_id)
        .bind(model)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(ClaudeBridgeConfig {
            id: 1,
            enabled,
            provider_id: provider_id.map(str::to_owned),
            model: model.map(str::to_owned),
            created_at: now,
            updated_at: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteClaudeBridgeConfigRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteClaudeBridgeConfigRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn get_returns_none_when_empty() {
        let (repo, _db) = setup().await;
        assert!(repo.get().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_creates_config() {
        let (repo, _db) = setup().await;
        let cfg = repo.upsert(true, Some("prov-1"), Some("kimi-k3")).await.unwrap();

        assert_eq!(cfg.id, 1);
        assert!(cfg.enabled);
        assert_eq!(cfg.provider_id.as_deref(), Some("prov-1"));
        assert_eq!(cfg.model.as_deref(), Some("kimi-k3"));
        assert!(cfg.updated_at > 0);
    }

    #[tokio::test]
    async fn upsert_then_get_returns_same() {
        let (repo, _db) = setup().await;
        repo.upsert(true, Some("prov-2"), Some("glm-5-2")).await.unwrap();

        let cfg = repo.get().await.unwrap().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.provider_id.as_deref(), Some("prov-2"));
        assert_eq!(cfg.model.as_deref(), Some("glm-5-2"));
    }

    #[tokio::test]
    async fn upsert_overwrites_existing() {
        let (repo, _db) = setup().await;
        repo.upsert(true, Some("prov-1"), Some("kimi-k3")).await.unwrap();
        let cfg = repo.upsert(false, Some("prov-2"), Some("glm-5-2")).await.unwrap();

        assert!(!cfg.enabled);
        assert_eq!(cfg.provider_id.as_deref(), Some("prov-2"));
        assert_eq!(cfg.model.as_deref(), Some("glm-5-2"));

        let fetched = repo.get().await.unwrap().unwrap();
        assert_eq!(fetched.provider_id.as_deref(), Some("prov-2"));
        assert!(!fetched.enabled);
    }

    #[tokio::test]
    async fn upsert_allows_clearing_provider_and_model() {
        let (repo, _db) = setup().await;
        repo.upsert(true, Some("prov-1"), Some("kimi-k3")).await.unwrap();
        let cfg = repo.upsert(false, None, None).await.unwrap();

        assert!(cfg.provider_id.is_none());
        assert!(cfg.model.is_none());
    }
}
