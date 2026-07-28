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
                    (id, source, name, description, rule_content, display_name, role_name, category, has_avatar, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET \
                    source = excluded.source, \
                    name = excluded.name, \
                    description = excluded.description, \
                    rule_content = excluded.rule_content, \
                    display_name = excluded.display_name, \
                    role_name = excluded.role_name, \
                    category = excluded.category, \
                    has_avatar = excluded.has_avatar, \
                    updated_at = excluded.updated_at",
            )
            .bind(params.id)
            .bind(params.source)
            .bind(params.name)
            .bind(params.description)
            .bind(params.rule_content)
            .bind(params.display_name)
            .bind(params.role_name)
            .bind(params.category)
            .bind(params.has_avatar)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_missing(&self, keep_ids: &[&str]) -> Result<u64, DbError> {
        if keep_ids.is_empty() {
            let result = sqlx::query("DELETE FROM assistant_marketplace_personas")
                .execute(&self.pool)
                .await?;
            return Ok(result.rows_affected());
        }

        let placeholders = std::iter::repeat_n("?", keep_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM assistant_marketplace_personas WHERE id NOT IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in keep_ids {
            q = q.bind(*id);
        }
        let result = q.execute(&self.pool).await?;
        Ok(result.rows_affected())
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
                display_name: Some("A股顾问"),
                role_name: Some("A股顾问"),
                category: Some("金融投资"),
                has_avatar: true,
            },
            UpsertMarketplacePersonaParams {
                id: "backend-architect",
                source: "workbuddy",
                name: "Backend Architect",
                description: None,
                rule_content: "You design backend systems.",
                display_name: None,
                role_name: None,
                category: None,
                has_avatar: false,
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
            display_name: Some("A股顾问"),
            role_name: None,
            category: None,
            has_avatar: false,
        }])
        .await
        .unwrap();

        repo.upsert_many(&[UpsertMarketplacePersonaParams {
            id: "a-share-advisor",
            source: "workbuddy",
            name: "A Share Advisor v2",
            description: Some("v2"),
            rule_content: "v2 prompt",
            display_name: Some("A股顾问v2"),
            role_name: None,
            category: None,
            has_avatar: true,
        }])
        .await
        .unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1, "re-materializing the catalog must overwrite, not duplicate");
        assert_eq!(listed[0].name, "A Share Advisor v2");
        assert_eq!(listed[0].rule_content, "v2 prompt");
    }

    fn params<'a>(id: &'a str, name: &'a str) -> UpsertMarketplacePersonaParams<'a> {
        UpsertMarketplacePersonaParams {
            id,
            source: "workbuddy",
            name,
            description: None,
            rule_content: "prompt",
            display_name: None,
            role_name: None,
            category: None,
            has_avatar: false,
        }
    }

    #[tokio::test]
    async fn delete_missing_removes_only_ids_not_in_keep_list() {
        let repo = fixture().await;
        repo.upsert_many(&[params("a", "A"), params("b", "B"), params("c", "C")])
            .await
            .unwrap();

        let deleted = repo.delete_missing(&["a", "c"]).await.unwrap();
        assert_eq!(deleted, 1);

        let mut ids: Vec<String> = repo.list().await.unwrap().into_iter().map(|r| r.id).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn delete_missing_with_empty_keep_list_clears_the_table() {
        let repo = fixture().await;
        repo.upsert_many(&[params("a", "A"), params("b", "B")]).await.unwrap();

        let deleted = repo.delete_missing(&[]).await.unwrap();
        assert_eq!(deleted, 2);
        assert!(repo.list().await.unwrap().is_empty());
    }
}
