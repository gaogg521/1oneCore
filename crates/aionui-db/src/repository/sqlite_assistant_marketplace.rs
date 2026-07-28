//! SQLite-backed implementation of [`IAssistantMarketplaceRepository`].

use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{MarketplacePersonaRow, UpsertMarketplacePersonaParams};
use crate::repository::assistant_marketplace::IAssistantMarketplaceRepository;

#[derive(Clone, Debug)]
pub struct SqliteAssistantMarketplaceRepository {
    pool: SqlitePool,
}

impl SqliteAssistantMarketplaceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAssistantMarketplaceRepository for SqliteAssistantMarketplaceRepository {
    async fn list(&self) -> Result<Vec<MarketplacePersonaRow>, DbError> {
        let rows =
            sqlx::query_as::<_, MarketplacePersonaRow>("SELECT * FROM assistant_marketplace_personas ORDER BY name ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn get(&self, id: &str) -> Result<Option<MarketplacePersonaRow>, DbError> {
        let row = sqlx::query_as::<_, MarketplacePersonaRow>("SELECT * FROM assistant_marketplace_personas WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn upsert_many(&self, entries: &[UpsertMarketplacePersonaParams<'_>]) -> Result<(), DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        for params in entries {
            sqlx::query(
                "INSERT INTO assistant_marketplace_personas \
                    (id, source, name, description, rule_content, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET \
                    source = excluded.source, \
                    name = excluded.name, \
                    description = excluded.description, \
                    rule_content = excluded.rule_content, \
                    updated_at = excluded.updated_at",
            )
            .bind(params.id)
            .bind(params.source)
            .bind(params.name)
            .bind(params.description)
            .bind(params.rule_content)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn fixture() -> SqliteAssistantMarketplaceRepository {
        let db = init_database_memory().await.unwrap();
        SqliteAssistantMarketplaceRepository::new(db.pool().clone())
    }

    #[tokio::test]
    async fn upsert_many_then_list_and_get_round_trip() {
        let repo = fixture().await;
        repo.upsert_many(&[
            UpsertMarketplacePersonaParams {
                id: "a-share-advisor",
                source: "workbuddy",
                name: "A Share Advisor",
                description: Some("An investment persona"),
                rule_content: "You are an A-share advisor.",
            },
            UpsertMarketplacePersonaParams {
                id: "backend-architect",
                source: "workbuddy",
                name: "Backend Architect",
                description: None,
                rule_content: "You design backend systems.",
            },
        ])
        .await
        .unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "A Share Advisor");

        let fetched = repo.get("backend-architect").await.unwrap().expect("row should exist");
        assert_eq!(fetched.rule_content, "You design backend systems.");
        assert_eq!(fetched.description, None);

        assert!(repo.get("does-not-exist").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_many_is_idempotent_and_overwrites() {
        let repo = fixture().await;
        repo.upsert_many(&[UpsertMarketplacePersonaParams {
            id: "a-share-advisor",
            source: "workbuddy",
            name: "A Share Advisor",
            description: Some("v1"),
            rule_content: "v1 prompt",
        }])
        .await
        .unwrap();

        repo.upsert_many(&[UpsertMarketplacePersonaParams {
            id: "a-share-advisor",
            source: "workbuddy",
            name: "A Share Advisor v2",
            description: Some("v2"),
            rule_content: "v2 prompt",
        }])
        .await
        .unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1, "re-materializing the catalog must overwrite, not duplicate");
        assert_eq!(listed[0].name, "A Share Advisor v2");
        assert_eq!(listed[0].rule_content, "v2 prompt");
    }
}
