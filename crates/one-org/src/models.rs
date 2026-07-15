//! Row types and role/tenant helpers for one-org tables.

use serde::Serialize;

/// Personal-edition sentinel tenant. Users without a `one_user_org` row are
/// implicitly in this tenant.
pub const DEFAULT_TENANT_ID: &str = "default";

/// Upstream's built-in operator user (`ensure_system_user` in aionui-db).
/// Mirrors the 1ONE desktop-operator semantics: this user is the instance
/// administrator until explicit roles are assigned.
pub const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";

pub const ROLE_MEMBER: &str = "member";
pub const ROLE_ORG_ADMIN: &str = "org_admin";
pub const ROLE_SYSTEM_ADMIN: &str = "system_admin";

pub fn is_enterprise_tenant_id(tenant_id: &str) -> bool {
    !tenant_id.is_empty() && tenant_id != DEFAULT_TENANT_ID
}

/// `admin` is the legacy alias kept for parity with the 1ONE TS role model.
pub fn is_admin_role(role: &str) -> bool {
    role == ROLE_SYSTEM_ADMIN || role == ROLE_ORG_ADMIN || role == "admin"
}

pub fn is_system_admin_role(role: &str) -> bool {
    role == ROLE_SYSTEM_ADMIN
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantRow {
    pub id: String,
    pub name: String,
    pub exit_password_hash: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InviteRow {
    pub id: String,
    pub tenant_id: String,
    pub code: String,
    pub created_by: String,
    pub max_uses: Option<i64>,
    pub use_count: i64,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub revoked: i64,
}

impl InviteRow {
    pub fn is_active(&self, now_ms: i64) -> bool {
        if self.revoked != 0 {
            return false;
        }
        if let Some(expires_at) = self.expires_at
            && expires_at < now_ms
        {
            return false;
        }
        if let Some(max_uses) = self.max_uses
            && self.use_count >= max_uses
        {
            return false;
        }
        true
    }
}

/// API shape for an invite (admin listing / creation response).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct InviteDto {
    pub id: String,
    pub tenant_id: String,
    pub code: String,
    pub created_by: String,
    pub max_uses: Option<i64>,
    pub use_count: i64,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub revoked: bool,
}

impl From<InviteRow> for InviteDto {
    fn from(row: InviteRow) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            code: row.code,
            created_by: row.created_by,
            max_uses: row.max_uses,
            use_count: row.use_count,
            expires_at: row.expires_at,
            created_at: row.created_at,
            revoked: row.revoked != 0,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserOrgRow {
    pub user_id: String,
    pub tenant_id: String,
    pub role: String,
    pub org_unit_path: Option<String>,
    pub org_profile_source: Option<String>,
    pub org_profile_synced_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Resolved enterprise context for the current user.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgContextDto {
    pub tenant_id: String,
    pub tenant_name: Option<String>,
    pub role: String,
    pub is_enterprise: bool,
    pub member_count: i64,
}

/// Admin view of a user — joins upstream `users` (id/username) with
/// `one_user_org` (tenant/role/display_name/org_unit_path).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AdminUserDto {
    pub user_id: String,
    pub username: String,
    pub tenant_id: String,
    pub role: String,
    pub display_name: Option<String>,
    pub org_unit_path: Option<String>,
    pub job_title: Option<String>,
    pub last_login: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuntimeNodeRow {
    pub id: String,
    pub tenant_id: String,
    pub user_id: String,
    pub machine_id: String,
    pub display_name: String,
    pub hostnames: String,
    pub ip_addresses: String,
    pub installed_agents: String,
    pub last_seen_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeNodeDto {
    pub id: String,
    pub tenant_id: String,
    pub user_id: String,
    pub machine_id: String,
    pub display_name: String,
    pub hostnames: serde_json::Value,
    pub ip_addresses: serde_json::Value,
    pub installed_agents: serde_json::Value,
    pub last_seen_at: i64,
    pub updated_at: i64,
}

impl From<RuntimeNodeRow> for RuntimeNodeDto {
    fn from(row: RuntimeNodeRow) -> Self {
        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap_or(serde_json::json!([]));
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            user_id: row.user_id,
            machine_id: row.machine_id,
            display_name: row.display_name,
            hostnames: parse(&row.hostnames),
            ip_addresses: parse(&row.ip_addresses),
            installed_agents: parse(&row.installed_agents),
            last_seen_at: row.last_seen_at,
            updated_at: row.updated_at,
        }
    }
}

/// Result of dissolving stale local enterprise data via `reset_local_enterprise`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetLocalResult {
    pub archived_tenant_count: i64,
    pub archived_member_count: i64,
    pub archive_path: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogRow {
    pub id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub action: String,
    pub resource: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: i64,
}
