//! Row types and API DTOs for one-enterprise tables.

use serde::Serialize;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EnterpriseRow {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub display_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EnterpriseMemberRow {
    pub user_id: String,
    pub enterprise_id: String,
    pub display_name: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    pub role: String,
    pub joined_at: i64,
    pub updated_at: i64,
}

/// The caller's own enterprise-org identity: which SSO company they belong to,
/// their own name, and their department / job title. Independent of any
/// project-group membership. `company_id` is the raw IdP company id (e.g.
/// Feishu `tenant_key`); `company_name` is the human-readable company name,
/// often `None` because Feishu SSO doesn't surface it. `display_name` is the
/// member's own name; `department` / `job_title` are only populated when the
/// SSO grant includes a directory scope.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnterpriseIdentityDto {
    pub provider: String,
    pub company_id: String,
    pub company_name: Option<String>,
    pub display_name: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    pub role: String,
}
