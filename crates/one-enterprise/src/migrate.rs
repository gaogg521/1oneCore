//! Self-managed migration runner for one-enterprise tables.
//!
//! Shares the `_one_migrations` ledger with one-org / one-sso / one-employee
//! but keys its entries under the `enterprise_` prefix so names never collide
//! with another crate's `001_init`. Append-only: never edit or reorder shipped
//! entries — add a new file instead.

use sqlx::SqlitePool;

use crate::error::EnterpriseError;

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "enterprise_001_init",
        include_str!("../migrations/enterprise_001_init.sql"),
    ),
    (
        "enterprise_002_company_origin",
        include_str!("../migrations/enterprise_002_company_origin.sql"),
    ),
];

/// Run all pending one-enterprise migrations. Idempotent; call once at startup
/// after the upstream database has been initialized.
pub async fn run_one_enterprise_migrations(pool: &SqlitePool) -> Result<(), EnterpriseError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _one_migrations (\
             name TEXT PRIMARY KEY,\
             applied_at INTEGER NOT NULL\
         )",
    )
    .execute(pool)
    .await?;

    for (name, sql) in MIGRATIONS {
        let applied: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM _one_migrations WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await?;
        if applied {
            continue;
        }

        let mut tx = pool.begin().await?;
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _one_migrations (name, applied_at) VALUES (?, ?)")
            .bind(name)
            .bind(aionui_common::now_ms())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(migration = name, "one-enterprise migration applied");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = aionui_db::init_database_memory().await.unwrap();
        run_one_enterprise_migrations(db.pool()).await.unwrap();
        run_one_enterprise_migrations(db.pool()).await.unwrap();

        for table in ["one_enterprises", "one_enterprise_members"] {
            let exists: bool =
                sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?")
                    .bind(table)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert!(exists, "table {table} should exist");
        }
    }
}
