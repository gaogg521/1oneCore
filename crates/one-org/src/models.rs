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
    /// Owning company (one-enterprise `one_enterprises.id`), or `None` for a
    /// standalone invite-code project group with no company (Direction B, the
    /// "可独立可归属" model). Added in migration 006.
    pub enterprise_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Lightweight (id, name) summary of every project group on this server, for
/// admin pickers (e.g. the devops resource scope selector, P0-4). No member
/// count — just enough to populate a dropdown.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct TenantSummaryDto {
    #[sqlx(rename = "id")]
    pub tenant_id: String,
    pub name: String,
}

/// A project group owned by a company, for the company admin console
/// (Direction B). Distinct from the invite-code `OrgTenant` returned by
/// join/create — this is the company-scoped listing view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnterpriseTenantDto {
    pub tenant_id: String,
    pub name: String,
    pub member_count: i64,
    pub created_at: i64,
}

/// One project group a user belongs to, for the "my project groups" switcher
/// (Direction B / Phase 2 multi-membership). `is_active` marks the group
/// currently in effect (resolved via `one_active_tenant`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MyTenantDto {
    pub tenant_id: String,
    pub name: String,
    pub role: String,
    pub member_count: i64,
    pub is_active: bool,
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
    pub display_name: Option<String>,
    pub org_unit_path: Option<String>,
    pub job_title: Option<String>,
    pub org_profile_source: Option<String>,
    pub org_profile_synced_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Resolved project-group context for the current user. Pure project-group
/// (invite-code tenant) info — the SSO-company "enterprise org" dimension is a
/// separate concern served by one-enterprise (`/api/one/enterprise/me`).
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
    /// Structured department assignment (P2-3), distinct from the free-text
    /// SSO-synced `org_unit_path` above.
    pub department_id: Option<String>,
    pub last_login: Option<i64>,
    pub created_at: i64,
}

/// What one directory-mapping run did (T6 stage 3), for the admin console.
/// Names rather than ids/counts throughout — an admin reading this wants to
/// know WHICH departments, not just how many.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryMapReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    /// Removed because they dropped out of the mapped subtree AND were empty
    /// (no children, no assigned members).
    pub removed: Vec<String>,
    /// Dropped out of the mapped subtree but kept, because real local
    /// structure — a manually-added child or an assigned member — is still
    /// hanging off them. Not an error: reported so the admin can reassign
    /// and re-run, the same "explicit over surprising" rule `delete_department`
    /// already enforces everywhere else in this file.
    pub kept_with_local_data: Vec<String>,
}

/// A department/sub-team node within a project group (P2-3 organizational
/// hierarchy). `parent_id` is `None` for a top-level department. The frontend
/// builds the tree client-side from the flat list returned by
/// `OrgService::list_departments`.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct DepartmentDto {
    pub id: String,
    pub tenant_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// `None` for a manually-created department (the overwhelming default).
    /// `Some("directory")` means a T6 stage 3 mapping sync created/owns this
    /// row — the admin console renders it read-only-ish (name still editable,
    /// but it will be overwritten on the next sync from upstream's name) and
    /// distinct from a department someone typed in by hand.
    pub source: Option<String>,
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

/// One agent tool-call, for the agent-run audit (P1-1 "可审计的本地优先").
/// Derived from persisted `messages` (tool-call rows) joined to the owning
/// conversation — the record of which agent run touched which file / ran which
/// command / called which tool. `detail` is a best-effort target (command /
/// path / url) extracted from the tool args.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AgentAuditEntry {
    pub id: String,
    pub conversation_id: String,
    pub user_id: Option<String>,
    pub tool_name: String,
    pub detail: Option<String>,
    pub status: Option<String>,
    pub created_at: i64,
}

/// Redacted SMTP config for the admin settings UI (P2-4 onboarding). The
/// password is never echoed back — only whether one is stored. `enabled`
/// reflects the operator's own toggle; a real send additionally requires an
/// `EmailSender` implementation to be wired at the app layer (see
/// `service::EmailSender`) — until then sends report "not configured".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmtpConfigDto {
    pub host: Option<String>,
    pub port: Option<i64>,
    pub username: Option<String>,
    pub has_password: bool,
    pub from_address: Option<String>,
    pub enabled: bool,
    pub updated_at: Option<i64>,
}

/// Redacted integration-connector config for the admin settings UI (P2-1
/// reserved framework). The secret (token / API key) is never echoed back —
/// only whether one is stored. `enabled` reflects the operator's own toggle; a
/// real sync additionally requires an `IntegrationProvider` implementation to
/// be wired at the app layer — until then a "test" reports "not configured".
/// `config` is the parsed non-secret JSON object (empty object when unset).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationDto {
    pub provider: String,
    pub base_url: Option<String>,
    pub config: serde_json::Value,
    pub has_secret: bool,
    pub enabled: bool,
    pub updated_at: Option<i64>,
}
