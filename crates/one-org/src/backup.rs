//! Enterprise configuration backup / restore (P1-1).
//!
//! Procurement due diligence asks "how do I back this up and how do I get it
//! back", and until now the answer was "you can't". This exports the
//! enterprise-side configuration — project groups, memberships, departments,
//! invites, companies, licence, SSO wiring and the shared registries — as a
//! single versioned JSON document, and restores it idempotently.
//!
//! # Deliberate exclusions
//!
//! * **User conversations and messages.** Orders of magnitude larger than the
//!   config, and personal rather than organizational data. A "restore" that
//!   replayed someone's chat history into a new deployment would be a privacy
//!   problem, not a feature.
//! * **Secrets.** A backup file gets downloaded, emailed and checked into
//!   places nobody audits. Shipping IdP client secrets, LDAP bind passwords, MCP
//!   credentials or the exit-password hash inside it would turn every copy into
//!   a credential leak. They are stripped on export and must be re-entered
//!   after a restore — see [`REDACTED`] and [`is_secret_column`].
//!
//! # Why the schema is read at runtime
//!
//! Columns are discovered with `PRAGMA table_info` rather than hardcoded.
//! Migrations add and drop columns over time (`one_tenants` gained then dropped
//! its SSO binding columns, for instance), and a hardcoded list would silently
//! stop round-tripping the columns it had gone stale on.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::error::OrgError;

/// Bundle format version. Bump on any breaking change to the envelope; import
/// refuses anything it does not recognize rather than half-applying it.
pub const BACKUP_VERSION: u32 = 1;

/// Placeholder written in place of every redacted value, so a restored row is
/// obviously incomplete rather than subtly wrong.
pub const REDACTED: &str = "__REDACTED__";

/// Tables included in a backup, in dependency order so a sequential restore
/// never inserts a child before its parent.
///
/// Spans several crates' tables by design: this is a backup of the deployment's
/// enterprise configuration, not of one crate. Reading sibling tables through
/// the shared pool is the same cross-crate pattern `one-devops::user_org_role`
/// already uses. Every table is optional at runtime — a deployment that never
/// ran another crate's migrations simply has nothing to export for it.
const BACKUP_TABLES: &[&str] = &[
    // one-org
    "one_tenants",
    "one_user_org",
    "one_active_tenant",
    "one_departments",
    "one_tenant_invites",
    // one-enterprise
    "one_enterprises",
    "one_enterprise_members",
    // one-billing
    "one_enterprise_license",
    "one_license_activation",
    // one-billing T8: the media ledger is unlike `one_usage_events` (excluded
    // above for being unbounded event telemetry) — it is the actual record of
    // what was generated, and losing it on a restore is losing the artifacts'
    // only durable trail (the files themselves are not part of this backup;
    // only their paths and metadata are).
    "one_media_assets",
    "one_media_ledger_settings",
    // one-sso (provider wiring; identities are per-user login state, not config)
    "one_sso_providers",
    // one-devops shared registries
    "one_skill_registry",
    "one_mcp_registry",
    "one_rag_documents",
];

/// One table's rows, as JSON objects keyed by column name.
type TableRows = Vec<serde_json::Map<String, serde_json::Value>>;

/// A restorable snapshot of the deployment's enterprise configuration.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupBundle {
    pub version: u32,
    /// When the export ran (ms epoch), for the operator's own bookkeeping.
    pub exported_at: i64,
    /// The project group whose admin exported it. Recorded for provenance; the
    /// bundle itself is deployment-wide.
    pub exported_by_tenant: String,
    /// True when any value in the bundle was stripped — always true in practice,
    /// and surfaced so the restore UI can warn about re-entering credentials.
    pub contains_redactions: bool,
    /// Table name → rows.
    pub tables: std::collections::BTreeMap<String, TableRows>,
}

/// Columns holding credentials, keyed by table.
///
/// `one_sso_providers.config` is opaque JSON whose secret keys live in one-sso's
/// private `secret_keys()`. one-org is the same architectural layer and cannot
/// depend on it, so rather than duplicate a list that would drift, the whole
/// blob goes through [`redact_json_secrets`], which strips by key *pattern* and
/// therefore also catches secret fields added later.
fn is_secret_column(table: &str, column: &str) -> bool {
    match (table, column) {
        // A bcrypt hash is crackable offline once the file leaves the machine.
        ("one_tenants", "exit_password_hash") => true,
        ("one_mcp_registry", "secrets_json") => true,
        _ => {
            // Generic net for anything credential-shaped, so a new column does
            // not silently start leaking.
            let lower = column.to_ascii_lowercase();
            [
                "secret",
                "password",
                "token",
                "credential",
                "api_key",
                "apikey",
                "private_key",
            ]
            .iter()
            .any(|needle| lower.contains(needle))
        }
    }
}

/// Columns whose value is JSON that may itself contain secrets.
fn is_json_config_column(table: &str, column: &str) -> bool {
    matches!((table, column), ("one_sso_providers", "config"))
}

/// Strip credential-shaped keys from a JSON config, keeping everything else so
/// a restore still recovers endpoints, app ids and redirect URIs.
fn redact_json_secrets(raw: &str) -> serde_json::Value {
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_str::<serde_json::Value>(raw) else {
        // Unparseable config: drop it rather than risk exporting a secret we
        // could not inspect.
        return serde_json::Value::String(REDACTED.to_string());
    };
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let lower = key.to_ascii_lowercase();
        if [
            "secret",
            "password",
            "token",
            "credential",
            "apikey",
            "api_key",
            "privatekey",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            obj.insert(key, serde_json::Value::String(REDACTED.to_string()));
        }
    }
    serde_json::Value::Object(obj)
}

/// Column names of `table`, or `None` when the table does not exist.
async fn table_columns(pool: &SqlitePool, table: &str) -> Result<Option<Vec<String>>, OrgError> {
    // Table names come from the private `BACKUP_TABLES` constant, never from a
    // request, so interpolating them is safe (PRAGMA takes no bind parameters).
    let rows = match sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => return Err(e.into()),
    };
    if rows.is_empty() {
        return Ok(None);
    }
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        columns.push(row.try_get::<String, _>("name")?);
    }
    Ok(Some(columns))
}

/// Export one table as JSON rows, redacting as it goes.
///
/// Rows are materialized by SQLite's own `json_object()` so INTEGER/TEXT/REAL/
/// NULL all keep their JSON types without per-column decoding guesswork. No
/// exported table holds a BLOB (chunk embeddings are excluded), which is the one
/// case `json_object` cannot represent.
async fn export_table(pool: &SqlitePool, table: &str) -> Result<Option<TableRows>, OrgError> {
    let Some(columns) = table_columns(pool, table).await? else {
        return Ok(None);
    };
    let pairs = columns
        .iter()
        .map(|c| format!("'{c}', \"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT json_object({pairs}) AS row_json FROM {table}");
    let rows = sqlx::query(&sql).fetch_all(pool).await?;

    let mut out: TableRows = Vec::with_capacity(rows.len());
    for row in rows {
        let raw: String = row.try_get("row_json")?;
        let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        for column in &columns {
            // A NULL secret column stays NULL: replacing it with the marker
            // would claim a credential existed where none did.
            if is_secret_column(table, column) && !obj.get(column).is_some_and(|v| v.is_null()) {
                obj.insert(column.clone(), serde_json::Value::String(REDACTED.to_string()));
            } else if is_json_config_column(table, column)
                && let Some(serde_json::Value::String(raw_config)) = obj.get(column)
            {
                let redacted = redact_json_secrets(raw_config);
                // Keep the wire type a string: the column stores JSON text, and
                // a restore writes it straight back.
                obj.insert(column.clone(), serde_json::Value::String(redacted.to_string()));
            }
        }
        out.push(obj);
    }
    Ok(Some(out))
}

/// Build a full backup bundle.
pub async fn export_bundle(pool: &SqlitePool, tenant_id: &str, now_ms: i64) -> Result<BackupBundle, OrgError> {
    let mut tables = std::collections::BTreeMap::new();
    for table in BACKUP_TABLES {
        if let Some(rows) = export_table(pool, table).await? {
            tables.insert((*table).to_string(), rows);
        }
    }
    Ok(BackupBundle {
        version: BACKUP_VERSION,
        exported_at: now_ms,
        exported_by_tenant: tenant_id.to_string(),
        contains_redactions: true,
        tables,
    })
}

/// Outcome of a restore, per table, so the operator can see what landed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub tables_applied: usize,
    pub rows_applied: usize,
    /// Tables present in the bundle that this deployment has no schema for
    /// (a crate whose migrations never ran here).
    pub tables_skipped: Vec<String>,
}

/// Restore a bundle.
///
/// Idempotent by construction: every row is written with `INSERT OR REPLACE`
/// keyed on the table's own primary key, so importing the same bundle twice
/// converges instead of duplicating or failing. Runs in one transaction — a
/// partially-restored org (members without their project group, say) would be
/// worse than no restore at all.
///
/// Redacted values are written through as-is rather than being skipped: leaving
/// the literal `__REDACTED__` marker in place makes it visible that a credential
/// needs re-entering, whereas silently keeping a stale secret would look like it
/// had been restored.
pub async fn import_bundle(pool: &SqlitePool, bundle: &BackupBundle) -> Result<ImportReport, OrgError> {
    if bundle.version != BACKUP_VERSION {
        return Err(OrgError::BadRequest(format!(
            "unsupported backup version {} (this build reads version {})",
            bundle.version, BACKUP_VERSION
        )));
    }

    let mut report = ImportReport {
        tables_applied: 0,
        rows_applied: 0,
        tables_skipped: Vec::new(),
    };

    let mut tx = pool.begin().await?;
    for table in BACKUP_TABLES {
        let Some(rows) = bundle.tables.get(*table) else {
            continue;
        };
        // Validate against the live schema, not the bundle's own key set: a
        // bundle from a newer build may carry columns this one has no place for.
        let Some(live_columns) = table_columns(pool, table).await? else {
            report.tables_skipped.push((*table).to_string());
            continue;
        };
        if rows.is_empty() {
            continue;
        }

        for row in rows {
            let columns: Vec<&String> = live_columns.iter().filter(|c| row.contains_key(*c)).collect();
            if columns.is_empty() {
                continue;
            }
            let placeholders = vec!["?"; columns.len()].join(", ");
            let column_list = columns
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("INSERT OR REPLACE INTO {table} ({column_list}) VALUES ({placeholders})");
            let mut query = sqlx::query(&sql);
            for column in &columns {
                query = match row.get(*column) {
                    Some(serde_json::Value::Null) | None => query.bind(Option::<String>::None),
                    Some(serde_json::Value::Bool(b)) => query.bind(*b as i64),
                    Some(serde_json::Value::Number(n)) => {
                        if let Some(i) = n.as_i64() {
                            query.bind(i)
                        } else {
                            query.bind(n.as_f64().unwrap_or_default())
                        }
                    }
                    Some(serde_json::Value::String(s)) => query.bind(s.clone()),
                    // Nested JSON is stored as text in these tables.
                    Some(other) => query.bind(other.to_string()),
                };
            }
            query.execute(&mut *tx).await?;
            report.rows_applied += 1;
        }
        report.tables_applied += 1;
    }
    tx.commit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE one_tenants (id TEXT PRIMARY KEY, name TEXT NOT NULL, exit_password_hash TEXT, created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id));
             CREATE TABLE one_sso_providers (provider TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 0, config TEXT NOT NULL DEFAULT '{}', updated_at INTEGER NOT NULL DEFAULT 0);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn seed(pool: &SqlitePool) {
        sqlx::raw_sql(
            "INSERT INTO one_tenants (id, name, exit_password_hash) VALUES ('t1', 'Group One', '$2b$12$realhash');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('u1', 't1', 'org_admin');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('u2', 't1', 'member');
             INSERT INTO one_sso_providers (provider, enabled, config) VALUES ('oidc', 1, '{\"issuer\":\"https://idp.example.com\",\"clientId\":\"abc\",\"clientSecret\":\"TOPSECRET\"}');",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn export_captures_rows_and_skips_absent_tables() {
        let pool = pool().await;
        seed(&pool).await;
        let bundle = export_bundle(&pool, "t1", 1000).await.unwrap();

        assert_eq!(bundle.version, BACKUP_VERSION);
        assert_eq!(bundle.exported_at, 1000);
        assert_eq!(bundle.tables["one_user_org"].len(), 2);
        // Tables this deployment has no schema for are simply absent, not empty.
        assert!(!bundle.tables.contains_key("one_departments"));
    }

    /// The security-critical assertion: nothing credential-shaped may leave the
    /// deployment inside a downloadable file.
    #[tokio::test]
    async fn export_redacts_every_secret() {
        let pool = pool().await;
        seed(&pool).await;
        let bundle = export_bundle(&pool, "t1", 0).await.unwrap();
        let serialized = serde_json::to_string(&bundle).unwrap();

        assert!(!serialized.contains("$2b$12$realhash"), "exit password hash leaked");
        assert!(!serialized.contains("TOPSECRET"), "OIDC client secret leaked");

        // Non-secret config survives so a restore is still useful.
        let sso = &bundle.tables["one_sso_providers"][0];
        let config = sso["config"].as_str().unwrap();
        assert!(config.contains("idp.example.com"), "issuer should survive redaction");
        assert!(config.contains("abc"), "clientId should survive redaction");
        assert!(config.contains(REDACTED));
    }

    #[tokio::test]
    async fn import_restores_into_an_empty_deployment() {
        let source = pool().await;
        seed(&source).await;
        let bundle = export_bundle(&source, "t1", 0).await.unwrap();

        let target = pool().await;
        let report = import_bundle(&target, &bundle).await.unwrap();
        assert!(report.rows_applied >= 4);

        let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_user_org")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(members, 2);
        let name: String = sqlx::query_scalar("SELECT name FROM one_tenants WHERE id = 't1'")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(name, "Group One");
    }

    /// Restoring twice must converge — an operator retrying a failed download
    /// must not end up with doubled memberships.
    #[tokio::test]
    async fn import_is_idempotent() {
        let source = pool().await;
        seed(&source).await;
        let bundle = export_bundle(&source, "t1", 0).await.unwrap();

        let target = pool().await;
        import_bundle(&target, &bundle).await.unwrap();
        import_bundle(&target, &bundle).await.unwrap();

        let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_user_org")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(members, 2, "re-import must not duplicate rows");
    }

    #[tokio::test]
    async fn import_rejects_an_unknown_version() {
        let target = pool().await;
        let bundle = BackupBundle {
            version: BACKUP_VERSION + 1,
            exported_at: 0,
            exported_by_tenant: "t1".into(),
            contains_redactions: true,
            tables: Default::default(),
        };
        let err = import_bundle(&target, &bundle).await.unwrap_err();
        assert!(
            matches!(err, OrgError::BadRequest(ref m) if m.contains("unsupported backup version")),
            "expected a version refusal, got {err:?}"
        );
    }

    /// A bundle from a newer build may carry columns this deployment lacks;
    /// those must be dropped rather than blowing up the whole restore.
    #[tokio::test]
    async fn import_ignores_columns_this_schema_does_not_have() {
        let source = pool().await;
        seed(&source).await;
        let mut bundle = export_bundle(&source, "t1", 0).await.unwrap();
        for row in bundle.tables.get_mut("one_tenants").unwrap() {
            row.insert("some_future_column".into(), serde_json::Value::String("x".into()));
        }

        let target = pool().await;
        import_bundle(&target, &bundle).await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM one_tenants WHERE id = 't1'")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(name, "Group One");
    }

    /// The bundle is deployment-wide, not per-project-group — it deliberately
    /// carries every tenant's rows so a restore can rebuild the whole
    /// deployment.
    ///
    /// That is exactly why the route must be gated on `RequireSystemAdmin`
    /// rather than `RequireOrgAdmin`: an org_admin only administers their own
    /// group, so exposing this to them would hand group A's admin the full
    /// roster of group B. This test pins the scope so nobody "fixes" the guard
    /// back down without noticing what the payload actually contains.
    #[tokio::test]
    async fn bundle_spans_every_tenant_not_just_the_callers() {
        let pool = pool().await;
        sqlx::raw_sql(
            "INSERT INTO one_tenants (id, name) VALUES ('t1', 'Group One');
             INSERT INTO one_tenants (id, name) VALUES ('t2', 'Group Two');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('a', 't1', 'org_admin');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('b', 't2', 'member');",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Exported *as* t1's admin — yet t2's rows are in the bundle.
        let bundle = export_bundle(&pool, "t1", 0).await.unwrap();
        let tenant_ids: Vec<&str> = bundle.tables["one_tenants"]
            .iter()
            .filter_map(|row| row["id"].as_str())
            .collect();
        assert!(
            tenant_ids.contains(&"t1") && tenant_ids.contains(&"t2"),
            "backup is deployment-wide, so it must not be reachable by a mere org_admin"
        );
        assert_eq!(bundle.tables["one_user_org"].len(), 2);
    }
}
