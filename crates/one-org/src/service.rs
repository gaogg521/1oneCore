//! Enterprise tenant service: join / exit / create / invites / exit password.
//!
//! Logic is a direct translation of the 1ONE ClaudeCode TS reference
//! (`src/process/webserver/auth/enterpriseJoinService.ts`); error codes and
//! transaction boundaries are kept identical. Enterprise user attributes
//! live in `one_user_org` — the upstream `users` table is never modified,
//! except for rotating the per-user `jwt_secret` through the upstream
//! repository to invalidate sessions after a tenant change (the per-user
//! secret makes this strictly scoped to the affected user).

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sqlx::SqlitePool;

use aionui_auth::{generate_random_secret_string, hash_password, verify_password};
use aionui_common::now_ms;
use aionui_db::IUserRepository;

use crate::error::OrgError;
use crate::models::{
    AdminUserDto, AuditLogRow, DEFAULT_TENANT_ID, InviteDto, InviteRow, OrgContextDto, ROLE_MEMBER, ROLE_SYSTEM_ADMIN,
    ResetLocalResult, RuntimeNodeDto, RuntimeNodeRow, SYSTEM_DEFAULT_USER_ID, TenantRow, UserOrgRow,
    is_enterprise_tenant_id, is_system_admin_role,
};

pub struct OrgService {
    pool: SqlitePool,
    user_repo: Arc<dyn IUserRepository>,
    data_dir: PathBuf,
}

/// Normalize an invite code: strip whitespace/dashes, uppercase.
pub fn normalize_invite_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_uppercase()
}

/// Dash-grouped display form (`XXXX-XXXX-…`), four hex chars per group.
fn format_invite_code_for_display(code: &str) -> String {
    let n = normalize_invite_code(code);
    n.as_bytes()
        .chunks(4)
        .filter_map(|c| std::str::from_utf8(c).ok())
        .collect::<Vec<_>>()
        .join("-")
}

/// 16 uppercase hex chars from 8 CSPRNG bytes (2^64 space — D4: the previous
/// 4-byte / 2^32 code was enumerable by any logged-in user).
fn generate_invite_code() -> String {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    buf.iter().map(|b| format!("{b:02X}")).collect()
}

fn short_id(prefix: &str) -> String {
    let uuid = uuid::Uuid::now_v7().simple().to_string();
    format!("{prefix}_{uuid}")
}

impl OrgService {
    pub fn new(pool: SqlitePool, user_repo: Arc<dyn IUserRepository>, data_dir: PathBuf) -> Self {
        Self { pool, user_repo, data_dir }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // --- membership / roles ---

    pub async fn membership(&self, user_id: &str) -> Result<Option<UserOrgRow>, OrgError> {
        let row = sqlx::query_as::<_, UserOrgRow>("SELECT * FROM one_user_org WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Effective role: explicit `one_user_org` row wins; the upstream
    /// built-in operator user is system_admin by default (desktop-operator
    /// semantics); everyone else is a plain member.
    pub async fn effective_role(&self, user_id: &str) -> Result<String, OrgError> {
        if let Some(row) = self.membership(user_id).await? {
            return Ok(row.role);
        }
        if user_id == SYSTEM_DEFAULT_USER_ID {
            return Ok(ROLE_SYSTEM_ADMIN.to_string());
        }
        Ok(ROLE_MEMBER.to_string())
    }

    pub async fn tenant_of(&self, user_id: &str) -> Result<String, OrgError> {
        Ok(self
            .membership(user_id)
            .await?
            .map(|row| row.tenant_id)
            .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string()))
    }

    async fn get_tenant(&self, tenant_id: &str) -> Result<Option<TenantRow>, OrgError> {
        let row = sqlx::query_as::<_, TenantRow>("SELECT * FROM one_tenants WHERE id = ?")
            .bind(tenant_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Invalidate the user's sessions by rotating their per-user JWT secret.
    async fn invalidate_user_tokens(&self, user_id: &str) -> Result<(), OrgError> {
        let secret = generate_random_secret_string();
        self.user_repo.update_jwt_secret(user_id, &secret).await?;
        Ok(())
    }

    // --- invites ---

    async fn find_active_invite_by_code(&self, code: &str) -> Result<Option<InviteRow>, OrgError> {
        let row = sqlx::query_as::<_, InviteRow>("SELECT * FROM one_tenant_invites WHERE code = ?")
            .bind(code)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.filter(|invite| invite.is_active(now_ms() as i64)))
    }

    /// Validate an invite code without leaking tenant identity
    /// (anti-enumeration, same as the TS preview endpoint).
    pub async fn preview_invite(&self, code_raw: &str) -> Result<(), OrgError> {
        let code = normalize_invite_code(code_raw);
        if code.len() < 6 {
            return Err(OrgError::InvalidCode);
        }
        self.find_active_invite_by_code(&code)
            .await?
            .map(|_| ())
            .ok_or(OrgError::InvalidCode)
    }

    pub async fn create_invite(
        &self,
        tenant_id: &str,
        created_by: &str,
        max_uses: Option<i64>,
        expires_in_days: Option<i64>,
    ) -> Result<(InviteDto, String), OrgError> {
        if self.get_tenant(tenant_id).await?.is_none() {
            return Err(OrgError::TenantNotFound);
        }

        let now = now_ms() as i64;
        let mut code = generate_invite_code();
        for _ in 0..5 {
            let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_tenant_invites WHERE code = ?")
                .bind(&code)
                .fetch_one(&self.pool)
                .await?;
            if !exists {
                break;
            }
            code = generate_invite_code();
        }

        let expires_at = expires_in_days
            .filter(|days| *days > 0)
            .map(|days| now + days * 24 * 60 * 60 * 1000);
        let id = short_id("inv");

        sqlx::query(
            "INSERT INTO one_tenant_invites \
             (id, tenant_id, code, created_by, max_uses, use_count, expires_at, created_at, revoked) \
             VALUES (?, ?, ?, ?, ?, 0, ?, ?, 0)",
        )
        .bind(&id)
        .bind(tenant_id)
        .bind(&code)
        .bind(created_by)
        .bind(max_uses)
        .bind(expires_at)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, InviteRow>("SELECT * FROM one_tenant_invites WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.pool)
            .await?;
        let display = format_invite_code_for_display(&code);
        Ok((row.into(), display))
    }

    pub async fn list_invites(&self, tenant_id: &str) -> Result<Vec<InviteDto>, OrgError> {
        let rows = sqlx::query_as::<_, InviteRow>(
            "SELECT * FROM one_tenant_invites WHERE tenant_id = ? ORDER BY created_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn revoke_invite(&self, tenant_id: &str, invite_id: &str) -> Result<(), OrgError> {
        let result = sqlx::query("UPDATE one_tenant_invites SET revoked = 1 WHERE id = ? AND tenant_id = ?")
            .bind(invite_id)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(OrgError::InvalidCode);
        }
        Ok(())
    }

    // --- join / create / exit ---

    pub async fn join_with_invite(&self, user_id: &str, code_raw: &str) -> Result<(String, String), OrgError> {
        let current_tenant = self.tenant_of(user_id).await?;
        if is_enterprise_tenant_id(&current_tenant) {
            return Err(OrgError::AlreadyInEnterprise);
        }

        let code = normalize_invite_code(code_raw);
        let invite = self
            .find_active_invite_by_code(&code)
            .await?
            .ok_or(OrgError::InvalidCode)?;

        let now = now_ms() as i64;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE one_tenant_invites SET use_count = use_count + 1 WHERE id = ?")
            .bind(&invite.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO one_user_org (user_id, tenant_id, role, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&invite.tenant_id)
        .bind(ROLE_MEMBER)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.invalidate_user_tokens(user_id).await?;
        self.audit(&invite.tenant_id, Some(user_id), "org.join", Some(&invite.id))
            .await;

        let tenant = self
            .get_tenant(&invite.tenant_id)
            .await?
            .ok_or(OrgError::TenantNotFound)?;
        Ok((tenant.id, tenant.name))
    }

    pub async fn create_tenant(&self, user_id: &str, name_raw: &str) -> Result<(String, String), OrgError> {
        let name = name_raw.trim();
        if name.is_empty() {
            return Err(OrgError::NameRequired);
        }
        let current_tenant = self.tenant_of(user_id).await?;
        if is_enterprise_tenant_id(&current_tenant) {
            return Err(OrgError::AlreadyInEnterprise);
        }
        let role = self.effective_role(user_id).await?;
        if !is_system_admin_role(&role) {
            return Err(OrgError::Forbidden(
                "Only system administrators can create an enterprise".into(),
            ));
        }
        // D3: one server = one enterprise. The one-devops registries and
        // collaboration boards carry no tenant_id, so a second tenant on the
        // same instance would share every skill / MCP / requirement with the
        // first. Reject creation once any tenant exists; members join the
        // existing enterprise via invite instead.
        let existing_tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_tenants")
            .fetch_one(&self.pool)
            .await?;
        if existing_tenants > 0 {
            return Err(OrgError::AlreadyHostsEnterprise);
        }

        let tenant_id = short_id("tenant");
        let now = now_ms() as i64;
        // Creator keeps system_admin (instance-level governance) — same
        // rationale as the TS reference: downgrading to org_admin here would
        // leave the instance with no system_admin.
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO one_tenants (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(&tenant_id)
            .bind(name)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO one_user_org (user_id, tenant_id, role, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
                 tenant_id = excluded.tenant_id, role = excluded.role, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&tenant_id)
        .bind(ROLE_SYSTEM_ADMIN)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.invalidate_user_tokens(user_id).await?;
        self.audit(&tenant_id, Some(user_id), "org.create", Some(name)).await;

        Ok((tenant_id, name.to_string()))
    }

    /// Archive and wipe all local tenant/membership data, so a stale/orphaned
    /// tenant left behind on this machine (from a prior test or a reinstall
    /// that never went through a clean `leave`) no longer blocks
    /// `create_tenant`'s "one server = one enterprise" gate. Self-service
    /// escape hatch for the scenario the D3 comment above didn't account
    /// for: a tenant row surviving with no one able to administer it.
    pub async fn reset_local_enterprise(&self, user_id: &str) -> Result<ResetLocalResult, OrgError> {
        let role = self.effective_role(user_id).await?;
        if !is_system_admin_role(&role) {
            return Err(OrgError::Forbidden(
                "Only system administrators can reset local enterprise data".into(),
            ));
        }

        #[derive(Serialize)]
        struct ArchivedTenant {
            id: String,
            name: String,
            created_at: i64,
            updated_at: i64,
            members: Vec<AdminUserDto>,
        }

        #[derive(Serialize)]
        struct ArchiveSnapshot {
            archived_at: i64,
            tenants: Vec<ArchivedTenant>,
        }

        let tenants = sqlx::query_as::<_, TenantRow>("SELECT * FROM one_tenants")
            .fetch_all(&self.pool)
            .await?;

        let mut archived_tenants = Vec::with_capacity(tenants.len());
        let mut affected_user_ids = Vec::new();
        let mut archived_member_count: i64 = 0;
        for tenant in &tenants {
            let members = self.list_users(&tenant.id).await?;
            archived_member_count += members.len() as i64;
            affected_user_ids.extend(members.iter().map(|m| m.user_id.clone()));
            archived_tenants.push(ArchivedTenant {
                id: tenant.id.clone(),
                name: tenant.name.clone(),
                created_at: tenant.created_at,
                updated_at: tenant.updated_at,
                members,
            });
        }
        let archived_tenant_count = archived_tenants.len() as i64;

        let now = now_ms() as i64;
        let snapshot = ArchiveSnapshot {
            archived_at: now,
            tenants: archived_tenants,
        };

        let archive_dir = self.data_dir.join("enterprise-archives");
        std::fs::create_dir_all(&archive_dir)
            .map_err(|e| OrgError::Internal(format!("failed to create enterprise archive directory: {e}")))?;
        let archive_path = archive_dir.join(format!("enterprise-{now}.json"));
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| OrgError::Internal(format!("failed to serialize enterprise archive: {e}")))?;
        std::fs::write(&archive_path, json)
            .map_err(|e| OrgError::Internal(format!("failed to write enterprise archive: {e}")))?;
        let archive_path_str = archive_path.to_string_lossy().to_string();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM one_user_org").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM one_tenants").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM one_tenant_invites").execute(&mut *tx).await?;
        tx.commit().await?;

        for affected_user_id in &affected_user_ids {
            self.invalidate_user_tokens(affected_user_id).await?;
        }
        self.audit(DEFAULT_TENANT_ID, Some(user_id), "org.reset_local", Some(&archive_path_str))
            .await;

        Ok(ResetLocalResult {
            archived_tenant_count,
            archived_member_count,
            archive_path: archive_path_str,
        })
    }

    pub async fn leave(&self, user_id: &str, exit_code: &str) -> Result<(), OrgError> {
        let membership = self.membership(user_id).await?;
        let Some(membership) = membership.filter(|m| is_enterprise_tenant_id(&m.tenant_id)) else {
            return Err(OrgError::NotInEnterprise);
        };

        let tenant = self.get_tenant(&membership.tenant_id).await?;
        if let Some(hash) = tenant.and_then(|t| t.exit_password_hash) {
            if !verify_password(exit_code, &hash)? {
                return Err(OrgError::WrongExitCode);
            }
        }

        sqlx::query("DELETE FROM one_user_org WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        self.invalidate_user_tokens(user_id).await?;
        self.audit(&membership.tenant_id, Some(user_id), "org.exit", None).await;
        Ok(())
    }

    // --- exit password (admin) ---

    pub async fn exit_password_status(&self, tenant_id: &str) -> Result<bool, OrgError> {
        let tenant = self.get_tenant(tenant_id).await?.ok_or(OrgError::TenantNotFound)?;
        Ok(tenant.exit_password_hash.is_some())
    }

    pub async fn set_exit_password(&self, tenant_id: &str, password: &str) -> Result<(), OrgError> {
        if password.is_empty() {
            return Err(OrgError::BadRequest("password is required".into()));
        }
        let hash = hash_password(password)?;
        let result = sqlx::query("UPDATE one_tenants SET exit_password_hash = ?, updated_at = ? WHERE id = ?")
            .bind(&hash)
            .bind(now_ms() as i64)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(OrgError::TenantNotFound);
        }
        Ok(())
    }

    pub async fn clear_exit_password(&self, tenant_id: &str) -> Result<(), OrgError> {
        let result = sqlx::query("UPDATE one_tenants SET exit_password_hash = NULL, updated_at = ? WHERE id = ?")
            .bind(now_ms() as i64)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(OrgError::TenantNotFound);
        }
        Ok(())
    }

    // --- context / info ---

    pub async fn member_count(&self, tenant_id: &str) -> Result<i64, OrgError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_user_org WHERE tenant_id = ?")
            .bind(tenant_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn context(&self, user_id: &str) -> Result<OrgContextDto, OrgError> {
        let tenant_id = self.tenant_of(user_id).await?;
        let role = self.effective_role(user_id).await?;
        let is_enterprise = is_enterprise_tenant_id(&tenant_id);
        let (tenant_name, member_count) = if is_enterprise {
            let name = self.get_tenant(&tenant_id).await?.map(|t| t.name);
            let count = self.member_count(&tenant_id).await?;
            (name, count)
        } else {
            (None, 0)
        };
        Ok(OrgContextDto {
            tenant_id,
            tenant_name,
            role,
            is_enterprise,
            member_count,
        })
    }

    /// Name of the enterprise hosted on this server, if any.
    pub async fn public_info(&self) -> Result<Option<String>, OrgError> {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM one_tenants ORDER BY created_at ASC LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(name)
    }

    // --- audit ---

    /// Best-effort audit write; failures are logged, never surfaced.
    pub async fn audit(&self, tenant_id: &str, user_id: Option<&str>, action: &str, resource: Option<&str>) {
        let result = sqlx::query(
            "INSERT INTO one_audit_logs (id, tenant_id, user_id, action, resource, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(short_id("audit"))
        .bind(tenant_id)
        .bind(user_id)
        .bind(action)
        .bind(resource)
        .bind(now_ms() as i64)
        .execute(&self.pool)
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, action, "one-org audit write failed");
        }
    }

    pub async fn list_audit_logs(&self, tenant_id: &str, limit: i64) -> Result<Vec<AuditLogRow>, OrgError> {
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, AuditLogRow>(
            "SELECT id, tenant_id, user_id, username, action, resource, ip_address, user_agent, created_at \
             FROM one_audit_logs WHERE tenant_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(tenant_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // --- admin: users ---

    pub async fn list_users(&self, tenant_id: &str) -> Result<Vec<AdminUserDto>, OrgError> {
        let rows = sqlx::query_as::<_, AdminUserDto>(
            "SELECT uo.user_id, u.username, uo.tenant_id, uo.role, uo.org_unit_path, \
                    u.last_login, uo.created_at \
             FROM one_user_org uo \
             JOIN users u ON u.id = uo.user_id \
             WHERE uo.tenant_id = ? \
             ORDER BY uo.created_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Promote/demote a user's role within a tenant. `role` must be one of
    /// `member`/`org_admin`/`system_admin` — validated by the caller (route
    /// handler) so we keep the service free of string validation.
    pub async fn set_user_role(&self, tenant_id: &str, user_id: &str, role: &str) -> Result<(), OrgError> {
        let result =
            sqlx::query("UPDATE one_user_org SET role = ?, updated_at = ? WHERE tenant_id = ? AND user_id = ?")
                .bind(role)
                .bind(now_ms() as i64)
                .bind(tenant_id)
                .bind(user_id)
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(OrgError::BadRequest(format!(
                "user {user_id} not in tenant {tenant_id}"
            )));
        }
        // Note: upstream `users` table has no role column — role lives
        // exclusively in `one_user_org`. The auth middleware's role check
        // reads from `CurrentUser`, which is populated from the JWT payload
        // (no role). RBAC for `/api/one/*` is handled by the `RequireOrgAdmin`
        // extractor reading `one_user_org` directly.
        self.audit(tenant_id, Some(user_id), "set_role", Some(role)).await;
        Ok(())
    }

    // --- admin: runtime nodes ---

    pub async fn list_runtime_nodes(&self, tenant_id: &str) -> Result<Vec<RuntimeNodeDto>, OrgError> {
        let rows = sqlx::query_as::<_, RuntimeNodeRow>(
            "SELECT id, tenant_id, user_id, machine_id, display_name, hostnames, ip_addresses, \
                    installed_agents, last_seen_at, updated_at \
             FROM one_runtime_nodes WHERE tenant_id = ? ORDER BY last_seen_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Upsert a runtime node heartbeat by (tenant_id, machine_id).
    pub async fn heartbeat_runtime_node(
        &self,
        tenant_id: &str,
        user_id: &str,
        machine_id: &str,
        display_name: &str,
        hostnames: &serde_json::Value,
        ip_addresses: &serde_json::Value,
        installed_agents: &serde_json::Value,
    ) -> Result<String, OrgError> {
        let now = now_ms() as i64;
        let hostnames_str = hostnames.to_string();
        let ip_str = ip_addresses.to_string();
        let agents_str = installed_agents.to_string();

        // Try UPDATE first; if no row affected, INSERT.
        let updated = sqlx::query(
            "UPDATE one_runtime_nodes SET user_id = ?, display_name = ?, hostnames = ?, \
                    ip_addresses = ?, installed_agents = ?, last_seen_at = ?, updated_at = ? \
             WHERE tenant_id = ? AND machine_id = ?",
        )
        .bind(user_id)
        .bind(display_name)
        .bind(&hostnames_str)
        .bind(&ip_str)
        .bind(&agents_str)
        .bind(now)
        .bind(now)
        .bind(tenant_id)
        .bind(machine_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated > 0 {
            let id: String =
                sqlx::query_scalar("SELECT id FROM one_runtime_nodes WHERE tenant_id = ? AND machine_id = ?")
                    .bind(tenant_id)
                    .bind(machine_id)
                    .fetch_one(&self.pool)
                    .await?;
            return Ok(id);
        }

        let id = short_id("node");
        sqlx::query(
            "INSERT INTO one_runtime_nodes \
             (id, tenant_id, user_id, machine_id, display_name, hostnames, ip_addresses, \
              installed_agents, last_seen_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(tenant_id)
        .bind(user_id)
        .bind(machine_id)
        .bind(display_name)
        .bind(&hostnames_str)
        .bind(&ip_str)
        .bind(&agents_str)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::SqliteUserRepository;

    async fn setup() -> (aionui_db::Database, Arc<OrgService>, Arc<dyn IUserRepository>) {
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_migrations(db.pool()).await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(SqliteUserRepository::new(db.pool().clone()));
        let data_dir = std::env::temp_dir().join(format!("one-org-test-{}", uuid::Uuid::now_v7()));
        let service = Arc::new(OrgService::new(db.pool().clone(), user_repo.clone(), data_dir));
        (db, service, user_repo)
    }

    /// Token rotation goes through the upstream user repo, so test users must
    /// exist in the upstream `users` table (in production the auth middleware
    /// guarantees this).
    async fn create_user(user_repo: &Arc<dyn IUserRepository>, username: &str) -> String {
        user_repo.create_user(username, "x").await.unwrap().id
    }

    #[test]
    fn normalize_strips_dashes_and_uppercases() {
        assert_eq!(normalize_invite_code(" ab-12 cd\t"), "AB12CD");
    }

    #[test]
    fn display_format_splits_after_four() {
        assert_eq!(format_invite_code_for_display("AB12CD34"), "AB12-CD34");
        assert_eq!(format_invite_code_for_display("AB12"), "AB12");
        // 16-char (8-byte) codes group into four dash-separated quads.
        assert_eq!(
            format_invite_code_for_display("0123456789ABCDEF"),
            "0123-4567-89AB-CDEF"
        );
        // Round-trips: a displayed code normalizes back to the raw form.
        assert_eq!(normalize_invite_code("0123-4567-89AB-CDEF"), "0123456789ABCDEF");
    }

    #[test]
    fn generated_invite_code_is_16_hex() {
        let code = generate_invite_code();
        assert_eq!(code.len(), 16, "8 CSPRNG bytes → 16 hex chars");
        assert!(code.chars().all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()));
    }

    #[tokio::test]
    async fn create_join_exit_full_cycle() {
        let (db, service, user_repo) = setup().await;

        // system_default_user is implicit system_admin → can create.
        let (tenant_id, tenant_name) = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "  Acme Inc  ")
            .await
            .unwrap();
        assert!(tenant_id.starts_with("tenant_"));
        assert_eq!(tenant_name, "Acme Inc");
        assert_eq!(
            service.effective_role(SYSTEM_DEFAULT_USER_ID).await.unwrap(),
            ROLE_SYSTEM_ADMIN
        );

        // Creating again while inside an enterprise is rejected.
        let err = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "Другая")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "ALREADY_IN_ENTERPRISE");

        // Invite + preview + join as a second user.
        let (invite, display) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, Some(2), Some(1))
            .await
            .unwrap();
        assert_eq!(invite.use_count, 0);
        assert!(display.contains('-'));
        service.preview_invite(&display).await.unwrap();

        let member = create_user(&user_repo, "member1").await;
        let member = member.as_str();
        let (joined_tenant, joined_name) = service.join_with_invite(member, &display).await.unwrap();
        assert_eq!(joined_tenant, tenant_id);
        assert_eq!(joined_name, "Acme Inc");
        assert_eq!(service.effective_role(member).await.unwrap(), ROLE_MEMBER);
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 2);

        // Double join rejected.
        let err = service.join_with_invite(member, &display).await.unwrap_err();
        assert_eq!(err.code(), "ALREADY_IN_ENTERPRISE");

        // Exit: no password set — member may leave without a code.
        service.leave(member, "").await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 1);

        // Re-join to exercise password-gated exit.
        service.preview_invite(&display).await.unwrap();
        service.join_with_invite(member, &display).await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 2);

        service.set_exit_password(&tenant_id, "s3cret").await.unwrap();
        assert!(service.exit_password_status(&tenant_id).await.unwrap());

        let err = service.leave(member, "wrong").await.unwrap_err();
        assert_eq!(err.code(), "WRONG_EXIT_CODE");

        service.leave(member, "s3cret").await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 1);
        let err = service.leave(member, "s3cret").await.unwrap_err();
        assert_eq!(err.code(), "NOT_IN_ENTERPRISE");

        db.close().await;
    }

    #[tokio::test]
    async fn non_admin_cannot_create_tenant() {
        let (db, service, user_repo) = setup().await;
        let user = create_user(&user_repo, "random_user").await;
        let err = service.create_tenant(&user, "Evil Corp").await.unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
        db.close().await;
    }

    #[tokio::test]
    async fn one_server_hosts_only_one_enterprise() {
        let (_db, service, _user_repo) = setup().await;
        service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        // Simulate the creator having exited (org row gone) so they are once
        // more an implicit system_admin not in any enterprise — the only way
        // to slip past the AlreadyInEnterprise / role guards.
        sqlx::query("DELETE FROM one_user_org WHERE user_id = ?")
            .bind(SYSTEM_DEFAULT_USER_ID)
            .execute(&service.pool)
            .await
            .unwrap();

        // A tenant still exists → D3 guard rejects a second enterprise.
        let err = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "SecondCorp")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "ALREADY_HOSTS_ENTERPRISE");
    }

    #[tokio::test]
    async fn reset_local_enterprise_clears_stale_tenant_and_allows_recreate() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let member = create_user(&user_repo, "member1").await;
        let (invite, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let _ = invite;
        service.join_with_invite(&member, &code).await.unwrap();

        // Simulate the same stale-data scenario as
        // `one_server_hosts_only_one_enterprise`: the creator's own
        // membership row is gone, but the tenant (and the other member) are
        // still there, so `create_tenant` is blocked.
        sqlx::query("DELETE FROM one_user_org WHERE user_id = ?")
            .bind(SYSTEM_DEFAULT_USER_ID)
            .execute(&service.pool)
            .await
            .unwrap();
        let err = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "SecondCorp")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "ALREADY_HOSTS_ENTERPRISE");

        let result = service.reset_local_enterprise(SYSTEM_DEFAULT_USER_ID).await.unwrap();
        assert_eq!(result.archived_tenant_count, 1);
        assert_eq!(result.archived_member_count, 1); // only `member` had a row left
        assert!(std::path::Path::new(&result.archive_path).exists());
        let archived_json = std::fs::read_to_string(&result.archive_path).unwrap();
        assert!(archived_json.contains("Acme"));
        assert!(archived_json.contains(&member));

        let tenants_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_tenants")
            .fetch_one(&service.pool)
            .await
            .unwrap();
        assert_eq!(tenants_left, 0);
        let memberships_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_user_org")
            .fetch_one(&service.pool)
            .await
            .unwrap();
        assert_eq!(memberships_left, 0);

        // The gate is clear again — creating a fresh enterprise now succeeds.
        let (new_tenant_id, new_name) = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "SecondCorp")
            .await
            .unwrap();
        assert_ne!(new_tenant_id, tenant_id);
        assert_eq!(new_name, "SecondCorp");

        db.close().await;
    }

    #[tokio::test]
    async fn reset_local_enterprise_requires_system_admin() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(&user_repo, "member1").await;
        service.join_with_invite(&member, &code).await.unwrap();

        let err = service.reset_local_enterprise(&member).await.unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");

        // Nothing was touched.
        let tenants_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_tenants")
            .fetch_one(&service.pool)
            .await
            .unwrap();
        assert_eq!(tenants_left, 1);

        db.close().await;
    }

    #[tokio::test]
    async fn invite_exhaustion_and_revoke() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        let (invite, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, Some(1), None)
            .await
            .unwrap();
        let u1 = create_user(&user_repo, "u1").await;
        let u2 = create_user(&user_repo, "u2").await;
        service.join_with_invite(&u1, &code).await.unwrap();
        // max_uses=1 exhausted.
        let err = service.join_with_invite(&u2, &code).await.unwrap_err();
        assert_eq!(err.code(), "INVALID_CODE");

        // Revoked invite stops validating.
        let (invite2, code2) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        service.revoke_invite(&tenant_id, &invite2.id).await.unwrap();
        let err = service.preview_invite(&code2).await.unwrap_err();
        assert_eq!(err.code(), "INVALID_CODE");

        let listed = service.list_invites(&tenant_id).await.unwrap();
        assert_eq!(listed.len(), 2);
        let _ = invite;
        db.close().await;
    }
}
