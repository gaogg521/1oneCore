//! Self-managed migration runner for one-org tables.
//!
//! Deliberately independent from the upstream `sqlx::migrate!()` pipeline:
//! upstream tracks its files in `_sqlx_migrations` with sequential version
//! numbers, and mixing our files into that directory would make every
//! upstream rebase a potential out-of-order failure. We keep our own
//! `_one_migrations` ledger instead (name-keyed, append-only).

use sqlx::SqlitePool;

use crate::error::OrgError;

/// Embedded migrations, applied in array order. Append-only: never edit or
/// reorder shipped entries — add a new file instead.
const MIGRATIONS: &[(&str, &str)] = &[
    ("001_init", include_str!("../migrations/001_init.sql")),
    (
        "002_membership_display",
        include_str!("../migrations/002_membership_display.sql"),
    ),
    (
        "003_membership_job_title",
        include_str!("../migrations/003_membership_job_title.sql"),
    ),
    (
        "004_tenant_sso_binding",
        include_str!("../migrations/004_tenant_sso_binding.sql"),
    ),
    (
        "005_drop_tenant_sso_binding",
        include_str!("../migrations/005_drop_tenant_sso_binding.sql"),
    ),
    (
        "006_tenant_enterprise_link",
        include_str!("../migrations/006_tenant_enterprise_link.sql"),
    ),
    (
        "007_multi_membership",
        include_str!("../migrations/007_multi_membership.sql"),
    ),
    ("008_onboarding", include_str!("../migrations/008_onboarding.sql")),
    ("009_departments", include_str!("../migrations/009_departments.sql")),
    ("010_integrations", include_str!("../migrations/010_integrations.sql")),
    (
        "011_department_directory_source",
        include_str!("../migrations/011_department_directory_source.sql"),
    ),
    (
        "012_department_directory_map_root",
        include_str!("../migrations/012_department_directory_map_root.sql"),
    ),
];

/// Run all pending one-org migrations. Idempotent; call once at startup
/// after the upstream database (and its migrator) has been initialized.
pub async fn run_one_migrations(pool: &SqlitePool) -> Result<(), OrgError> {
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
        tracing::info!(migration = name, "one-org migration applied");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = aionui_db::init_database_memory().await.unwrap();
        run_one_migrations(db.pool()).await.unwrap();
        run_one_migrations(db.pool()).await.unwrap();

        let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _one_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64);

        for table in [
            "one_tenants",
            "one_tenant_invites",
            "one_user_org",
            "one_active_tenant",
            "one_runtime_nodes",
            "one_audit_logs",
        ] {
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
