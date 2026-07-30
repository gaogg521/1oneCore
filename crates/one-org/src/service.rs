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
use aionui_common::license::{Feature, Tier, tier_allows};
use aionui_common::{decrypt_string, encrypt_string, now_ms};
use aionui_db::IUserRepository;

use crate::email::{EmailSender, SendEmailResult, StubEmailSender};
use crate::error::OrgError;
use crate::integration::{IntegrationCredentials, IntegrationProvider, IntegrationTestResult, StubIntegrationProvider};
use crate::models::{
    AdminUserDto, AgentAuditEntry, AuditLogRow, DEFAULT_TENANT_ID, DepartmentDto, EnterpriseTenantDto, IntegrationDto,
    InviteDto, InviteRow, MyTenantDto, OrgContextDto, ROLE_MEMBER, ROLE_ORG_ADMIN, ROLE_SYSTEM_ADMIN, ResetLocalResult,
    RuntimeNodeDto, RuntimeNodeRow, SYSTEM_DEFAULT_USER_ID, SmtpConfigDto, TenantRow, UserOrgRow, is_admin_role,
    is_enterprise_tenant_id, is_system_admin_role,
};

pub struct OrgService {
    pool: SqlitePool,
    user_repo: Arc<dyn IUserRepository>,
    data_dir: PathBuf,
    /// Encrypts the stored SMTP password (P2-4 onboarding), same key/helper as
    /// provider API keys and SSO client secrets elsewhere in the app.
    encryption_key: [u8; 32],
    /// Sends invite emails (P2-4 onboarding). Defaults to `StubEmailSender`
    /// (reports "not configured"); the app layer can swap in a real sender via
    /// `with_email_sender` once SMTP is actually wired.
    email_sender: Arc<dyn EmailSender>,
    /// Tests integration connectors (P2-1 reserved framework). Defaults to
    /// `StubIntegrationProvider` (reports "not configured"); the app layer can
    /// swap in a real provider via `with_integration_provider` once a connector
    /// client is actually wired.
    integration_provider: Arc<dyn IntegrationProvider>,
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
    pub fn new(
        pool: SqlitePool,
        user_repo: Arc<dyn IUserRepository>,
        data_dir: PathBuf,
        encryption_key: [u8; 32],
    ) -> Self {
        Self {
            pool,
            user_repo,
            data_dir,
            encryption_key,
            email_sender: Arc::new(StubEmailSender),
            integration_provider: Arc::new(StubIntegrationProvider),
        }
    }

    /// Swap in a real `EmailSender` once SMTP is actually configured/wired at
    /// the app layer. Chainable at construction time.
    pub fn with_email_sender(mut self, sender: Arc<dyn EmailSender>) -> Self {
        self.email_sender = sender;
        self
    }

    /// Swap in a real `IntegrationProvider` once a connector client is wired at
    /// the app layer (P2-1). Chainable at construction time.
    pub fn with_integration_provider(mut self, provider: Arc<dyn IntegrationProvider>) -> Self {
        self.integration_provider = provider;
        self
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // --- membership / roles / active tenant ---

    /// The project group a user is currently acting in (Phase 2
    /// multi-membership). Resolution order: the explicit `one_active_tenant`
    /// pointer *if the user is still a member of it*; else the user's
    /// most-recently-joined membership; else the personal-edition default.
    ///
    /// Read-only — never repairs the pointer (join/switch/leave own that), so
    /// the personal / standalone edition (no membership rows, empty
    /// `one_active_tenant`) always resolves to `DEFAULT_TENANT_ID` with zero
    /// writes, exactly as the single-membership model did. This is the single
    /// choke point every `tenant_of`/`effective_role` caller flows through, so
    /// the RBAC extractors and the team-resource TenantResolver keep working
    /// unchanged — they just now see the *active* group.
    pub async fn active_tenant_id(&self, user_id: &str) -> Result<String, OrgError> {
        // Preferred: the explicit active-tenant pointer, but only when it still
        // points at a group the user actually belongs to (the JOIN drops a
        // pointer left dangling by a `leave`).
        let active: Option<String> = sqlx::query_scalar(
            "SELECT at.tenant_id FROM one_active_tenant at \
             JOIN one_user_org uo ON uo.user_id = at.user_id AND uo.tenant_id = at.tenant_id \
             WHERE at.user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(tenant_id) = active {
            return Ok(tenant_id);
        }
        // Fallback: any membership, most-recently-joined first.
        let any: Option<String> = sqlx::query_scalar(
            "SELECT tenant_id FROM one_user_org WHERE user_id = ? ORDER BY created_at DESC, tenant_id ASC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(any.unwrap_or_else(|| DEFAULT_TENANT_ID.to_string()))
    }

    /// The user's membership row in a specific tenant, if any.
    async fn membership_row(&self, user_id: &str, tenant_id: &str) -> Result<Option<UserOrgRow>, OrgError> {
        let row = sqlx::query_as::<_, UserOrgRow>("SELECT * FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
            .bind(user_id)
            .bind(tenant_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// The user's membership row in their *active* tenant. Kept as the
    /// single-row accessor the rest of the service (and `effective_role`)
    /// reads through, so switching the active tenant transparently switches
    /// which row is "the" membership.
    pub async fn membership(&self, user_id: &str) -> Result<Option<UserOrgRow>, OrgError> {
        let tenant_id = self.active_tenant_id(user_id).await?;
        self.membership_row(user_id, &tenant_id).await
    }

    /// Effective role in the *active* tenant: explicit `one_user_org` row
    /// wins; the upstream built-in operator user is system_admin by default
    /// (desktop-operator semantics); everyone else is a plain member.
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
        self.active_tenant_id(user_id).await
    }

    /// All project groups a user belongs to, for the "my project groups"
    /// switcher — each with the user's role there, the group's member count,
    /// and whether it's the currently-active group.
    pub async fn list_memberships(&self, user_id: &str) -> Result<Vec<MyTenantDto>, OrgError> {
        let active = self.active_tenant_id(user_id).await?;
        let rows = sqlx::query_as::<_, (String, String, String, i64)>(
            "SELECT t.id, t.name, uo.role, \
                    (SELECT COUNT(*) FROM one_user_org m WHERE m.tenant_id = t.id) AS member_count \
             FROM one_user_org uo JOIN one_tenants t ON t.id = uo.tenant_id \
             WHERE uo.user_id = ? ORDER BY uo.created_at ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(tenant_id, name, role, member_count)| MyTenantDto {
                is_active: tenant_id == active,
                tenant_id,
                name,
                role,
                member_count,
            })
            .collect())
    }

    /// Switch which project group a user is acting in. Validates membership
    /// (you can only activate a group you belong to) and upserts the pointer.
    /// No token rotation: the JWT carries only the user id, and every request
    /// re-resolves tenant/role server-side, so a switch takes effect on the
    /// next request without re-authentication.
    pub async fn set_active_tenant(&self, user_id: &str, tenant_id: &str) -> Result<(), OrgError> {
        let is_member: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
                .bind(user_id)
                .bind(tenant_id)
                .fetch_one(&self.pool)
                .await?;
        if !is_member {
            return Err(OrgError::NotInEnterprise);
        }
        let now = now_ms() as i64;
        sqlx::query(
            "INSERT INTO one_active_tenant (user_id, tenant_id, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Bulk-generate `count` invite codes at once (P2-4 onboarding). Each code
    /// is unique (delegates to `create_invite`). `count` is clamped to [1, 100].
    pub async fn create_invites_bulk(
        &self,
        tenant_id: &str,
        created_by: &str,
        count: usize,
        max_uses: Option<i64>,
        expires_in_days: Option<i64>,
    ) -> Result<Vec<(InviteDto, String)>, OrgError> {
        let count = count.clamp(1, 100);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(
                self.create_invite(tenant_id, created_by, max_uses, expires_in_days)
                    .await?,
            );
        }
        Ok(out)
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
        let code = normalize_invite_code(code_raw);
        let invite = self
            .find_active_invite_by_code(&code)
            .await?
            .ok_or(OrgError::InvalidCode)?;

        // Phase 2 multi-membership: a user may belong to several project
        // groups, so joining is only rejected when they are already in *this*
        // group (idempotency guard that also avoids burning an invite use).
        // The old "already in any enterprise" gate is gone.
        let already_member: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
                .bind(user_id)
                .bind(&invite.tenant_id)
                .fetch_one(&self.pool)
                .await?;
        if already_member {
            return Err(OrgError::AlreadyInEnterprise);
        }

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
             ON CONFLICT(user_id, tenant_id) DO UPDATE SET updated_at = excluded.updated_at, \
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
        // The group just joined becomes the active one.
        sqlx::query(
            "INSERT INTO one_active_tenant (user_id, tenant_id, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&invite.tenant_id)
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

    /// Set the email domains that may auto-join `tenant_id` without an invite
    /// code (P2-4 onboarding). Empty list disables auto-join (the default).
    pub async fn set_tenant_allowed_domains(&self, tenant_id: &str, domains: &[String]) -> Result<(), OrgError> {
        let cleaned: Vec<String> = domains
            .iter()
            .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        let json = if cleaned.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&cleaned).unwrap_or_else(|_| "[]".to_owned()))
        };
        let updated = sqlx::query("UPDATE one_tenants SET allowed_email_domains = ? WHERE id = ?")
            .bind(json)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(OrgError::TenantNotFound);
        }
        Ok(())
    }

    pub async fn tenant_allowed_domains(&self, tenant_id: &str) -> Result<Vec<String>, OrgError> {
        let json: Option<String> = sqlx::query_scalar("SELECT allowed_email_domains FROM one_tenants WHERE id = ?")
            .bind(tenant_id)
            .fetch_optional(&self.pool)
            .await?
            .flatten();
        Ok(json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default())
    }

    /// Auto-join a user to any tenant whose `allowed_email_domains` matches the
    /// email's domain (P2-4 onboarding) — no invite code needed. Best-effort:
    /// designed to be called from the SSO login hook and must never fail the
    /// login; callers should swallow the `Result` err like `EnterpriseSync`
    /// does. Returns the joined tenant id, or `None` when no tenant matches or
    /// the user is already a member there (idempotent).
    pub async fn auto_join_by_email(&self, user_id: &str, email: &str) -> Result<Option<String>, OrgError> {
        let Some(domain) = email.rsplit('@').next().map(str::trim).filter(|d| !d.is_empty()) else {
            return Ok(None);
        };
        let domain = domain.to_ascii_lowercase();

        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, allowed_email_domains FROM one_tenants WHERE allowed_email_domains IS NOT NULL")
                .fetch_all(&self.pool)
                .await?;
        let target_tenant = rows.into_iter().find_map(|(tenant_id, domains_json)| {
            let domains: Vec<String> = domains_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            domains
                .iter()
                .any(|d| d.eq_ignore_ascii_case(&domain))
                .then_some(tenant_id)
        });
        let Some(tenant_id) = target_tenant else {
            return Ok(None);
        };

        let already_member: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
                .bind(user_id)
                .bind(&tenant_id)
                .fetch_one(&self.pool)
                .await?;
        if already_member {
            return Ok(None);
        }

        let now = now_ms() as i64;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO one_user_org (user_id, tenant_id, role, created_at, updated_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, tenant_id) DO UPDATE SET updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&tenant_id)
        .bind(ROLE_MEMBER)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO one_active_tenant (user_id, tenant_id, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&tenant_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.invalidate_user_tokens(user_id).await?;
        let username = self.lookup_username(user_id).await;
        self.audit(
            &tenant_id,
            Some(user_id),
            username.as_deref(),
            "org.auto_join_domain",
            None,
        )
        .await;
        Ok(Some(tenant_id))
    }

    // --- SMTP config + invite email (P2-4 onboarding) ---
    //
    // No SMTP client library is wired in: this is the "底层适配" the operator
    // asked for — a config store + a pluggable send seam, same shape as
    // `one_billing::BillingProvider` for payment. `StubEmailSender` (default)
    // reports "not configured"; a real implementation (e.g. wrapping `lettre`)
    // can be dropped in at the app layer without touching this crate.

    pub async fn get_smtp_config(&self) -> Result<SmtpConfigDto, OrgError> {
        type SmtpConfigRow = (
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
            i64,
        );
        let row: Option<SmtpConfigRow> = sqlx::query_as(
            "SELECT host, port, username, password_encrypted, from_address, enabled, updated_at \
             FROM one_smtp_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((host, port, username, password_encrypted, from_address, enabled, updated_at)) => SmtpConfigDto {
                host,
                port,
                username,
                has_password: password_encrypted.is_some(),
                from_address,
                enabled,
                updated_at: Some(updated_at),
            },
            None => SmtpConfigDto {
                host: None,
                port: None,
                username: None,
                has_password: false,
                from_address: None,
                enabled: false,
                updated_at: None,
            },
        })
    }

    /// `password` absent = keep the stored one (if any); present = replace
    /// (encrypted at rest, same helper as provider API keys).
    #[allow(clippy::too_many_arguments)]
    pub async fn set_smtp_config(
        &self,
        host: &str,
        port: i64,
        username: Option<&str>,
        password: Option<&str>,
        from_address: &str,
        enabled: bool,
    ) -> Result<SmtpConfigDto, OrgError> {
        let existing_password: Option<String> =
            sqlx::query_scalar("SELECT password_encrypted FROM one_smtp_config WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let password_encrypted = match password {
            Some(p) if !p.is_empty() => {
                Some(encrypt_string(p, &self.encryption_key).map_err(|e| OrgError::Internal(e.to_string()))?)
            }
            _ => existing_password,
        };
        sqlx::query(
            "INSERT INTO one_smtp_config (id, host, port, username, password_encrypted, from_address, enabled, updated_at) \
             VALUES (1, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET host = excluded.host, port = excluded.port, username = excluded.username, \
                 password_encrypted = excluded.password_encrypted, from_address = excluded.from_address, \
                 enabled = excluded.enabled, updated_at = excluded.updated_at",
        )
        .bind(host)
        .bind(port)
        .bind(username)
        .bind(&password_encrypted)
        .bind(from_address)
        .bind(enabled)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        self.get_smtp_config().await
    }

    /// The decrypted SMTP password, for a real `EmailSender` implementation to
    /// consume. `None` when unset or decryption fails (never panics).
    pub async fn smtp_password(&self) -> Result<Option<String>, OrgError> {
        let encrypted: Option<String> =
            sqlx::query_scalar("SELECT password_encrypted FROM one_smtp_config WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(encrypted.and_then(|e| decrypt_string(&e, &self.encryption_key).ok()))
    }

    /// Send an invite by email through whatever `EmailSender` is wired
    /// (`StubEmailSender` by default — reports "not configured"). Looks up the
    /// invite by id (scoped to `tenant_id`) and formats its code for display.
    pub async fn send_invite_email(
        &self,
        tenant_id: &str,
        invite_id: &str,
        to: &str,
    ) -> Result<SendEmailResult, OrgError> {
        let row = sqlx::query_as::<_, InviteRow>("SELECT * FROM one_tenant_invites WHERE id = ? AND tenant_id = ?")
            .bind(invite_id)
            .bind(tenant_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(OrgError::InvalidCode)?;
        let tenant = self.get_tenant(tenant_id).await?.ok_or(OrgError::TenantNotFound)?;
        let display_code = format_invite_code_for_display(&row.code);
        Ok(self.email_sender.send_invite(to, &display_code, &tenant.name).await)
    }

    // --- Integration connectors (P2-1 reserved framework) ---
    //
    // Per-(tenant, provider) connector config. Storing a row does NOT sync
    // anything — the secret is encrypted at rest and a real
    // `IntegrationProvider` (wired at the app layer) does the actual work. Until
    // then a "test" reports "not configured" via `StubIntegrationProvider`.

    /// Parse the stored non-secret `config_json` into a JSON object, defaulting
    /// to `{}` when null/blank/invalid (never fails the read on bad data).
    fn parse_config(config_json: Option<String>) -> serde_json::Value {
        config_json
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    }

    /// All configured connectors for a project group (redacted — no secrets).
    pub async fn list_integrations(&self, tenant_id: &str) -> Result<Vec<IntegrationDto>, OrgError> {
        type IntegrationRow = (String, Option<String>, Option<String>, Option<String>, bool, i64);
        let rows: Vec<IntegrationRow> = sqlx::query_as(
            "SELECT provider, base_url, config_json, secret_encrypted, enabled, updated_at \
             FROM one_integrations WHERE tenant_id = ? ORDER BY provider",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(provider, base_url, config_json, secret_encrypted, enabled, updated_at)| IntegrationDto {
                    provider,
                    base_url,
                    config: Self::parse_config(config_json),
                    has_secret: secret_encrypted.is_some(),
                    enabled,
                    updated_at: Some(updated_at),
                },
            )
            .collect())
    }

    /// One connector's redacted config, or an empty/disabled default when this
    /// provider has never been configured for the tenant.
    pub async fn get_integration(&self, tenant_id: &str, provider: &str) -> Result<IntegrationDto, OrgError> {
        type IntegrationRow = (Option<String>, Option<String>, Option<String>, bool, i64);
        let row: Option<IntegrationRow> = sqlx::query_as(
            "SELECT base_url, config_json, secret_encrypted, enabled, updated_at \
             FROM one_integrations WHERE tenant_id = ? AND provider = ?",
        )
        .bind(tenant_id)
        .bind(provider)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((base_url, config_json, secret_encrypted, enabled, updated_at)) => IntegrationDto {
                provider: provider.to_owned(),
                base_url,
                config: Self::parse_config(config_json),
                has_secret: secret_encrypted.is_some(),
                enabled,
                updated_at: Some(updated_at),
            },
            None => IntegrationDto {
                provider: provider.to_owned(),
                base_url: None,
                config: serde_json::json!({}),
                has_secret: false,
                enabled: false,
                updated_at: None,
            },
        })
    }

    /// Upsert a connector. `secret` absent/empty = keep the stored one (if any);
    /// present = replace (encrypted at rest, same helper as the SMTP password).
    /// `config` is a non-secret JSON object stored verbatim.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_integration(
        &self,
        tenant_id: &str,
        provider: &str,
        base_url: Option<&str>,
        config: &serde_json::Value,
        secret: Option<&str>,
        enabled: bool,
    ) -> Result<IntegrationDto, OrgError> {
        let existing_secret: Option<String> =
            sqlx::query_scalar("SELECT secret_encrypted FROM one_integrations WHERE tenant_id = ? AND provider = ?")
                .bind(tenant_id)
                .bind(provider)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let secret_encrypted = match secret {
            Some(s) if !s.is_empty() => {
                Some(encrypt_string(s, &self.encryption_key).map_err(|e| OrgError::Internal(e.to_string()))?)
            }
            _ => existing_secret,
        };
        let config_json = serde_json::to_string(config).map_err(|e| OrgError::Internal(e.to_string()))?;
        sqlx::query(
            "INSERT INTO one_integrations (tenant_id, provider, base_url, config_json, secret_encrypted, enabled, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id, provider) DO UPDATE SET base_url = excluded.base_url, config_json = excluded.config_json, \
                 secret_encrypted = excluded.secret_encrypted, enabled = excluded.enabled, updated_at = excluded.updated_at",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(base_url)
        .bind(&config_json)
        .bind(&secret_encrypted)
        .bind(enabled)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        self.get_integration(tenant_id, provider).await
    }

    /// The decrypted connector secret, for a real `IntegrationProvider` to
    /// consume. `None` when unset or decryption fails (never panics).
    pub async fn integration_secret(&self, tenant_id: &str, provider: &str) -> Result<Option<String>, OrgError> {
        let encrypted: Option<String> =
            sqlx::query_scalar("SELECT secret_encrypted FROM one_integrations WHERE tenant_id = ? AND provider = ?")
                .bind(tenant_id)
                .bind(provider)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(encrypted.and_then(|e| decrypt_string(&e, &self.encryption_key).ok()))
    }

    /// Probe a saved connector through whatever `IntegrationProvider` is wired
    /// (`StubIntegrationProvider` by default — reports "not configured").
    pub async fn test_integration(&self, tenant_id: &str, provider: &str) -> Result<IntegrationTestResult, OrgError> {
        let dto = self.get_integration(tenant_id, provider).await?;
        let secret = self.integration_secret(tenant_id, provider).await?;
        Ok(self
            .integration_provider
            .test_connection(IntegrationCredentials {
                provider,
                base_url: dto.base_url.as_deref(),
                config: &dto.config,
                secret: secret.as_deref(),
            })
            .await)
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
        // leave the instance with no system_admin. A project group carries no
        // SSO-company binding — the SSO company is a separate dimension
        // (one-enterprise); this is purely an invite-code tenant.
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO one_tenants (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(&tenant_id)
            .bind(name)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO one_user_org \
             (user_id, tenant_id, role, display_name, org_unit_path, job_title, org_profile_source, \
              org_profile_synced_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, tenant_id) DO UPDATE SET \
                 role = excluded.role, updated_at = excluded.updated_at, \
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
        sqlx::query(
            "INSERT INTO one_active_tenant (user_id, tenant_id, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&tenant_id)
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

    /// Create a project group OWNED by a company (Direction B). Unlike
    /// `create_tenant` (the standalone invite-code path, left byte-for-byte
    /// intact), this:
    /// - does NOT enforce the global "one server = one enterprise" D3 limit — a
    ///   company legitimately owns many project groups;
    /// - does NOT auto-join the creator (the group starts empty);
    /// - optionally seeds `initial_admin_user_id` as the group's org_admin —
    ///   Phase 2 multi-membership allows this even when that user already
    ///   belongs to other groups (the composite PK `(user_id, tenant_id)` makes
    ///   a second membership row legitimate);
    /// - auto-generates one invite so the empty group is immediately joinable.
    ///
    /// Authorization (system_admin OR company-admin of `enterprise_id`) is
    /// enforced by the route handler before this is called.
    pub async fn create_tenant_for_enterprise(
        &self,
        enterprise_id: &str,
        name_raw: &str,
        created_by: &str,
        initial_admin_user_id: Option<&str>,
    ) -> Result<(String, String, String), OrgError> {
        let name = name_raw.trim();
        if name.is_empty() {
            return Err(OrgError::NameRequired);
        }

        let tenant_id = short_id("tenant");
        let now = now_ms() as i64;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO one_tenants (id, name, enterprise_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&tenant_id)
            .bind(name)
            .bind(enterprise_id)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        if let Some(admin) = initial_admin_user_id {
            sqlx::query(
                "INSERT INTO one_user_org (user_id, tenant_id, role, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(admin)
            .bind(&tenant_id)
            .bind(ROLE_ORG_ADMIN)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let (_invite, code) = self.create_invite(&tenant_id, created_by, None, None).await?;
        self.audit(
            &tenant_id,
            Some(created_by),
            None,
            "org.create_for_enterprise",
            Some(name),
        )
        .await;
        Ok((tenant_id, name.to_string(), code))
    }

    /// Every project group on this server (id + name), for admin pickers such
    /// as the devops resource scope selector (P0-4). Ordered by creation.
    pub async fn list_all_tenants(&self) -> Result<Vec<crate::models::TenantSummaryDto>, OrgError> {
        let rows = sqlx::query_as::<_, crate::models::TenantSummaryDto>(
            "SELECT id, name FROM one_tenants ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The project groups a company owns, with per-group member counts.
    pub async fn list_tenants_by_enterprise(&self, enterprise_id: &str) -> Result<Vec<EnterpriseTenantDto>, OrgError> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT id, name, created_at FROM one_tenants WHERE enterprise_id = ? ORDER BY created_at ASC",
        )
        .bind(enterprise_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (tenant_id, name, created_at) in rows {
            let member_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_user_org WHERE tenant_id = ?")
                .bind(&tenant_id)
                .fetch_one(&self.pool)
                .await?;
            out.push(EnterpriseTenantDto {
                tenant_id,
                name,
                member_count,
                created_at,
            });
        }
        Ok(out)
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
        sqlx::query("DELETE FROM one_active_tenant").execute(&mut *tx).await?;
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

    /// Leave a project group. `tenant_id` selects which group to leave;
    /// `None` leaves the user's currently-active group. Scoped delete so a
    /// user who belongs to several groups only leaves the one named.
    pub async fn leave(&self, user_id: &str, tenant_id: Option<&str>, exit_code: &str) -> Result<(), OrgError> {
        let target = match tenant_id {
            Some(t) => t.to_string(),
            None => self.active_tenant_id(user_id).await?,
        };
        let membership = self.membership_row(user_id, &target).await?;
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

        sqlx::query("DELETE FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
            .bind(user_id)
            .bind(&membership.tenant_id)
            .execute(&self.pool)
            .await?;
        // If the group just left was the active one, repoint to another
        // membership (or clear the pointer so resolution falls back to default).
        self.reselect_active_after_leave(user_id, &membership.tenant_id).await?;
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

    // --- backup / restore (P1-1) ---

    /// Export the deployment's enterprise configuration (see `backup` module).
    pub async fn export_backup(
        &self,
        tenant_id: &str,
        actor_user_id: &str,
    ) -> Result<crate::backup::BackupBundle, OrgError> {
        let bundle = crate::backup::export_bundle(&self.pool, tenant_id, now_ms() as i64).await?;
        // Exports are worth an audit trail: the file leaves the deployment, and
        // "who took a copy of the org config, when" is a question a security
        // review will ask.
        let actor_username = self.lookup_username(actor_user_id).await;
        self.audit(
            tenant_id,
            Some(actor_user_id),
            actor_username.as_deref(),
            "org.backup.export",
            Some(&format!("{} tables", bundle.tables.len())),
        )
        .await;
        Ok(bundle)
    }

    /// Restore an exported bundle. Idempotent; see the `backup` module.
    pub async fn import_backup(
        &self,
        tenant_id: &str,
        actor_user_id: &str,
        bundle: &crate::backup::BackupBundle,
    ) -> Result<crate::backup::ImportReport, OrgError> {
        let report = crate::backup::import_bundle(&self.pool, bundle).await?;
        let actor_username = self.lookup_username(actor_user_id).await;
        self.audit(
            tenant_id,
            Some(actor_user_id),
            actor_username.as_deref(),
            "org.backup.import",
            Some(&format!(
                "{} tables / {} rows",
                report.tables_applied, report.rows_applied
            )),
        )
        .await;
        Ok(report)
    }

    /// Admin-initiated removal of another member from `tenant_id` (P0-2).
    ///
    /// This is `leave()` performed *by an administrator on someone else*, and
    /// it deliberately mirrors that method's cleanup so a removed member is
    /// left in exactly the same state as one who quit: the membership row is
    /// deleted, a dangling active-tenant pointer is repointed, and the target's
    /// JWT secret is rotated so **existing sessions stop working immediately**
    /// rather than lingering until token expiry. Without that rotation a
    /// just-offboarded employee would keep a working client — the whole point
    /// of having this endpoint.
    ///
    /// Differs from `leave()` in that no exit password is required (the admin
    /// is the authority here, not the member) and three guards apply instead.
    pub async fn remove_member(
        &self,
        tenant_id: &str,
        actor_user_id: &str,
        target_user_id: &str,
    ) -> Result<(), OrgError> {
        // Removing yourself would let an admin bypass the exit-password gate
        // that `leave()` enforces. Send them through the front door.
        if actor_user_id == target_user_id {
            return Err(OrgError::BadRequest(
                "cannot remove yourself; use leave to exit the project group".into(),
            ));
        }

        let membership = self
            .membership_row(target_user_id, tenant_id)
            .await?
            .ok_or_else(|| OrgError::BadRequest(format!("user {target_user_id} not in tenant {tenant_id}")))?;

        // A system_admin outranks an org_admin; only a peer may remove one.
        // Otherwise any org_admin could unseat the machine owner.
        if is_system_admin_role(&membership.role) {
            let actor_role = self
                .membership_row(actor_user_id, tenant_id)
                .await?
                .map(|m| m.role)
                .unwrap_or_default();
            if !is_system_admin_role(&actor_role) {
                return Err(OrgError::Forbidden(
                    "only system_admin can remove a system_admin".into(),
                ));
            }
        }

        if is_admin_role(&membership.role) {
            self.ensure_not_last_admin(tenant_id, target_user_id).await?;
        }

        sqlx::query("DELETE FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
            .bind(target_user_id)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        self.reselect_active_after_leave(target_user_id, tenant_id).await?;
        self.invalidate_user_tokens(target_user_id).await?;

        // Attributed to the ACTOR, with the target in `resource` — same
        // rationale as `set_user_role`: attributing it to the target would
        // make every removal read as a voluntary exit and hide who did it.
        let actor_username = self.lookup_username(actor_user_id).await;
        self.audit(
            tenant_id,
            Some(actor_user_id),
            actor_username.as_deref(),
            "org.member.remove",
            Some(target_user_id),
        )
        .await;
        Ok(())
    }

    /// After leaving `left_tenant`, if the active pointer named it, move the
    /// pointer to any remaining membership (most-recently-joined) or delete it
    /// (so resolution falls back to the personal-edition default). Keeps
    /// `one_active_tenant` from dangling.
    async fn reselect_active_after_leave(&self, user_id: &str, left_tenant: &str) -> Result<(), OrgError> {
        let active: Option<String> = sqlx::query_scalar("SELECT tenant_id FROM one_active_tenant WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        if active.as_deref() != Some(left_tenant) {
            return Ok(());
        }
        let next: Option<String> = sqlx::query_scalar(
            "SELECT tenant_id FROM one_user_org WHERE user_id = ? ORDER BY created_at DESC, tenant_id ASC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        match next {
            Some(t) => {
                sqlx::query("UPDATE one_active_tenant SET tenant_id = ?, updated_at = ? WHERE user_id = ?")
                    .bind(&t)
                    .bind(now_ms() as i64)
                    .bind(user_id)
                    .execute(&self.pool)
                    .await?;
            }
            None => {
                sqlx::query("DELETE FROM one_active_tenant WHERE user_id = ?")
                    .bind(user_id)
                    .execute(&self.pool)
                    .await?;
            }
        }
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

    /// Whether the caller's company plan includes `feature`. company
    /// (`one_enterprise_members`) → tier (`one_enterprise_license`) → the
    /// `aionui-common` matrix. No enterprise / billing not installed → allowed
    /// (personal-edition red line). Tolerant of absent tables.
    pub async fn enterprise_feature_allowed(&self, user_id: &str, feature: Feature) -> Result<bool, OrgError> {
        let enterprise_id: Option<String> =
            sqlx::query_scalar("SELECT enterprise_id FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        let Some(enterprise_id) = enterprise_id else {
            return Ok(true);
        };
        let tier: Option<String> =
            sqlx::query_scalar("SELECT tier FROM one_enterprise_license WHERE enterprise_id = ?")
                .bind(&enterprise_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        let tier = tier.map(|t| Tier::parse(&t)).unwrap_or(Tier::Free);
        Ok(tier_allows(tier, feature))
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

    /// Agent-run audit (P1-1): every tool the agents invoked — which file /
    /// command / tool — reconstructed from the persisted `messages` tool-call
    /// rows joined to the owning conversation. Server-wide (one instance = one
    /// company); admin-only + AuditLog-gated at the route. Optional filters by
    /// user, tool name, and time; newest first.
    pub async fn list_agent_audit(
        &self,
        user_filter: Option<&str>,
        tool_filter: Option<&str>,
        since_ms: Option<i64>,
        limit: i64,
    ) -> Result<Vec<AgentAuditEntry>, OrgError> {
        let limit = limit.clamp(1, 2000);
        // Tool name / target vary by backend (aionrs vs ACP) — extract
        // best-effort from a few well-known JSON shapes.
        let name_expr = "COALESCE(json_extract(m.content,'$.name'), json_extract(m.content,'$.toolName'), \
                         json_extract(m.content,'$.tool'), '')";
        let detail_expr = "COALESCE(json_extract(m.content,'$.args.command'), json_extract(m.content,'$.args.path'), \
                          json_extract(m.content,'$.args.file_path'), json_extract(m.content,'$.args.pattern'), \
                          json_extract(m.content,'$.args.url'), json_extract(m.content,'$.input.command'), \
                          json_extract(m.content,'$.input.path'), json_extract(m.content,'$.description'))";
        let mut sql = format!(
            "SELECT m.id AS id, m.conversation_id AS conversation_id, c.user_id AS user_id, \
                    {name_expr} AS tool_name, {detail_expr} AS detail, m.status AS status, m.created_at AS created_at \
             FROM messages m JOIN conversations c ON c.id = m.conversation_id \
             WHERE m.type IN ('tool_call', 'acp_tool_call')"
        );
        if user_filter.is_some() {
            sql.push_str(" AND c.user_id = ?");
        }
        if tool_filter.is_some() {
            sql.push_str(&format!(" AND {name_expr} = ?"));
        }
        if since_ms.is_some() {
            sql.push_str(" AND m.created_at >= ?");
        }
        sql.push_str(" ORDER BY m.created_at DESC LIMIT ?");

        let mut q = sqlx::query_as::<_, AgentAuditEntry>(&sql);
        if let Some(u) = user_filter {
            q = q.bind(u);
        }
        if let Some(tool) = tool_filter {
            q = q.bind(tool);
        }
        if let Some(s) = since_ms {
            q = q.bind(s);
        }
        q = q.bind(limit);
        Ok(q.fetch_all(&self.pool).await?)
    }

    // --- admin: users ---

    pub async fn list_users(&self, tenant_id: &str) -> Result<Vec<AdminUserDto>, OrgError> {
        let rows = sqlx::query_as::<_, AdminUserDto>(
            "SELECT uo.user_id, u.username, uo.tenant_id, uo.role, uo.display_name, uo.org_unit_path, \
                    uo.job_title, uo.department_id, u.last_login, uo.created_at \
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

    // --- departments / organizational hierarchy (P2-3) ---

    /// Create a department (top-level when `parent_id` is `None`). The parent,
    /// if given, must already exist in the same tenant.
    pub async fn create_department(
        &self,
        tenant_id: &str,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<DepartmentDto, OrgError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(OrgError::BadRequest("department name is required".into()));
        }
        if let Some(pid) = parent_id {
            let exists: bool =
                sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_departments WHERE id = ? AND tenant_id = ?")
                    .bind(pid)
                    .bind(tenant_id)
                    .fetch_one(&self.pool)
                    .await?;
            if !exists {
                return Err(OrgError::DepartmentNotFound);
            }
        }
        let id = short_id("dept");
        let now = now_ms() as i64;
        sqlx::query(
            "INSERT INTO one_departments (id, tenant_id, parent_id, name, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(tenant_id)
        .bind(parent_id)
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query_as::<_, DepartmentDto>("SELECT * FROM one_departments WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    /// Every department in the tenant (flat; the frontend assembles the tree
    /// from `parent_id`).
    pub async fn list_departments(&self, tenant_id: &str) -> Result<Vec<DepartmentDto>, OrgError> {
        let rows = sqlx::query_as::<_, DepartmentDto>(
            "SELECT * FROM one_departments WHERE tenant_id = ? ORDER BY created_at ASC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn rename_department(
        &self,
        tenant_id: &str,
        department_id: &str,
        name: &str,
    ) -> Result<DepartmentDto, OrgError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(OrgError::BadRequest("department name is required".into()));
        }
        let updated = sqlx::query("UPDATE one_departments SET name = ?, updated_at = ? WHERE id = ? AND tenant_id = ?")
            .bind(name)
            .bind(now_ms() as i64)
            .bind(department_id)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(OrgError::DepartmentNotFound);
        }
        sqlx::query_as::<_, DepartmentDto>("SELECT * FROM one_departments WHERE id = ?")
            .bind(department_id)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    /// Delete a department. Rejected (not cascaded) when it still has child
    /// departments or assigned members — the caller must reassign those first,
    /// the same "explicit over surprising" rule as elsewhere in this crate.
    pub async fn delete_department(&self, tenant_id: &str, department_id: &str) -> Result<(), OrgError> {
        let has_children: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_departments WHERE parent_id = ?")
            .bind(department_id)
            .fetch_one(&self.pool)
            .await?;
        if has_children {
            return Err(OrgError::BadRequest(
                "department has sub-departments; move or delete them first".into(),
            ));
        }
        let has_members: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_user_org WHERE department_id = ? AND tenant_id = ?")
                .bind(department_id)
                .bind(tenant_id)
                .fetch_one(&self.pool)
                .await?;
        if has_members {
            return Err(OrgError::BadRequest(
                "department still has members assigned; reassign them first".into(),
            ));
        }
        let deleted = sqlx::query("DELETE FROM one_departments WHERE id = ? AND tenant_id = ?")
            .bind(department_id)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(OrgError::DepartmentNotFound);
        }
        Ok(())
    }

    /// Assign (or clear, `department_id = None`) a member's department.
    pub async fn assign_member_department(
        &self,
        tenant_id: &str,
        user_id: &str,
        department_id: Option<&str>,
    ) -> Result<(), OrgError> {
        if let Some(did) = department_id {
            let exists: bool =
                sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_departments WHERE id = ? AND tenant_id = ?")
                    .bind(did)
                    .bind(tenant_id)
                    .fetch_one(&self.pool)
                    .await?;
            if !exists {
                return Err(OrgError::DepartmentNotFound);
            }
        }
        let updated = sqlx::query(
            "UPDATE one_user_org SET department_id = ?, updated_at = ? WHERE user_id = ? AND tenant_id = ?",
        )
        .bind(department_id)
        .bind(now_ms() as i64)
        .bind(user_id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(OrgError::Forbidden("user is not a member of this tenant".into()));
        }
        Ok(())
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
        let service = Arc::new(OrgService::new(
            db.pool().clone(),
            user_repo.clone(),
            data_dir,
            [7u8; 32],
        ));
        (db, service, user_repo)
    }

    /// Token rotation goes through the upstream user repo, so test users must
    /// exist in the upstream `users` table (in production the auth middleware
    /// guarantees this).
    async fn create_user(user_repo: &Arc<dyn IUserRepository>, username: &str) -> String {
        user_repo.create_user(username, "x").await.unwrap().id
    }

    #[tokio::test]
    async fn bulk_invite_generates_unique_codes_and_clamps() {
        let (_db, service, _user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let batch = service
            .create_invites_bulk(&tenant_id, SYSTEM_DEFAULT_USER_ID, 5, Some(1), Some(7))
            .await
            .unwrap();
        assert_eq!(batch.len(), 5);
        let displays: std::collections::HashSet<_> = batch.iter().map(|(_, d)| d.clone()).collect();
        assert_eq!(displays.len(), 5, "all codes unique");
        // Count is clamped to [1, 100].
        assert_eq!(
            service
                .create_invites_bulk(&tenant_id, SYSTEM_DEFAULT_USER_ID, 0, None, None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn domain_auto_join_matches_case_insensitively_and_is_idempotent() {
        let (_db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        service
            .set_tenant_allowed_domains(&tenant_id, &["Acme.com".to_owned()])
            .await
            .unwrap();
        assert_eq!(
            service.tenant_allowed_domains(&tenant_id).await.unwrap(),
            vec!["acme.com"]
        );

        let alice = create_user(&user_repo, "alice").await;
        // Domain match is case-insensitive on both sides.
        let joined = service.auto_join_by_email(&alice, "Alice@ACME.COM").await.unwrap();
        assert_eq!(joined, Some(tenant_id.clone()));

        // Idempotent: already a member → no-op, not an error.
        assert_eq!(
            service.auto_join_by_email(&alice, "alice@acme.com").await.unwrap(),
            None
        );

        // Non-matching domain → no-op.
        let bob = create_user(&user_repo, "bob").await;
        assert_eq!(service.auto_join_by_email(&bob, "bob@other.com").await.unwrap(), None);

        // Malformed / no '@' → no-op, never panics.
        assert_eq!(service.auto_join_by_email(&bob, "not-an-email").await.unwrap(), None);

        // Disabling (empty list) stops future auto-joins.
        service.set_tenant_allowed_domains(&tenant_id, &[]).await.unwrap();
        assert!(service.tenant_allowed_domains(&tenant_id).await.unwrap().is_empty());
        let carol = create_user(&user_repo, "carol").await;
        assert_eq!(
            service.auto_join_by_email(&carol, "carol@acme.com").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn smtp_config_roundtrips_and_redacts_password() {
        let (_db, service, _user_repo) = setup().await;
        // Absent by default.
        let cfg = service.get_smtp_config().await.unwrap();
        assert!(!cfg.enabled);
        assert!(!cfg.has_password);

        let saved = service
            .set_smtp_config(
                "smtp.acme.com",
                587,
                Some("bot"),
                Some("s3cret"),
                "noreply@acme.com",
                true,
            )
            .await
            .unwrap();
        assert_eq!(saved.host.as_deref(), Some("smtp.acme.com"));
        assert!(saved.has_password, "password presence is reported...");
        // ...but the DTO never carries the plaintext/ciphertext itself.
        let serialized = serde_json::to_string(&saved).unwrap();
        assert!(!serialized.contains("s3cret"));

        // Omitting password on a later save keeps the stored one.
        let updated = service
            .set_smtp_config("smtp.acme.com", 465, Some("bot"), None, "noreply@acme.com", true)
            .await
            .unwrap();
        assert!(updated.has_password);
        assert_eq!(service.smtp_password().await.unwrap().as_deref(), Some("s3cret"));
    }

    #[tokio::test]
    async fn invite_email_is_not_configured_by_default() {
        let (_db, service, _user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();
        let (invite, _display) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let result = service
            .send_invite_email(&tenant_id, &invite.id, "new-hire@acme.com")
            .await
            .unwrap();
        assert_eq!(result.status, "not_configured");
    }

    #[tokio::test]
    async fn department_tree_crud_and_member_assignment() {
        let (_db, service, user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        // Top-level + nested department.
        let eng = service
            .create_department(&tenant_id, "Engineering", None)
            .await
            .unwrap();
        assert_eq!(eng.parent_id, None);
        let backend = service
            .create_department(&tenant_id, "Backend", Some(&eng.id))
            .await
            .unwrap();
        assert_eq!(backend.parent_id.as_deref(), Some(eng.id.as_str()));

        // Unknown parent → DEPARTMENT_NOT_FOUND.
        assert_eq!(
            service
                .create_department(&tenant_id, "Ghost", Some("nope"))
                .await
                .unwrap_err()
                .code(),
            "DEPARTMENT_NOT_FOUND"
        );
        // Empty name rejected.
        assert_eq!(
            service
                .create_department(&tenant_id, "  ", None)
                .await
                .unwrap_err()
                .code(),
            "BAD_REQUEST"
        );

        let all = service.list_departments(&tenant_id).await.unwrap();
        assert_eq!(all.len(), 2);

        // Rename.
        let renamed = service
            .rename_department(&tenant_id, &backend.id, "Platform")
            .await
            .unwrap();
        assert_eq!(renamed.name, "Platform");

        // Deleting a department with a child is rejected.
        assert_eq!(
            service.delete_department(&tenant_id, &eng.id).await.unwrap_err().code(),
            "BAD_REQUEST"
        );

        // Assign a member, then deletion of their department is rejected.
        let alice = create_user(&user_repo, "alice").await;
        let (_, code) = service
            .create_invite(&tenant_id, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        service.join_with_invite(&alice, &code).await.unwrap();
        service
            .assign_member_department(&tenant_id, &alice, Some(&backend.id))
            .await
            .unwrap();
        assert_eq!(
            service
                .delete_department(&tenant_id, &backend.id)
                .await
                .unwrap_err()
                .code(),
            "BAD_REQUEST"
        );
        let users = service.list_users(&tenant_id).await.unwrap();
        let alice_row = users.iter().find(|u| u.user_id == alice).unwrap();
        assert_eq!(alice_row.department_id.as_deref(), Some(backend.id.as_str()));

        // Clear assignment, then deletion succeeds (leaf, no members).
        service
            .assign_member_department(&tenant_id, &alice, None)
            .await
            .unwrap();
        service.delete_department(&tenant_id, &backend.id).await.unwrap();
        // Now eng has no children → deletable too.
        service.delete_department(&tenant_id, &eng.id).await.unwrap();
        assert!(service.list_departments(&tenant_id).await.unwrap().is_empty());

        // Assigning to an unknown department → DEPARTMENT_NOT_FOUND.
        assert_eq!(
            service
                .assign_member_department(&tenant_id, &alice, Some("nope"))
                .await
                .unwrap_err()
                .code(),
            "DEPARTMENT_NOT_FOUND"
        );
        // Assigning a non-member → FORBIDDEN.
        let bob = create_user(&user_repo, "bob").await;
        assert_eq!(
            service
                .assign_member_department(&tenant_id, &bob, None)
                .await
                .unwrap_err()
                .code(),
            "FORBIDDEN"
        );
    }

    #[tokio::test]
    async fn integration_connector_roundtrips_redacts_and_stubs_test() {
        let (_db, service, _user_repo) = setup().await;
        let (tenant_id, _) = service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        // Absent by default.
        let empty = service.list_integrations(&tenant_id).await.unwrap();
        assert!(empty.is_empty());
        let default = service.get_integration(&tenant_id, "github").await.unwrap();
        assert!(!default.enabled && !default.has_secret);

        // Save a connector with a secret + non-secret config.
        let config = serde_json::json!({ "org": "acme" });
        let saved = service
            .set_integration(
                &tenant_id,
                "github",
                Some("https://api.github.com"),
                &config,
                Some("ghp_secret"),
                true,
            )
            .await
            .unwrap();
        assert_eq!(saved.base_url.as_deref(), Some("https://api.github.com"));
        assert!(saved.has_secret);
        assert_eq!(saved.config["org"], "acme");
        // The DTO never carries the plaintext/ciphertext secret.
        let serialized = serde_json::to_string(&saved).unwrap();
        assert!(!serialized.contains("ghp_secret"));

        // Omitting the secret on a later save keeps the stored one; other
        // fields update.
        let updated = service
            .set_integration(&tenant_id, "github", Some("https://ghe.acme.com"), &config, None, false)
            .await
            .unwrap();
        assert!(updated.has_secret);
        assert!(!updated.enabled);
        assert_eq!(updated.base_url.as_deref(), Some("https://ghe.acme.com"));
        assert_eq!(
            service
                .integration_secret(&tenant_id, "github")
                .await
                .unwrap()
                .as_deref(),
            Some("ghp_secret")
        );

        // A second provider is independent; list returns both.
        service
            .set_integration(&tenant_id, "jira", None, &serde_json::json!({}), Some("jira_tok"), true)
            .await
            .unwrap();
        let all = service.list_integrations(&tenant_id).await.unwrap();
        assert_eq!(all.len(), 2);

        // The default stub provider reports "not configured" for a test.
        let result = service.test_integration(&tenant_id, "github").await.unwrap();
        assert_eq!(result.status, "not_configured");
    }

    #[tokio::test]
    async fn agent_audit_reconstructs_tool_calls_from_messages() {
        let (db, service, user_repo) = setup().await;
        let uid = create_user(&user_repo, "alice").await;
        let pool = db.pool();
        sqlx::query("INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) VALUES ('c1', ?, 'chat', 'acp', 0, 0)")
            .bind(&uid)
            .execute(pool)
            .await
            .unwrap();
        // A Read (aionrs shape), a Bash (acp shape), and a non-tool message.
        sqlx::query(r#"INSERT INTO messages (id, conversation_id, type, content, created_at) VALUES ('m1', 'c1', 'tool_call', '{"name":"Read","args":{"path":"/tmp/a.txt"}}', 10)"#)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(r#"INSERT INTO messages (id, conversation_id, type, content, created_at) VALUES ('m2', 'c1', 'acp_tool_call', '{"name":"Bash","args":{"command":"ls -la"}}', 20)"#)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(r#"INSERT INTO messages (id, conversation_id, type, content, created_at) VALUES ('m3', 'c1', 'text', '{"text":"hi"}', 5)"#)
            .execute(pool)
            .await
            .unwrap();

        // All tool calls, newest first; non-tool message excluded.
        let all = service.list_agent_audit(None, None, None, 100).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].tool_name, "Bash");
        assert_eq!(all[0].detail.as_deref(), Some("ls -la"));
        assert_eq!(all[0].user_id.as_deref(), Some(uid.as_str()));
        assert_eq!(all[1].tool_name, "Read");
        assert_eq!(all[1].detail.as_deref(), Some("/tmp/a.txt"));

        // Filter by tool + by user.
        assert_eq!(
            service
                .list_agent_audit(None, Some("Read"), None, 100)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            service
                .list_agent_audit(Some("bob"), None, None, 100)
                .await
                .unwrap()
                .is_empty()
        );
        // Time filter drops the older Read (created_at 10 < 15).
        assert_eq!(
            service.list_agent_audit(None, None, Some(15), 100).await.unwrap().len(),
            1
        );
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

    /// Personal edition (no membership row) reports everything empty — the
    /// red line that this endpoint's personal-mode shape is unchanged.
    #[tokio::test]
    async fn context_in_personal_edition_is_empty() {
        let (_db, service, user_repo) = setup().await;
        let user = create_user(&user_repo, "solo").await;

        let ctx = service.context(&user).await.unwrap();
        assert_eq!(ctx.tenant_id, DEFAULT_TENANT_ID);
        assert!(!ctx.is_enterprise);
        assert_eq!(ctx.member_count, 0);
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
        service.leave(member, None, "").await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 1);

        // Re-join to exercise password-gated exit.
        service.preview_invite(&display).await.unwrap();
        service.join_with_invite(member, &display).await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 2);

        service.set_exit_password(&tenant_id, "s3cret").await.unwrap();
        assert!(service.exit_password_status(&tenant_id).await.unwrap());

        let err = service.leave(member, None, "wrong").await.unwrap_err();
        assert_eq!(err.code(), "WRONG_EXIT_CODE");

        service.leave(member, None, "s3cret").await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 1);
        let err = service.leave(member, None, "s3cret").await.unwrap_err();
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
        let err = service.leave(SYSTEM_DEFAULT_USER_ID, None, "").await.unwrap_err();
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
        service.leave(SYSTEM_DEFAULT_USER_ID, None, "").await.unwrap();
        assert_eq!(service.member_count(&tenant_id).await.unwrap(), 1);

        db.close().await;
    }

    #[tokio::test]
    async fn last_admin_can_leave_when_no_other_members_remain() {
        let (db, service, _user_repo) = setup().await;
        service.create_tenant(SYSTEM_DEFAULT_USER_ID, "Acme").await.unwrap();

        // Sole admin, sole member — leaving just empties the tenant, no one
        // is orphaned.
        service.leave(SYSTEM_DEFAULT_USER_ID, None, "").await.unwrap();

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

    // --- Direction B: company-owned project groups ---

    #[tokio::test]
    async fn create_for_enterprise_allows_multiple_under_same_company() {
        // Unlike the D3-guarded `create_tenant`, a company may own many groups.
        let (db, service, _repo) = setup().await;
        let (t1, _, code1) = service
            .create_tenant_for_enterprise("ent1", "Group A", SYSTEM_DEFAULT_USER_ID, None)
            .await
            .unwrap();
        let (t2, ..) = service
            .create_tenant_for_enterprise("ent1", "Group B", SYSTEM_DEFAULT_USER_ID, None)
            .await
            .unwrap();
        assert_ne!(t1, t2);
        assert!(!code1.is_empty(), "an invite is auto-generated");
        let list = service.list_tenants_by_enterprise("ent1").await.unwrap();
        assert_eq!(list.len(), 2);
        // Empty groups (no auto-join) — the crux fix.
        assert_eq!(list.iter().map(|t| t.member_count).sum::<i64>(), 0);
        db.close().await;
    }

    #[tokio::test]
    async fn create_for_enterprise_does_not_auto_join_creator() {
        let (db, service, repo) = setup().await;
        let op = create_user(&repo, "op").await;
        service
            .create_tenant_for_enterprise("ent1", "Group A", &op, None)
            .await
            .unwrap();
        // one_user_org PK = user_id is never stressed: the creator is not joined.
        assert!(service.membership(&op).await.unwrap().is_none());
        db.close().await;
    }

    #[tokio::test]
    async fn create_for_enterprise_seeds_initial_admin_across_multiple_groups() {
        let (db, service, repo) = setup().await;
        let admin = create_user(&repo, "grpadmin").await;
        service
            .create_tenant_for_enterprise("ent1", "Group A", SYSTEM_DEFAULT_USER_ID, Some(&admin))
            .await
            .unwrap();
        // Phase 2 multi-membership: seeding the same admin into a second group
        // now succeeds (composite PK), and they belong to both as org_admin.
        service
            .create_tenant_for_enterprise("ent1", "Group B", SYSTEM_DEFAULT_USER_ID, Some(&admin))
            .await
            .unwrap();
        let mine = service.list_memberships(&admin).await.unwrap();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|m| m.role == ROLE_ORG_ADMIN));
        // Exactly one active group (fallback picks the most-recently-created).
        assert_eq!(mine.iter().filter(|m| m.is_active).count(), 1);
        db.close().await;
    }

    // --- Direction B / Phase 2: multi-membership + active-tenant switching ---

    /// Helper: create the standalone tenant + a second company-owned group and
    /// have `member` join both. Returns (group1_id, group2_id).
    async fn setup_two_groups(
        service: &Arc<OrgService>,
        user_repo: &Arc<dyn IUserRepository>,
    ) -> (String, String, String) {
        let (g1, _) = service
            .create_tenant(SYSTEM_DEFAULT_USER_ID, "Group One")
            .await
            .unwrap();
        let (g2, _, code2) = service
            .create_tenant_for_enterprise("ent1", "Group Two", SYSTEM_DEFAULT_USER_ID, None)
            .await
            .unwrap();
        let (_, code1) = service
            .create_invite(&g1, SYSTEM_DEFAULT_USER_ID, None, None)
            .await
            .unwrap();
        let member = create_user(user_repo, "multi").await;
        service.join_with_invite(&member, &code1).await.unwrap();
        service.join_with_invite(&member, &code2).await.unwrap();
        (g1, g2, member)
    }

    #[tokio::test]
    async fn join_second_group_auto_activates_and_lists_both() {
        let (db, service, user_repo) = setup().await;
        let (g1, g2, member) = setup_two_groups(&service, &user_repo).await;

        // Belongs to both groups.
        let mine = service.list_memberships(&member).await.unwrap();
        assert_eq!(mine.len(), 2);
        // The most-recently-joined group (g2) is active.
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), g2);
        assert_eq!(service.tenant_of(&member).await.unwrap(), g2);
        assert!(mine.iter().find(|m| m.tenant_id == g2).unwrap().is_active);
        assert!(!mine.iter().find(|m| m.tenant_id == g1).unwrap().is_active);

        db.close().await;
    }

    #[tokio::test]
    async fn switch_active_tenant_changes_resolution() {
        let (db, service, user_repo) = setup().await;
        let (g1, g2, member) = setup_two_groups(&service, &user_repo).await;
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), g2);

        service.set_active_tenant(&member, &g1).await.unwrap();
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), g1);
        assert_eq!(service.tenant_of(&member).await.unwrap(), g1);
        let ctx = service.context(&member).await.unwrap();
        assert_eq!(ctx.tenant_id, g1);

        // Switching to a group you don't belong to is rejected.
        let err = service.set_active_tenant(&member, "tenant_bogus").await.unwrap_err();
        assert_eq!(err.code(), "NOT_IN_ENTERPRISE");

        db.close().await;
    }

    #[tokio::test]
    async fn effective_role_follows_active_tenant() {
        let (db, service, user_repo) = setup().await;
        let (g1, g2, member) = setup_two_groups(&service, &user_repo).await;
        // Promote the member to org_admin in g1 only.
        service
            .set_user_role(&g1, SYSTEM_DEFAULT_USER_ID, &member, ROLE_ORG_ADMIN)
            .await
            .unwrap();

        // Active is g2 → plain member; switch to g1 → org_admin.
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), g2);
        assert_eq!(service.effective_role(&member).await.unwrap(), ROLE_MEMBER);
        service.set_active_tenant(&member, &g1).await.unwrap();
        assert_eq!(service.effective_role(&member).await.unwrap(), ROLE_ORG_ADMIN);

        db.close().await;
    }

    #[tokio::test]
    async fn leave_active_group_reselects_remaining_and_is_scoped() {
        let (db, service, user_repo) = setup().await;
        let (g1, g2, member) = setup_two_groups(&service, &user_repo).await;
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), g2);

        // Leave the active group (g2) → still a member of g1, which becomes
        // active. Scoped: g1 membership untouched.
        service.leave(&member, Some(&g2), "").await.unwrap();
        let mine = service.list_memberships(&member).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].tenant_id, g1);
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), g1);

        // Leaving the last group falls back to personal-edition default.
        service.leave(&member, None, "").await.unwrap();
        assert!(service.list_memberships(&member).await.unwrap().is_empty());
        assert_eq!(service.active_tenant_id(&member).await.unwrap(), DEFAULT_TENANT_ID);

        db.close().await;
    }

    #[tokio::test]
    async fn active_tenant_defaults_when_no_membership() {
        // Red line: personal edition (no membership rows) resolves to the
        // default tenant with no active-tenant row, exactly as before Phase 2.
        let (db, service, user_repo) = setup().await;
        let solo = create_user(&user_repo, "solo").await;
        assert_eq!(service.active_tenant_id(&solo).await.unwrap(), DEFAULT_TENANT_ID);
        assert_eq!(service.tenant_of(&solo).await.unwrap(), DEFAULT_TENANT_ID);
        assert!(service.list_memberships(&solo).await.unwrap().is_empty());
        db.close().await;
    }
}
