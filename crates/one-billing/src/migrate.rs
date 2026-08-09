//! Self-managed migration runner for one-billing tables.
//!
//! Shares the `_one_migrations` ledger with one-org / one-sso / one-enterprise
//! but keys entries under the `billing_` prefix so names never collide.
//! Append-only. MUST run after one-enterprise migrations — `billing_001_init`
//! grandfathers existing `one_enterprises` rows.

use sqlx::SqlitePool;

use crate::error::BillingError;

const MIGRATIONS: &[(&str, &str)] = &[
    ("billing_001_init", include_str!("../migrations/billing_001_init.sql")),
    (
        "billing_002_model_control",
        include_str!("../migrations/billing_002_model_control.sql"),
    ),
    (
        "billing_003_license_activation",
        include_str!("../migrations/billing_003_license_activation.sql"),
    ),
];

/// Run all pending one-billing migrations. Idempotent; call once at startup
/// after the upstream database AND one-enterprise migrations have run.
pub async fn run_one_billing_migrations(pool: &SqlitePool) -> Result<(), BillingError> {
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
        tracing::info!(migration = name, "one-billing migration applied");
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = aionui_db::init_database_memory().await.unwrap();
        // one-enterprise tables must exist first (grandfather SELECT).
        one_enterprise_tables(db.pool()).await;
        run_one_billing_migrations(db.pool()).await.unwrap();
        run_one_billing_migrations(db.pool()).await.unwrap();

        for table in ["one_enterprise_license", "one_usage_events"] {
            let exists: bool =
                sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?")
                    .bind(table)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert!(exists, "table {table} should exist");
        }
    }

    /// Minimal `one_enterprises` shape for tests that don't pull in the
    /// one-enterprise crate. `seat_status` mirrors `enterprise_004_seat_status`
    /// (T6-4) — must stay in sync with that migration, or a test member row
    /// inserted without the column would silently read back as NULL and get
    /// mistaken for a pending (unseated) member.
    pub(crate) async fn one_enterprise_tables(pool: &SqlitePool) {
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS one_enterprises (id TEXT PRIMARY KEY, provider TEXT, external_id TEXT, display_name TEXT, created_at INTEGER, updated_at INTEGER);
             CREATE TABLE IF NOT EXISTS one_enterprise_members (user_id TEXT PRIMARY KEY, enterprise_id TEXT NOT NULL, display_name TEXT, department TEXT, job_title TEXT, role TEXT NOT NULL DEFAULT 'member', seat_status TEXT NOT NULL DEFAULT 'active', joined_at INTEGER, updated_at INTEGER);",
        )
        .execute(pool)
        .await
        .unwrap();
    }
}
