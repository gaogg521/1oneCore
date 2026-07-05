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
