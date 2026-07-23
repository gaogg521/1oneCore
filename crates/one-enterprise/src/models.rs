//! Row types and API DTOs for one-enterprise tables.

use serde::Serialize;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EnterpriseRow {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub display_name: Option<String>,
    /// How this company was created: `'sso'` (derived from an SSO login's
    /// company id) or `'manual'` (explicitly set up by an operator). Added in
    /// migration `enterprise_002`.
    pub origin: String,
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

/// Company-tier (真实企业) member roles. Distinct from one-org's project-group
/// roles: a company administrator governs the company (members, project groups,
/// SSO policy), independent of any single project group's admin.
pub const ROLE_COMPANY_ADMIN: &str = "admin";
pub const ROLE_COMPANY_MEMBER: &str = "member";

pub fn is_company_admin_role(role: &str) -> bool {
    role == ROLE_COMPANY_ADMIN
}

/// The deployment's company as seen by a caller (Direction B, tier above
/// project groups). `viewer_role` is the caller's own membership role, or
/// `None` when they aren't a member.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanyOverviewDto {
    pub company_id: String,
    pub name: Option<String>,
    pub origin: String,
    pub member_count: i64,
    pub viewer_role: Option<String>,
}

/// A company member row for the company admin console.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanyMemberDto {
    pub user_id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    pub role: String,
}
