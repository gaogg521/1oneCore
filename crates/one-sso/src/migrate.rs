//! Migration runner for one-sso tables.
//!
//! Shares the `_one_migrations` ledger with one-org/one-employee; entry
//! names carry the `sso_` prefix so the key spaces stay disjoint.

use sqlx::SqlitePool;

use crate::error::SsoError;

const MIGRATIONS: &[(&str, &str)] = &[
    ("sso_001_init", include_str!("../migrations/001_init.sql")),
    (
        "sso_002_identity_display",
        include_str!("../migrations/002_identity_display.sql"),
    ),
];

/// Run all pending one-sso migrations. Idempotent.
pub async fn run_one_sso_migrations(pool: &SqlitePool) -> Result<(), SsoError> {
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
        tracing::info!(migration = name, "one-sso migration applied");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = aionui_db::init_database_memory().await.unwrap();
        run_one_sso_migrations(db.pool()).await.unwrap();
        run_one_sso_migrations(db.pool()).await.unwrap();

        for table in ["one_sso_providers", "one_sso_identities"] {
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
