//! Business logic for the enterprise-org dimension. No axum imports.

use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::EnterpriseError;
use crate::models::{
    CompanyMemberDto, CompanyOverviewDto, EnterpriseIdentityDto, ROLE_COMPANY_ADMIN, ROLE_COMPANY_MEMBER,
    is_company_admin_role,
};

/// Desktop-operator sentinel user id (mirrors `one_org::models::SYSTEM_DEFAULT_USER_ID`).
/// Defaults to system_admin when it has no explicit `one_user_org` row.
const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";
/// one-org's instance-level admin role (mirrors `one_org::models::ROLE_SYSTEM_ADMIN`).
const ROLE_SYSTEM_ADMIN: &str = "system_admin";

pub struct EnterpriseService {
    pool: SqlitePool,
}

impl EnterpriseService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Attach the caller's membership to the deployment's company at SSO login
    /// (via the `EnterpriseSync` hook wired in aionui-app). Never touches
    /// `one_tenants`. Which company they join:
    ///
    /// 1. If an operator explicitly set up a company ("显式设立"), that is THE
    ///    deployment company — every SSO login joins it, even when the IdP did
    ///    NOT return a company id. This makes the company robust to Feishu not
    ///    surfacing `tenant_key`, and keeps "one server = one company".
    /// 2. Otherwise fall back to the legacy SSO-derived company keyed on
    ///    `(provider, external_id=tenant_key)` — bootstraps a company from SSO
    ///    when no explicit one exists.
    /// 3. No explicit company AND no `external_id` → no-op. The personal /
    ///    standalone edition never reaches here (it has no SSO login at all),
    ///    so its behaviour is unchanged.
    ///
    /// The membership upsert deliberately does NOT touch `role`, so an operator
    /// who is already `admin` is never downgraded to `member` by a later login.
    pub async fn sync_member(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
    ) -> Result<(), EnterpriseError> {
        let external_id = external_id.trim();
        let now = now_ms() as i64;

        let enterprise_id = if let Some(id) = self.manual_company_id().await? {
            id
        } else if !external_id.is_empty() {
            self.upsert_sso_company(provider, external_id, now).await?
        } else {
            return Ok(());
        };

        self.upsert_member(user_id, &enterprise_id, display_name, department, job_title, now)
            .await?;
        tracing::info!(
            user_id,
            provider,
            enterprise_id,
            "enterprise membership synced from SSO"
        );
        Ok(())
    }

    /// The explicitly-set-up ("manual") company on this server, if any.
    async fn manual_company_id(&self) -> Result<Option<String>, EnterpriseError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM one_enterprises WHERE origin = 'manual' LIMIT 1")
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// The single company this deployment hosts (explicit preferred, else the
    /// oldest SSO-bootstrapped one). One server = one company.
    async fn deployment_company_id(&self) -> Result<Option<String>, EnterpriseError> {
        if let Some(id) = self.manual_company_id().await? {
            return Ok(Some(id));
        }
        Ok(
            sqlx::query_scalar("SELECT id FROM one_enterprises ORDER BY created_at ASC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Find-or-create the SSO-derived company for `(provider, external_id)`.
    async fn upsert_sso_company(&self, provider: &str, external_id: &str, now: i64) -> Result<String, EnterpriseError> {
        if let Some(id) =
            sqlx::query_scalar::<_, String>("SELECT id FROM one_enterprises WHERE provider = ? AND external_id = ?")
                .bind(provider)
                .bind(external_id)
                .fetch_optional(&self.pool)
                .await?
        {
            return Ok(id);
        }
        let id = uuid::Uuid::now_v7().simple().to_string();
        sqlx::query(
            "INSERT INTO one_enterprises (id, provider, external_id, display_name, origin, created_at, updated_at) \
             VALUES (?, ?, ?, NULL, 'sso', ?, ?)",
        )
        .bind(&id)
        .bind(provider)
        .bind(external_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Upsert a member WITHOUT touching `role` (preserves an existing admin).
    async fn upsert_member(
        &self,
        user_id: &str,
        enterprise_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
        now: i64,
    ) -> Result<(), EnterpriseError> {
        let display_name = display_name.map(str::trim).filter(|s| !s.is_empty());
        let department = department.map(str::trim).filter(|s| !s.is_empty());
        let job_title = job_title.map(str::trim).filter(|s| !s.is_empty());
        sqlx::query(
            "INSERT INTO one_enterprise_members \
             (user_id, enterprise_id, display_name, department, job_title, role, joined_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'member', ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET enterprise_id = excluded.enterprise_id, \
                 display_name = excluded.display_name, department = excluded.department, \
                 job_title = excluded.job_title, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(enterprise_id)
        .bind(display_name)
        .bind(department)
        .bind(job_title)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert a member with an EXPLICIT role (setup / role management).
    async fn upsert_member_role(
        &self,
        user_id: &str,
        enterprise_id: &str,
        role: &str,
        now: i64,
    ) -> Result<(), EnterpriseError> {
        sqlx::query(
            "INSERT INTO one_enterprise_members (user_id, enterprise_id, role, joined_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET enterprise_id = excluded.enterprise_id, \
                 role = excluded.role, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(enterprise_id)
        .bind(role)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- company tier (Direction B) ---

    /// True when the caller is a one-org system_admin. Cross-domain read of
    /// `one_user_org` (same precedent as one-org reading `one_sso_identities`,
    /// one-sso reading `one_user_org`): the desktop operator
    /// (`system_default_user`) is system_admin by default.
    async fn caller_is_system_admin(&self, user_id: &str) -> Result<bool, EnterpriseError> {
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM one_user_org WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match role {
            Some(r) => r == ROLE_SYSTEM_ADMIN,
            None => user_id == SYSTEM_DEFAULT_USER_ID,
        })
    }

    /// The company the caller belongs to (`one_enterprise_members.enterprise_id`).
    pub async fn company_of(&self, user_id: &str) -> Result<Option<String>, EnterpriseError> {
        Ok(
            sqlx::query_scalar("SELECT enterprise_id FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn member_role(&self, user_id: &str) -> Result<Option<String>, EnterpriseError> {
        Ok(
            sqlx::query_scalar("SELECT role FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// True when the caller is an admin of ANY company (for one-sso gating and
    /// the RequireCompanyAdmin extractor — v1 hosts a single company).
    pub async fn is_company_admin(&self, user_id: &str) -> Result<bool, EnterpriseError> {
        Ok(self
            .member_role(user_id)
            .await?
            .as_deref()
            .map(is_company_admin_role)
            .unwrap_or(false))
    }

    /// True when the caller is an admin of the specific `enterprise_id`.
    pub async fn is_company_admin_of(&self, user_id: &str, enterprise_id: &str) -> Result<bool, EnterpriseError> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM one_enterprise_members WHERE user_id = ? AND enterprise_id = ?")
                .bind(user_id)
                .bind(enterprise_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(role.as_deref().map(is_company_admin_role).unwrap_or(false))
    }

    /// 显式设立: a system_admin establishes the deployment's company by name and
    /// becomes its company admin. One server = one company. If an SSO login had
    /// already bootstrapped a nameless company, it is adopted (named + marked
    /// explicit) rather than duplicated.
    pub async fn setup_company(&self, user_id: &str, name_raw: &str) -> Result<CompanyOverviewDto, EnterpriseError> {
        if !self.caller_is_system_admin(user_id).await? {
            return Err(EnterpriseError::Forbidden(
                "Only system administrators can set up a company".into(),
            ));
        }
        let name = name_raw.trim();
        if name.is_empty() {
            return Err(EnterpriseError::NameRequired);
        }
        if self.manual_company_id().await?.is_some() {
            return Err(EnterpriseError::CompanyExists);
        }
        let now = now_ms() as i64;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM one_enterprises ORDER BY created_at ASC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        let enterprise_id = if let Some(id) = existing {
            sqlx::query("UPDATE one_enterprises SET display_name = ?, origin = 'manual', updated_at = ? WHERE id = ?")
                .bind(name)
                .bind(now)
                .bind(&id)
                .execute(&self.pool)
                .await?;
            id
        } else {
            let id = uuid::Uuid::now_v7().simple().to_string();
            sqlx::query(
                "INSERT INTO one_enterprises (id, provider, external_id, display_name, origin, created_at, updated_at) \
                 VALUES (?, 'manual', ?, ?, 'manual', ?, ?)",
            )
            .bind(&id)
            .bind(&id)
            .bind(name)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await?;
            id
        };
        self.upsert_member_role(user_id, &enterprise_id, ROLE_COMPANY_ADMIN, now)
            .await?;
        tracing::info!(user_id, enterprise_id, "company set up (显式设立)");
        self.company_overview(user_id)
            .await?
            .ok_or(EnterpriseError::CompanyNotFound)
    }

    /// The deployment's company as seen by `user_id` (their membership, else
    /// the deployment company), or `None` when no company exists.
    pub async fn company_overview(&self, user_id: &str) -> Result<Option<CompanyOverviewDto>, EnterpriseError> {
        let company_id = match self.company_of(user_id).await? {
            Some(id) => Some(id),
            None => self.deployment_company_id().await?,
        };
        let Some(company_id) = company_id else {
            return Ok(None);
        };
        let row: Option<(Option<String>, String)> =
            sqlx::query_as("SELECT display_name, origin FROM one_enterprises WHERE id = ?")
                .bind(&company_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((name, origin)) = row else {
            return Ok(None);
        };
        let member_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ?")
                .bind(&company_id)
                .fetch_one(&self.pool)
                .await?;
        let viewer_role = self.member_role(user_id).await?;
        Ok(Some(CompanyOverviewDto {
            company_id,
            name,
            origin,
            member_count,
            viewer_role,
        }))
    }

    /// All members of a company, for the admin console (LEFT JOIN upstream
    /// `users` for the login username).
    pub async fn list_members(&self, enterprise_id: &str) -> Result<Vec<CompanyMemberDto>, EnterpriseError> {
        let rows = sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
            ),
        >(
            "SELECT m.user_id, u.username, m.display_name, m.department, m.job_title, m.role \
             FROM one_enterprise_members m LEFT JOIN users u ON u.id = m.user_id \
             WHERE m.enterprise_id = ? ORDER BY m.joined_at ASC",
        )
        .bind(enterprise_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(user_id, username, display_name, department, job_title, role)| CompanyMemberDto {
                    user_id,
                    username,
                    display_name,
                    department,
                    job_title,
                    role,
                },
            )
            .collect())
    }

    /// Set a company member's role (admin/member). Never leaves the company
    /// with zero admins.
    pub async fn set_member_role(
        &self,
        enterprise_id: &str,
        target_user_id: &str,
        role_raw: &str,
    ) -> Result<(), EnterpriseError> {
        let role = role_raw.trim();
        if role != ROLE_COMPANY_ADMIN && role != ROLE_COMPANY_MEMBER {
            return Err(EnterpriseError::InvalidRole(role.to_string()));
        }
        let current: Option<String> =
            sqlx::query_scalar("SELECT role FROM one_enterprise_members WHERE user_id = ? AND enterprise_id = ?")
                .bind(target_user_id)
                .bind(enterprise_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(current) = current else {
            return Err(EnterpriseError::MemberNotFound);
        };
        if is_company_admin_role(&current) && role != ROLE_COMPANY_ADMIN {
            let admin_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ? AND role = ?")
                    .bind(enterprise_id)
                    .bind(ROLE_COMPANY_ADMIN)
                    .fetch_one(&self.pool)
                    .await?;
            if admin_count <= 1 {
                return Err(EnterpriseError::LastCompanyAdmin);
            }
        }
        let now = now_ms() as i64;
        sqlx::query(
            "UPDATE one_enterprise_members SET role = ?, updated_at = ? WHERE user_id = ? AND enterprise_id = ?",
        )
        .bind(role)
        .bind(now)
        .bind(target_user_id)
        .bind(enterprise_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The caller's own enterprise-org identity, or `None` if they have no
    /// enterprise membership (local/LDAP account, or hasn't logged in via an
    /// SSO company since this feature landed).
    pub async fn identity_of(&self, user_id: &str) -> Result<Option<EnterpriseIdentityDto>, EnterpriseError> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
            ),
        >(
            "SELECT e.provider, e.external_id, e.display_name, m.display_name, m.department, m.job_title, m.role \
             FROM one_enterprise_members m \
             JOIN one_enterprises e ON e.id = m.enterprise_id \
             WHERE m.user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(provider, company_id, company_name, display_name, department, job_title, role)| EnterpriseIdentityDto {
                provider,
                company_id,
                company_name,
                display_name,
                department,
                job_title,
                role,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service() -> EnterpriseService {
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_enterprise_migrations(db.pool()).await.unwrap();
        EnterpriseService::new(db.pool().clone())
    }

    #[tokio::test]
    async fn sync_member_then_identity_of_roundtrips() {
        let svc = service().await;
        svc.sync_member(
            "u1",
            "feishu",
            "tenant_huanle",
            Some("赵高"),
            Some("研发中心"),
            Some("工程师"),
        )
        .await
        .unwrap();

        let id = svc.identity_of("u1").await.unwrap().expect("identity present");
        assert_eq!(id.provider, "feishu");
        assert_eq!(id.company_id, "tenant_huanle");
        // Feishu doesn't surface the company's own name → stays None.
        assert_eq!(id.company_name, None);
        assert_eq!(id.display_name.as_deref(), Some("赵高"));
        assert_eq!(id.department.as_deref(), Some("研发中心"));
        assert_eq!(id.job_title.as_deref(), Some("工程师"));
        assert_eq!(id.role, "member");
    }

    #[tokio::test]
    async fn same_company_logins_converge_on_one_enterprise_row() {
        let svc = service().await;
        svc.sync_member("u1", "feishu", "tenant_huanle", None, Some("研发"), None)
            .await
            .unwrap();
        svc.sync_member("u2", "feishu", "tenant_huanle", None, Some("产品"), None)
            .await
            .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprises")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "same (provider, tenant_key) is one enterprise");
        assert_eq!(
            svc.identity_of("u1").await.unwrap().unwrap().department.as_deref(),
            Some("研发")
        );
        assert_eq!(
            svc.identity_of("u2").await.unwrap().unwrap().department.as_deref(),
            Some("产品")
        );
    }

    #[tokio::test]
    async fn empty_company_id_is_a_noop() {
        let svc = service().await;
        svc.sync_member("u1", "feishu", "  ", Some("x"), None, None)
            .await
            .unwrap();
        assert!(svc.identity_of("u1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn identity_of_is_none_without_membership() {
        let svc = service().await;
        assert!(svc.identity_of("nobody").await.unwrap().is_none());
    }

    // --- Direction B: company tier ---

    async fn insert_manual_company(svc: &EnterpriseService, id: &str, name: &str) {
        sqlx::query(
            "INSERT INTO one_enterprises (id, provider, external_id, display_name, origin, created_at, updated_at) \
             VALUES (?, 'manual', ?, ?, 'manual', 1, 1)",
        )
        .bind(id)
        .bind(id)
        .bind(name)
        .execute(&svc.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sync_member_attaches_to_manual_company_without_tenant_key() {
        // An explicitly set-up company makes every SSO login join it, even when
        // the IdP returned NO company id (the tenant_key-missing scenario).
        let svc = service().await;
        insert_manual_company(&svc, "ent1", "Acme").await;
        svc.sync_member("u1", "feishu", "", Some("赵高"), None, None)
            .await
            .unwrap();
        assert_eq!(svc.company_of("u1").await.unwrap().as_deref(), Some("ent1"));
        // No spurious SSO-derived company was created.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprises")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn sync_member_never_downgrades_admin() {
        let svc = service().await;
        insert_manual_company(&svc, "ent1", "Acme").await;
        // Seed the operator as company admin, then a later SSO login must not
        // downgrade them to member.
        svc.upsert_member_role("op", "ent1", ROLE_COMPANY_ADMIN, 1)
            .await
            .unwrap();
        svc.sync_member("op", "feishu", "", Some("Op"), None, None)
            .await
            .unwrap();
        assert!(svc.is_company_admin("op").await.unwrap());
    }

    #[tokio::test]
    async fn sync_member_without_company_is_noop() {
        // Lock-in: no explicit company AND no tenant_key → nothing written. This
        // is the personal / standalone path (which never reaches SSO anyway).
        let svc = service().await;
        svc.sync_member("u1", "feishu", "", Some("x"), None, None)
            .await
            .unwrap();
        assert!(svc.identity_of("u1").await.unwrap().is_none());
        assert!(svc.company_of("u1").await.unwrap().is_none());
    }

    // Governance-aware setup: adds the cross-domain `one_user_org` table so the
    // system_admin check resolves (the `users` table is created by
    // init_database_memory). Mirrors the real multi-crate DB.
    async fn service_with_governance() -> EnterpriseService {
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_enterprise_migrations(db.pool()).await.unwrap();
        sqlx::query(
            "CREATE TABLE one_user_org (user_id TEXT PRIMARY KEY, tenant_id TEXT, role TEXT NOT NULL DEFAULT 'member')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        EnterpriseService::new(db.pool().clone())
    }

    #[tokio::test]
    async fn setup_company_seeds_creator_as_admin() {
        let svc = service_with_governance().await;
        // system_default_user is system_admin by default (no one_user_org row).
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        assert_eq!(overview.name.as_deref(), Some("Acme"));
        assert_eq!(overview.origin, "manual");
        assert_eq!(overview.member_count, 1);
        assert_eq!(overview.viewer_role.as_deref(), Some("admin"));
        assert!(svc.is_company_admin("system_default_user").await.unwrap());
    }

    #[tokio::test]
    async fn setup_company_rejected_for_non_admin() {
        let svc = service_with_governance().await;
        sqlx::query("INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('bob', 't1', 'member')")
            .execute(&svc.pool)
            .await
            .unwrap();
        let err = svc.setup_company("bob", "Acme").await.unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
    }

    #[tokio::test]
    async fn second_company_rejected() {
        let svc = service_with_governance().await;
        svc.setup_company("system_default_user", "Acme").await.unwrap();
        let err = svc.setup_company("system_default_user", "Beta").await.unwrap_err();
        assert_eq!(err.code(), "COMPANY_ALREADY_EXISTS");
    }

    #[tokio::test]
    async fn members_listed_and_role_managed_with_last_admin_guard() {
        let svc = service_with_governance().await;
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;
        // A second SSO member joins the (manual) company.
        svc.sync_member("u2", "feishu", "", Some("Bob"), None, None)
            .await
            .unwrap();
        let members = svc.list_members(&ent).await.unwrap();
        assert_eq!(members.len(), 2);
        // Promote u2, then the last-admin guard blocks demoting the sole-remaining
        // admin path.
        svc.set_member_role(&ent, "u2", "admin").await.unwrap();
        assert!(svc.is_company_admin("u2").await.unwrap());
        svc.set_member_role(&ent, "u2", "member").await.unwrap();
        // Now only system_default_user is admin — demoting it must fail.
        let err = svc
            .set_member_role(&ent, "system_default_user", "member")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "LAST_COMPANY_ADMIN");
        // Unknown target → 404.
        let err = svc.set_member_role(&ent, "ghost", "admin").await.unwrap_err();
        assert_eq!(err.code(), "COMPANY_MEMBER_NOT_FOUND");
    }
}
