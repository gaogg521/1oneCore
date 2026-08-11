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
    ("008_mcp_secrets", include_str!("../migrations/008_mcp_secrets.sql")),
    (
        "009_resource_visibility",
        include_str!("../migrations/009_resource_visibility.sql"),
    ),
    (
        "010_provider_registry",
        include_str!("../migrations/010_provider_registry.sql"),
    ),
    ("011_dlp", include_str!("../migrations/011_dlp.sql")),
    (
        "012_collaboration_tenant_scope",
        include_str!("../migrations/012_collaboration_tenant_scope.sql"),
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

    backfill_collaboration_tenant_ids(pool).await?;

    Ok(())
}

/// Backfill `tenant_id` on rows still at the 012 migration's 'default'
/// sentinel, from the creator's current `one_user_org` membership (or, for
/// rows with no reliable creator of their own, from their parent row's
/// already-backfilled tenant_id). Not part of the 012 migration file itself
/// — see that file's header comment for why — and deliberately NOT
/// ledger-gated: it only ever touches 'default' rows, so re-running it every
/// boot is a no-op once a row has a real tenant_id, and it self-heals if an
/// earlier boot ran before one-org's tables existed.
///
/// `one_user_org` belongs to one-org, not this crate. In the real app
/// one-org's migrations always run first (see
/// `aionui-app/router/routes.rs`), but this crate's own tests exercise
/// `run_one_devops_migrations` in isolation against a pool that never has
/// it — checking existence first, rather than letting the query fail, keeps
/// that isolation test meaningful instead of forcing it to know about
/// another crate's schema.
async fn backfill_collaboration_tenant_ids(pool: &SqlitePool) -> Result<(), DevopsError> {
    let has_user_org: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'one_user_org'")
            .fetch_one(pool)
            .await?;
    if !has_user_org {
        return Ok(());
    }

    for (table, creator_col) in [
        ("one_requirements", "creator_id"),
        ("one_milestones", "creator_id"),
        ("one_test_plans", "creator_id"),
        ("one_test_cases", "creator_id"),
        ("one_pipelines", "creator_id"),
    ] {
        sqlx::query(&format!(
            "UPDATE {table} SET tenant_id = ( \
                 SELECT tenant_id FROM one_user_org WHERE user_id = {table}.{creator_col} LIMIT 1 \
             ) \
             WHERE tenant_id = 'default' AND {creator_col} IN (SELECT user_id FROM one_user_org)"
        ))
        .execute(pool)
        .await?;
    }

    // No reliable creator of their own — inherit from the parent, which was
    // just backfilled above.
    sqlx::query(
        "UPDATE one_requirement_comments SET tenant_id = ( \
             SELECT r.tenant_id FROM one_requirements r WHERE r.id = one_requirement_comments.requirement_id \
         ) \
         WHERE tenant_id = 'default' \
           AND requirement_id IN (SELECT id FROM one_requirements WHERE tenant_id != 'default')",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE one_pipeline_runs SET tenant_id = ( \
             SELECT p.tenant_id FROM one_pipelines p WHERE p.id = one_pipeline_runs.pipeline_id \
         ) \
         WHERE tenant_id = 'default' \
           AND pipeline_id IN (SELECT id FROM one_pipelines WHERE tenant_id != 'default')",
    )
    .execute(pool)
    .await?;

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
        assert!(tables.contains(&"one_provider_registry".to_owned()));
        assert!(tables.contains(&"one_provider_channel_tokens".to_owned()));
        assert!(tables.contains(&"one_dlp_rules".to_owned()));
        assert!(tables.contains(&"one_dlp_events".to_owned()));
    }

    /// With a real `one_user_org` present (simulating the real app, where
    /// one-org's migrations run first), legacy rows get their tenant_id
    /// derived from the creator's membership — and a comment/pipeline-run
    /// with no membership of its own inherits from its already-backfilled
    /// parent. Rows whose creator has no membership row at all stay at the
    /// 'default' sentinel rather than being guessed at.
    #[tokio::test]
    async fn backfill_derives_tenant_id_from_creator_and_parent() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE one_user_org (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, role TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('alice', 't1', 'member')")
            .execute(&pool)
            .await
            .unwrap();

        run_one_devops_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO one_requirements (id, type, subject, status, priority, creator_id, created_at, updated_at) \
             VALUES ('r1', 'task', 'subj', 'backlog', 'medium', 'alice', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // orphan creator: no one_user_org row for 'ghost'.
        sqlx::query(
            "INSERT INTO one_requirements (id, type, subject, status, priority, creator_id, created_at, updated_at) \
             VALUES ('r2', 'task', 'subj', 'backlog', 'medium', 'ghost', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO one_requirement_comments (id, requirement_id, author_type, author_name, body, created_at) \
             VALUES ('c1', 'r1', 'agent', 'bot', 'hi', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO one_pipelines (id, name, creator_id, created_at, updated_at) \
             VALUES ('p1', 'build', 'alice', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO one_pipeline_runs (id, pipeline_id, created_at, updated_at) VALUES ('run1', 'p1', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        backfill_collaboration_tenant_ids(&pool).await.unwrap();

        let req_tenant: String = sqlx::query_scalar("SELECT tenant_id FROM one_requirements WHERE id = 'r1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(req_tenant, "t1");
        let orphan_tenant: String = sqlx::query_scalar("SELECT tenant_id FROM one_requirements WHERE id = 'r2'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(orphan_tenant, "default");
        let comment_tenant: String =
            sqlx::query_scalar("SELECT tenant_id FROM one_requirement_comments WHERE id = 'c1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(comment_tenant, "t1");
        let run_tenant: String = sqlx::query_scalar("SELECT tenant_id FROM one_pipeline_runs WHERE id = 'run1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(run_tenant, "t1");
    }
}
