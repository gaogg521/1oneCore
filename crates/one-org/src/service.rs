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
    ResetLocalResult, RuntimeNodeDto, RuntimeNodeRow, SYSTEM_DEFAULT_USER_ID, TenantRow, UserOrgRow, is_admin_role,
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
        Self {
            pool,
            user_repo,
            data_dir,
        }
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

    /// Best-effort read of the most recent SSO profile snapshot for a user
    /// (`display_name`, `org_unit_path`, `job_title`, provider) — `one-org`
    /// doesn't own `one_sso_identities` (`one-sso` does, and same-layer
    /// domain crates can't depend on each other per the workspace layering
    /// rules) but reads it directly here, mirroring the precedent in
    /// `one-sso::SsoService::effective_role` reading `one_user_org`. Returns
    /// `None` for locally-created members with no SSO identity at all — that
    /// query failing entirely (e.g. table not yet migrated in some odd test
    /// setup) degrades the same way, rather than blocking the join/create.
    async fn sso_profile_for(&self, user_id: &str) -> Option<(Option<String>, Option<String>, Option<String>, String)> {
        sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>, String)>(
            "SELECT display_name, org_unit_path, job_title, provider FROM one_sso_identities \
             WHERE user_id = ? ORDER BY last_seen_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
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
        // Snapshot the joiner's SSO profile (if any) onto the new membership
        // row — name/department/job title extracted from the identity
        // provider at login has nowhere else to live once someone actually
        // becomes a tenant member. Locally-created members (no SSO identity)
        // get NULLs here, same as before this fix.
        let (display_name, org_unit_path, job_title, org_profile_source) = match self.sso_profile_for(user_id).await {
            Some((d, o, j, p)) => (d, o, j, Some(p)),
            None => (None, None, None, None),
        };
        let org_profile_synced_at = org_profile_source.as_ref().map(|_| now);

        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE one_tenant_invites SET use_count = use_count + 1 WHERE id = ?")
            .bind(&invite.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO one_user_org \
             (user_id, tenant_id, role, display_name, org_unit_path, job_title, org_profile_source, \
              org_profile_synced_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at, \
                 display_name = excluded.display_name, org_unit_path = excluded.org_unit_path, \
                 job_title = excluded.job_title, org_profile_source = excluded.org_profile_source, \
                 org_profile_synced_at = excluded.org_profile_synced_at",
        )
        .bind(user_id)
        .bind(&invite.tenant_id)
        .bind(ROLE_MEMBER)
        .bind(&display_name)
        .bind(&org_unit_path)
        .bind(&job_title)
        .bind(&org_profile_source)
        .bind(org_profile_synced_at)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.invalidate_user_tokens(user_id).await?;
        let username = self.lookup_username(user_id).await;
        self.audit(
            &invite.tenant_id,
            Some(user_id),
            username.as_deref(),
            "org.join",
            Some(&invite.id),
        )
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
        // Same SSO-profile snapshot as join_with_invite — see its comment.
        // Uncommon (the creator is usually already authenticated locally as
        // system_admin before creating the tenant) but cheap to keep
        // consistent.
        let (display_name, org_unit_path, job_title, org_profile_source) = match self.sso_profile_for(user_id).await {
            Some((d, o, j, p)) => (d, o, j, Some(p)),
            None => (None, None, None, None),
        };
        let org_profile_synced_at = org_profile_source.as_ref().map(|_| now);

        // Creator keeps system_admin (instance-level governance) — same
        // rationale as the TS reference: downgrading to org_admin here would
        // leave the instance with no system_admin.
        // Bind the tenant to the creator's SSO company (Feishu tenant_key etc.)
        // when they signed in through an IdP. That binding is what lets later
        // same-company SSO logins auto-join without an invite code
        // (`auto_provision_enterprise`). Locally-created enterprises (no SSO
        // identity) leave it NULL and stay invite-only — a project group.
        let sso_binding = self.sso_org_binding_for(user_id).await;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO one_tenants (id, name, sso_provider, sso_org_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&tenant_id)
        .bind(name)
        .bind(sso_binding.as_ref().map(|(p, _)| p.as_str()))
        .bind(sso_binding.as_ref().map(|(_, o)| o.as_str()))
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO one_user_org \
             (user_id, tenant_id, role, display_name, org_unit_path, job_title, org_profile_source, \
              org_profile_synced_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
                 tenant_id = excluded.tenant_id, role = excluded.role, updated_at = excluded.updated_at, \
                 display_name = excluded.display_name, org_unit_path = excluded.org_unit_path, \
                 job_title = excluded.job_title, org_profile_source = excluded.org_profile_source, \
                 org_profile_synced_at = excluded.org_profile_synced_at",
        )
        .bind(user_id)
        .bind(&tenant_id)
        .bind(ROLE_SYSTEM_ADMIN)
        .bind(&display_name)
        .bind(&org_unit_path)
        .bind(&job_title)
        .bind(&org_profile_source)
        .bind(org_profile_synced_at)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.invalidate_user_tokens(user_id).await?;
        let username = self.lookup_username(user_id).await;
        self.audit(&tenant_id, Some(user_id), username.as_deref(), "org.create", Some(name))
            .await;

        Ok((tenant_id, name.to_string()))
    }

    /// The company (provider, org_external_id) the user's most recent SSO
    /// identity belongs to, if any. `None` for local/LDAP accounts and for
    /// providers that don't surface a company id.
    async fn sso_org_binding_for(&self, user_id: &str) -> Option<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT provider, org_external_id FROM one_sso_identities \
             WHERE user_id = ? AND org_external_id IS NOT NULL AND org_external_id <> '' \
             ORDER BY last_seen_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
    }

    /// Auto-join the enterprise bound to this SSO company — the "real
    /// enterprise" tier: a colleague who signs in with the company IdP lands in
    /// the same tenant with their real name/department, no invite code needed.
    ///
    /// Deliberately **join-only: it never creates a tenant.** Creating an
    /// enterprise stays an explicit admin action (`create_tenant`, which is what
    /// records the company binding). Auto-creating here would silently turn a
    /// standalone install into an enterprise server the moment someone signed in
    /// with SSO — a behavior change for personal-edition users, which is out of
    /// bounds. No binding match (different company, an invite-only project
    /// group, or no enterprise at all) => no change; the user simply stays where
    /// they were.
    ///
    /// Returns whether a membership was actually written.
    pub async fn auto_provision_enterprise(
        &self,
        user_id: &str,
        provider: &str,
        org_external_id: &str,
    ) -> Result<bool, OrgError> {
        let org_external_id = org_external_id.trim();
        if org_external_id.is_empty() {
            return Ok(false);
        }
        // Already in an enterprise (this one or another) — leave membership alone.
        let current = self.tenant_of(user_id).await?;
        if is_enterprise_tenant_id(&current) {
            return Ok(false);
        }

        let tenant_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM one_tenants WHERE sso_provider = ? AND sso_org_id = ? LIMIT 1")
                .bind(provider)
                .bind(org_external_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(tenant_id) = tenant_id else {
            return Ok(false);
        };

        let now = now_ms() as i64;
        // Same SSO-profile snapshot as join_with_invite — see its comment.
        let (display_name, org_unit_path, job_title, org_profile_source) = match self.sso_profile_for(user_id).await {
            Some((d, o, j, p)) => (d, o, j, Some(p)),
            None => (None, None, None, None),
        };
        let org_profile_synced_at = org_profile_source.as_ref().map(|_| now);

        sqlx::query(
            "INSERT INTO one_user_org \
             (user_id, tenant_id, role, display_name, org_unit_path, job_title, org_profile_source, \
              org_profile_synced_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at, \
                 display_name = excluded.display_name, org_unit_path = excluded.org_unit_path, \
                 job_title = excluded.job_title, org_profile_source = excluded.org_profile_source, \
                 org_profile_synced_at = excluded.org_profile_synced_at",
        )
        .bind(user_id)
        .bind(&tenant_id)
        .bind(ROLE_MEMBER)
        .bind(&display_name)
        .bind(&org_unit_path)
        .bind(&job_title)
        .bind(&org_profile_source)
        .bind(org_profile_synced_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.invalidate_user_tokens(user_id).await?;
        let username = self.lookup_username(user_id).await;
        self.audit(
            &tenant_id,
            Some(user_id),
            username.as_deref(),
            "org.sso_auto_join",
            Some(provider),
        )
        .await;
        tracing::info!(
            user_id,
            provider,
            tenant_id,
            "SSO auto-joined enterprise by company binding"
        );

        Ok(true)
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
        let username = self.lookup_username(user_id).await;
        self.audit(
            DEFAULT_TENANT_ID,
            Some(user_id),
            username.as_deref(),
            "org.reset_local",
            Some(&archive_path_str),
        )
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
        if let Some(hash) = tenant.and_then(|t| t.exit_password_hash)
            && !verify_password(exit_code, &hash)?
        {
            return Err(OrgError::WrongExitCode);
        }

        if is_admin_role(&membership.role) {
            self.ensure_not_last_admin(&membership.tenant_id, user_id).await?;
        }

        sqlx::query("DELETE FROM one_user_org WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        self.invalidate_user_tokens(user_id).await?;
        let username = self.lookup_username(user_id).await;
        self.audit(
            &membership.tenant_id,
            Some(user_id),
            username.as_deref(),
            "org.exit",
            None,
        )
        .await;
        Ok(())
    }

    /// Reject an admin removal/demotion when it would leave the tenant with
    /// zero admins while other members remain — with no admin left, no one
    /// can invite, configure SSO, or promote a replacement, permanently
    /// orphaning the tenant. Allowed when the departing admin is also the
    /// tenant's last member (the tenant simply becomes empty, not orphaned).
    async fn ensure_not_last_admin(&self, tenant_id: &str, excluding_user_id: &str) -> Result<(), OrgError> {
        let remaining_admins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM one_user_org \
             WHERE tenant_id = ? AND user_id != ? AND role IN ('system_admin', 'org_admin', 'admin')",
        )
        .bind(tenant_id)
        .bind(excluding_user_id)
        .fetch_one(&self.pool)
        .await?;
        if remaining_admins > 0 {
            return Ok(());
        }

        let remaining_members: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM one_user_org WHERE tenant_id = ? AND user_id != ?")
                .bind(tenant_id)
                .bind(excluding_user_id)
                .fetch_one(&self.pool)
                .await?;
        if remaining_members == 0 {
            return Ok(());
        }

        Err(OrgError::LastAdminCannotLeave)
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
        let membership = self.membership(user_id).await?;
        let tenant_id = membership
            .as_ref()
            .map(|m| m.tenant_id.clone())
            .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string());
        let role = self.effective_role(user_id).await?;
        let is_enterprise = is_enterprise_tenant_id(&tenant_id);
        // Enterprise-only fields stay `None`/`false`/`0` in personal edition so
        // the client's personal-mode rendering is byte-identical to before.
        let (tenant_name, member_count, sso_bound, display_name, org_unit_path, job_title) = if is_enterprise {
            let tenant = self.get_tenant(&tenant_id).await?;
            let name = tenant.as_ref().map(|t| t.name.clone());
            let sso_bound = tenant.as_ref().is_some_and(|t| t.is_sso_bound());
            let count = self.member_count(&tenant_id).await?;
            let (display_name, org_unit_path, job_title) = membership
                .as_ref()
                .map(|m| (m.display_name.clone(), m.org_unit_path.clone(), m.job_title.clone()))
                .unwrap_or((None, None, None));
            (name, count, sso_bound, display_name, org_unit_path, job_title)
        } else {
            (None, 0, false, None, None, None)
        };
        Ok(OrgContextDto {
            tenant_id,
            tenant_name,
            role,
            is_enterprise,
            member_count,
            sso_bound,
            display_name,
            org_unit_path,
            job_title,
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

    /// Best-effort audit write; failures are logged, never surfaced. The
    /// `username` column exists precisely so the audit tab reads as "who did
    /// this" without a raw user id — callers should always resolve it via
    /// `lookup_username`/an already-in-scope actor username rather than
    /// leaving it `None`.
    pub async fn audit(
        &self,
        tenant_id: &str,
        user_id: Option<&str>,
        username: Option<&str>,
        action: &str,
        resource: Option<&str>,
    ) {
        let result = sqlx::query(
            "INSERT INTO one_audit_logs (id, tenant_id, user_id, username, action, resource, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(short_id("audit"))
        .bind(tenant_id)
        .bind(user_id)
        .bind(username)
        .bind(action)
        .bind(resource)
        .bind(now_ms() as i64)
        .execute(&self.pool)
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, action, "one-org audit write failed");
        }
    }

    /// Resolve a user id to its display username for an audit entry.
    /// Best-effort: an unresolvable id (deleted user, bad data) just leaves
    /// the audit row's username blank rather than failing the whole action.
    async fn lookup_username(&self, user_id: &str) -> Option<String> {
        self.user_repo
            .find_by_id(user_id)
            .await
            .ok()
            .flatten()
            .map(|u| u.username)
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
            "SELECT uo.user_id, u.username, uo.tenant_id, uo.role, uo.display_name, uo.org_unit_path, \
                    uo.job_title, u.last_login, uo.created_at \
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
    /// `actor_user_id` is the admin performing the change; `target_user_id`
    /// is whose role is being changed. They differ in the common case (an
    /// admin promotes/demotes someone else), so the audit row below must
    /// attribute the action to the actor — see the doc comment on `audit`.
    pub async fn set_user_role(
        &self,
        tenant_id: &str,
        actor_user_id: &str,
        target_user_id: &str,
        role: &str,
    ) -> Result<(), OrgError> {
        // Demoting the tenant's last admin to a non-admin role would leave no
        // one who can invite, configure SSO, or promote a replacement —
        // same guard as `leave()`.
        let current_role: Option<String> =
            sqlx::query_scalar("SELECT role FROM one_user_org WHERE tenant_id = ? AND user_id = ?")
                .bind(tenant_id)
                .bind(target_user_id)
                .fetch_optional(&self.pool)
                .await?;
        if current_role.is_some_and(|r| is_admin_role(&r)) && !is_admin_role(role) {
            self.ensure_not_last_admin(tenant_id, target_user_id).await?;
        }

        let result =
            sqlx::query("UPDATE one_user_org SET role = ?, updated_at = ? WHERE tenant_id = ? AND user_id = ?")
                .bind(role)
                .bind(now_ms() as i64)
                .bind(tenant_id)
                .bind(target_user_id)
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(OrgError::BadRequest(format!(
                "user {target_user_id} not in tenant {tenant_id}"
            )));
        }
        // Note: upstream `users` table has no role column — role lives
        // exclusively in `one_user_org`. The auth middleware's role check
        // reads from `CurrentUser`, which is populated from the JWT payload
        // (no role). RBAC for `/api/one/*` is handled by the `RequireOrgAdmin`
        // extractor reading `one_user_org` directly.
        //
        // Audit row is attributed to the ACTOR (who made the change), not the
        // target — the target + new role go into `resource` instead. Getting
        // this backwards would make every promotion/demotion look
        // self-inflicted in the audit log, hiding who actually did it.
        let actor_username = self.lookup_username(actor_user_id).await;
        self.audit(
            tenant_id,
            Some(actor_user_id),
            actor_username.as_deref(),
            "set_role",
            Some(&format!("user={target_user_id} role={role}")),
        )
        .await;
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
    use crate::models::ROLE_ORG_ADMIN;
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

    /// `one_sso_identities` is one-sso's table, not one-org's — recreate the
    /// minimal shape here (same pattern one-sso's own tests use for
    /// `one_user_org`) so `sso_profile_for` has something to read.
    async fn seed_sso_identity(
        pool: &SqlitePool,
        user_id: &str,
        display_name: &str,
        org_unit_path: &str,
        job_title: &str,
    ) {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS one_sso_identities (\
                 id TEXT PRIMARY KEY, provider TEXT NOT NULL, external_id TEXT NOT NULL, \
                 user_id TEXT NOT NULL, tenant_id TEXT NOT NULL DEFAULT 'default', \
                 display_name TEXT, org_unit_path TEXT, job_title TEXT, org_external_id TEXT, \
                 last_seen_at INTEGER, created_at INTEGER NOT NULL)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO one_sso_identities \
             (id, provider, external_id, user_id, display_name, org_unit_path, job_title, created_at, last_seen_at) \
             VALUES (?, 'feishu', ?, ?, ?, ?, ?, 0, 0)",
        )
        .bind(uuid::Uuid::now_v7().simple().to_string())
        .bind(format!("ext_{user_id}"))
        .bind(user_id)
        .bind(display_name)
        .bind(org_unit_path)
        .bind(job_title)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Like `seed_sso_identity`, but also records the IdP company id — the
    /// value the enterprise binding and auto-join match on.
    async fn seed_sso_identity_with_company(
        pool: &SqlitePool,
        user_id: &str,
        display_name: &str,
        org_unit_path: &str,
        company: &str,
    ) {
        seed_sso_identity(pool, user_id, display_name, org_unit_path, "工程师").await;
        sqlx::query("UPDATE one_sso_identities SET org_external_id = ? WHERE user_id = ?")
            .bind(company)
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Insert an enterprise already bound to a company, the way `create_tenant`
    /// records it when its creator signed in through an IdP.
    async fn seed_bound_enterprise(pool: &SqlitePool, tenant_id: &str, company: &str) {
        sqlx::query(
            "INSERT INTO one_tenants (id, name, sso_provider, sso_org_id, created_at, updated_at) \
             VALUES (?, '欢乐互娱', 'feishu', ?, 0, 0)",
        )
        .bind(tenant_id)
        .bind(company)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Creating an enterprise while signed in through an IdP records the
    /// creator's company on the tenant — that binding is what later
    /// same-company logins auto-join against.
    #[tokio::test]
    async fn create_tenant_binds_the_creators_sso_company() {
        let (db, service, _user_repo) = setup().await;
        seed_sso_identity_with_company(db.pool(), SYSTEM_DEFAULT_USER_ID, "老板", "总裁办", "tenant_huanle").await;

        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "欢乐互娱").await.unwrap();

        let (provider, company): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT sso_provider, sso_org_id FROM one_tenants WHERE id = ?")
                .bind(&tenant_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(provider.as_deref(), Some("feishu"));
        assert_eq!(company.as_deref(), Some("tenant_huanle"));
    }

    /// A locally-created enterprise (no SSO identity) stays unbound — it is an
    /// invite-only project group, and no SSO login can auto-join it.
    #[tokio::test]
    async fn create_tenant_leaves_the_binding_null_without_an_sso_identity() {
        let (db, service, _user_repo) = setup().await;

        let (tenant_id, _) = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "本地项目组")
            .await
            .unwrap();

        let (provider, company): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT sso_provider, sso_org_id FROM one_tenants WHERE id = ?")
                .bind(&tenant_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(provider, None);
        assert_eq!(company, None);
    }

    /// Red line: a personal-edition install must never silently become an
    /// enterprise server just because someone signed in with SSO. Auto-join is
    /// join-only — with no enterprise bound to the company, nothing changes.
    #[tokio::test]
    async fn auto_provision_enterprise_never_creates_a_tenant() {
        let (db, service, user_repo) = setup().await;
        let user = create_user(&user_repo, "zhaogao").await;
        seed_sso_identity_with_company(db.pool(), &user, "赵高", "研发中心", "tenant_huanle").await;

        let joined = service
            .auto_provision_enterprise(&user, "feishu", "tenant_huanle")
            .await
            .unwrap();

        assert!(!joined, "no enterprise is bound to this company yet");
        let tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_tenants")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(tenants, 0, "auto-join must never create a tenant");
        assert_eq!(service.tenant_of(&user).await.unwrap(), DEFAULT_TENANT_ID);
    }

    #[tokio::test]
    async fn auto_provision_enterprise_joins_the_enterprise_bound_to_the_same_company() {
        let (db, service, user_repo) = setup().await;
        seed_bound_enterprise(db.pool(), "tenant_x", "tenant_huanle").await;
        let user = create_user(&user_repo, "zhaogao").await;
        seed_sso_identity_with_company(db.pool(), &user, "赵高", "研发中心", "tenant_huanle").await;

        let joined = service
            .auto_provision_enterprise(&user, "feishu", "tenant_huanle")
            .await
            .unwrap();

        assert!(joined, "same-company SSO login joins without an invite code");
        let ctx = service.context(&user).await.unwrap();
        assert!(ctx.is_enterprise);
        assert_eq!(ctx.tenant_id, "tenant_x");
        assert_eq!(ctx.role, ROLE_MEMBER);
        // A company-bound tenant is a "real enterprise", and the member's real
        // name / department ride onto the context so the client can show them.
        assert!(ctx.sso_bound, "an SSO-company-bound tenant is a real enterprise");
        assert_eq!(ctx.display_name.as_deref(), Some("赵高"));
        assert_eq!(ctx.org_unit_path.as_deref(), Some("研发中心"));
    }

    /// The context of a member in an invite-only project group reports
    /// `sso_bound: false` so the client labels it a project group, not a real
    /// enterprise — even though it is still `is_enterprise` (a non-default
    /// tenant).
    #[tokio::test]
    async fn context_reports_project_group_as_not_sso_bound() {
        let (db, service, user_repo) = setup().await;
        sqlx::query("INSERT INTO one_tenants (id, name, created_at, updated_at) VALUES ('tenant_pg', '项目组', 0, 0)")
            .execute(db.pool())
            .await
            .unwrap();
        let user = create_user(&user_repo, "member1").await;
        sqlx::query(
            "INSERT INTO one_user_org (user_id, tenant_id, role, created_at, updated_at) \
             VALUES (?, 'tenant_pg', 'member', 0, 0)",
        )
        .bind(&user)
        .execute(db.pool())
        .await
        .unwrap();

        let ctx = service.context(&user).await.unwrap();
        assert!(ctx.is_enterprise, "a non-default tenant is still an enterprise tenant");
        assert!(!ctx.sso_bound, "an invite-only project group is not SSO-bound");
        assert_eq!(ctx.tenant_name.as_deref(), Some("项目组"));
    }

    /// Personal edition (no membership row) reports everything empty — the
    /// red line that this endpoint's personal-mode shape is unchanged.
    #[tokio::test]
    async fn context_in_personal_edition_is_empty() {
        let (_db, service, user_repo) = setup().await;
        let user = create_user(&user_repo, "solo").await;

        let ctx = service.context(&user).await.unwrap();
        assert_eq!(ctx.tenant_id, DEFAULT_TENANT_ID);
        assert!(!ctx.is_enterprise);
        assert!(!ctx.sso_bound);
        assert_eq!(ctx.member_count, 0);
        assert!(ctx.display_name.is_none());
        assert!(ctx.org_unit_path.is_none());
        assert!(ctx.job_title.is_none());
    }

    #[tokio::test]
    async fn auto_provision_enterprise_ignores_a_different_company() {
        let (db, service, user_repo) = setup().await;
        seed_bound_enterprise(db.pool(), "tenant_x", "tenant_huanle").await;
        let outsider = create_user(&user_repo, "stranger").await;
        seed_sso_identity_with_company(db.pool(), &outsider, "路人", "外部", "tenant_other_corp").await;

        let joined = service
            .auto_provision_enterprise(&outsider, "feishu", "tenant_other_corp")
            .await
            .unwrap();

        assert!(!joined, "another company's employee must not land in this enterprise");
        assert_eq!(service.tenant_of(&outsider).await.unwrap(), DEFAULT_TENANT_ID);
    }

    /// An invite-code project group carries no company binding, so SSO logins
    /// never auto-join it — it stays invite-only.
    #[tokio::test]
    async fn auto_provision_enterprise_skips_an_unbound_project_group() {
        let (db, service, user_repo) = setup().await;
        sqlx::query("INSERT INTO one_tenants (id, name, created_at, updated_at) VALUES ('tenant_pg', '项目组', 0, 0)")
            .execute(db.pool())
            .await
            .unwrap();
        let user = create_user(&user_repo, "zhaogao").await;
        seed_sso_identity_with_company(db.pool(), &user, "赵高", "研发中心", "tenant_huanle").await;

        let joined = service
            .auto_provision_enterprise(&user, "feishu", "tenant_huanle")
            .await
            .unwrap();

        assert!(!joined);
        assert_eq!(service.tenant_of(&user).await.unwrap(), DEFAULT_TENANT_ID);
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
    async fn join_with_invite_copies_the_joiner_sso_profile_onto_the_membership_row() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme Inc").await.unwrap();
        let (_, display) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();

        let member = create_user(&user_repo, "member1").await;
        seed_sso_identity(&service.pool, &member, "张三", "研发中心", "高级工程师").await;

        service.join_with_invite(&member, &display).await.unwrap();

        let users = service.list_users(&tenant_id).await.unwrap();
        let joined = users
            .iter()
            .find(|u| u.user_id == member)
            .expect("member should be listed");
        assert_eq!(joined.display_name.as_deref(), Some("张三"));
        assert_eq!(joined.org_unit_path.as_deref(), Some("研发中心"));
        assert_eq!(joined.job_title.as_deref(), Some("高级工程师"));

        db.close().await;
    }

    #[tokio::test]
    async fn join_with_invite_leaves_the_profile_null_for_a_locally_created_member() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme Inc").await.unwrap();
        let (_, display) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();

        // No matching one_sso_identities row at all for this member.
        let member = create_user(&user_repo, "local_member").await;
        service.join_with_invite(&member, &display).await.unwrap();

        let users = service.list_users(&tenant_id).await.unwrap();
        let joined = users
            .iter()
            .find(|u| u.user_id == member)
            .expect("member should be listed");
        assert_eq!(joined.display_name, None);
        assert_eq!(joined.org_unit_path, None);
        assert_eq!(joined.job_title, None);

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

    #[tokio::test]
    async fn last_admin_cannot_leave_while_other_members_remain() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(&user_repo, "member1").await;
        service.join_with_invite(&member, &code).await.unwrap();

        // SYSTEM_DEFAULT_USER_ID is the tenant's sole admin; member1 is a
        // plain member. Leaving now would orphan member1 with no one who can
        // invite, configure SSO, or promote a replacement.
        let err = service.leave(SYSTEM_DEFAULT_USER_ID, "").await.unwrap_err();
        assert_eq!(err.code(), "LAST_ADMIN_CANNOT_LEAVE");
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 2);

        db.close().await;
    }

    #[tokio::test]
    async fn admin_can_leave_when_another_admin_remains() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(&user_repo, "member1").await;
        service.join_with_invite(&member, &code).await.unwrap();
        service
            .set_user_role(&tenant_id, SYSTEM_DEFAULT_USER_ID, &member, ROLE_ORG_ADMIN)
            .await
            .unwrap();

        // Two admins now — SYSTEM_DEFAULT_USER_ID leaving is fine, member1
        // stays behind as org_admin.
        service.leave(SYSTEM_DEFAULT_USER_ID, "").await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 1);

        db.close().await;
    }

    #[tokio::test]
    async fn last_admin_can_leave_when_no_other_members_remain() {
        let (db, service, _user_repo) = setup().await;
        service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        // Sole admin, sole member — leaving just empties the tenant, no one
        // is orphaned.
        service.leave(SYSTEM_DEFAULT_USER_ID, "").await.unwrap();

        db.close().await;
    }

    #[tokio::test]
    async fn cannot_demote_last_admin_to_member() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(&user_repo, "member1").await;
        service.join_with_invite(&member, &code).await.unwrap();

        // Demoting the sole admin (SYSTEM_DEFAULT_USER_ID) to member would
        // leave member1 with no admin at all.
        let err = service
            .set_user_role(&tenant_id, SYSTEM_DEFAULT_USER_ID, SYSTEM_DEFAULT_USER_ID, ROLE_MEMBER)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "LAST_ADMIN_CANNOT_LEAVE");
        assert_eq!(
            service.effective_role(SYSTEM_DEFAULT_USER_ID).await.unwrap(),
            ROLE_SYSTEM_ADMIN
        );

        db.close().await;
    }

    #[tokio::test]
    async fn can_demote_admin_when_another_admin_remains() {
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(&user_repo, "member1").await;
        service.join_with_invite(&member, &code).await.unwrap();
        service
            .set_user_role(&tenant_id, SYSTEM_DEFAULT_USER_ID, &member, ROLE_ORG_ADMIN)
            .await
            .unwrap();

        // Two admins — demoting member1 back to plain member is fine since
        // SYSTEM_DEFAULT_USER_ID is still an admin.
        service
            .set_user_role(&tenant_id, SYSTEM_DEFAULT_USER_ID, &member, ROLE_MEMBER)
            .await
            .unwrap();
        assert_eq!(service.effective_role(&member).await.unwrap(), ROLE_MEMBER);

        db.close().await;
    }

    #[tokio::test]
    async fn audit_log_records_actor_username() {
        // `username` used to be left NULL on every write (the column existed
        // but no INSERT ever populated it) — the audit tab could only show a
        // raw user id, never a name. `ensure_system_user` seeds
        // SYSTEM_DEFAULT_USER_ID with username "admin".
        let (db, service, _user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        let logs = service.list_audit_logs(&tenant_id, 10).await.unwrap();
        let create_entry = logs.iter().find(|l| l.action == "org.create").unwrap();
        assert_eq!(create_entry.user_id.as_deref(), Some(SYSTEM_DEFAULT_USER_ID));
        assert_eq!(create_entry.username.as_deref(), Some("admin"));

        db.close().await;
    }

    #[tokio::test]
    async fn set_user_role_audit_attributes_to_actor_not_target() {
        // The audit row for a role change used to be attributed to the
        // TARGET user (whose role changed), not the ADMIN who changed it —
        // making every promotion/demotion look self-inflicted in the log.
        let (db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(&user_repo, "member1").await;
        service.join_with_invite(&member, &code).await.unwrap();

        service
            .set_user_role(&tenant_id, SYSTEM_DEFAULT_USER_ID, &member, ROLE_ORG_ADMIN)
            .await
            .unwrap();

        let logs = service.list_audit_logs(&tenant_id, 10).await.unwrap();
        let role_entry = logs.iter().find(|l| l.action == "set_role").unwrap();
        // Attributed to the actor (SYSTEM_DEFAULT_USER_ID/"admin")...
        assert_eq!(role_entry.user_id.as_deref(), Some(SYSTEM_DEFAULT_USER_ID));
        assert_eq!(role_entry.username.as_deref(), Some("admin"));
        // ...not the target member, whose id/role instead land in `resource`.
        assert_ne!(role_entry.user_id.as_deref(), Some(member.as_str()));
        let resource = role_entry.resource.as_deref().unwrap();
        assert!(
            resource.contains(&member),
            "resource should name the target: {resource}"
        );
        assert!(
            resource.contains(ROLE_ORG_ADMIN),
            "resource should name the new role: {resource}"
        );

        db.close().await;
    }
}
