//! Business logic for the enterprise-org dimension. No axum imports.

use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::EnterpriseError;
use crate::models::EnterpriseIdentityDto;

pub struct EnterpriseService {
    pool: SqlitePool,
}

impl EnterpriseService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Upsert the SSO company and the caller's membership in it. Called at SSO
    /// login (via the `EnterpriseSync` hook wired in aionui-app). This is the
    /// enterprise-org counterpart to a project-group join — it reflects the
    /// user's real company from the IdP and never touches `one_tenants`.
    ///
    /// `external_id` is the IdP company id (Feishu `tenant_key`). Empty id =
    /// no company info from this login → no-op. `display_name` is the member's
    /// own name (e.g. 赵高); the company's own human-readable name isn't
    /// available from Feishu SSO, so `one_enterprises.display_name` stays NULL
    /// until a directory API can fill it.
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
        if external_id.is_empty() {
            return Ok(());
        }
        let now = now_ms() as i64;

        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM one_enterprises WHERE provider = ? AND external_id = ?")
                .bind(provider)
                .bind(external_id)
                .fetch_optional(&self.pool)
                .await?;
        let enterprise_id = match existing {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::now_v7().simple().to_string();
                sqlx::query(
                    "INSERT INTO one_enterprises (id, provider, external_id, display_name, created_at, updated_at) \
                     VALUES (?, ?, ?, NULL, ?, ?)",
                )
                .bind(&id)
                .bind(provider)
                .bind(external_id)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                id
            }
        };

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
        .bind(&enterprise_id)
        .bind(display_name)
        .bind(department)
        .bind(job_title)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        tracing::info!(user_id, provider, enterprise_id, "enterprise membership synced from SSO");
        Ok(())
    }

    /// The caller's own enterprise-org identity, or `None` if they have no
    /// enterprise membership (local/LDAP account, or hasn't logged in via an
    /// SSO company since this feature landed).
    pub async fn identity_of(&self, user_id: &str) -> Result<Option<EnterpriseIdentityDto>, EnterpriseError> {
        let row = sqlx::query_as::<
            _,
            (String, String, Option<String>, Option<String>, Option<String>, Option<String>, String),
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
        svc.sync_member("u1", "feishu", "tenant_huanle", Some("赵高"), Some("研发中心"), Some("工程师"))
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
        assert_eq!(svc.identity_of("u1").await.unwrap().unwrap().department.as_deref(), Some("研发"));
        assert_eq!(svc.identity_of("u2").await.unwrap().unwrap().department.as_deref(), Some("产品"));
    }

    #[tokio::test]
    async fn empty_company_id_is_a_noop() {
        let svc = service().await;
        svc.sync_member("u1", "feishu", "  ", Some("x"), None, None).await.unwrap();
        assert!(svc.identity_of("u1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn identity_of_is_none_without_membership() {
        let svc = service().await;
        assert!(svc.identity_of("nobody").await.unwrap().is_none());
    }
}
