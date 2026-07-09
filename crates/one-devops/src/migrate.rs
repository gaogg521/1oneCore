//! Self-managed migration runner for one-devops tables.
//!
//! Same pattern as one-org: our own `_one_devops_migrations` ledger,
//! fully decoupled from the upstream sqlx migrator so upstream rebases
//! can never collide with our files.

use sqlx::SqlitePool;

use crate::error::DevopsError;

/// Embedded migrations, applied in array order. Append-only: never edit or
/// reorder shipped entries — add a new file instead.
const MIGRATIONS: &[(&str, &str)] = &[
    ("001_init", include_str!("../migrations/001_init.sql")),
    ("002_milestones", include_str!("../migrations/002_milestones.sql")),
    ("003_rag_pipeline", include_str!("../migrations/003_rag_pipeline.sql")),
    ("004_autopilot", include_str!("../migrations/004_autopilot.sql")),
    ("005_test_plans", include_str!("../migrations/005_test_plans.sql")),
    ("006_pipelines", include_str!("../migrations/006_pipelines.sql")),
    (
        "007_skill_auto_active",
        include_str!("../migrations/007_skill_auto_active.sql"),
    ),
];

/// Run all pending one-devops migrations. Idempotent; call once at startup
/// after the upstream database (and its migrator) has been initialized.
pub async fn run_one_devops_migrations(pool: &SqlitePool) -> Result<(), DevopsError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _one_devops_migrations (\
             name TEXT PRIMARY KEY,\
             applied_at INTEGER NOT NULL\
         )",
    )
    .execute(pool)
    .await?;

    for (name, sql) in MIGRATIONS {
        let applied: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM _one_devops_migrations WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await?;
        if applied {
            continue;
        }

        let mut tx = pool.begin().await?;
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _one_devops_migrations (name, applied_at) VALUES (?, ?)")
            .bind(name)
            .bind(aionui_common::now_ms())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(migration = name, "one-devops migration applied");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        run_one_devops_migrations(&pool).await.unwrap();
        run_one_devops_migrations(&pool).await.unwrap();

        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'one_%' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(tables.contains(&"one_requirements".to_owned()));
        assert!(tables.contains(&"one_requirement_comments".to_owned()));
        assert!(tables.contains(&"one_skill_registry".to_owned()));
        assert!(tables.contains(&"one_mcp_registry".to_owned()));
        assert!(tables.contains(&"one_rag_documents".to_owned()));
        assert!(tables.contains(&"one_milestones".to_owned()));
        assert!(tables.contains(&"one_rag_config".to_owned()));
        assert!(tables.contains(&"one_rag_chunks".to_owned()));
        assert!(tables.contains(&"one_test_plans".to_owned()));
        assert!(tables.contains(&"one_test_cases".to_owned()));
        assert!(tables.contains(&"one_pipelines".to_owned()));
        assert!(tables.contains(&"one_pipeline_runs".to_owned()));
    }
}
