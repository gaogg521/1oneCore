//! Business logic for the enterprise-org dimension. No axum imports.

use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::disband_cascade::{CompanyDisbandCascade, NoopCompanyDisbandCascade};
use crate::error::EnterpriseError;
use crate::models::{
    CompanyInviteDto, CompanyMemberDto, CompanyOverviewDto, DisbandCompanyResult, EnterpriseIdentityDto,
    ROLE_COMPANY_ADMIN, ROLE_COMPANY_MEMBER, SEAT_STATUS_ACTIVE, SEAT_STATUS_PENDING, is_company_admin_role,
};
use crate::session_revoker::{NoopSessionRevoker, SessionRevoker};

/// Desktop-operator sentinel user id (mirrors `one_org::models::SYSTEM_DEFAULT_USER_ID`).
/// Defaults to system_admin when it has no explicit `one_user_org` row.
const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";
/// one-org's instance-level admin role (mirrors `one_org::models::ROLE_SYSTEM_ADMIN`).
const ROLE_SYSTEM_ADMIN: &str = "system_admin";

pub struct EnterpriseService {
    pool: SqlitePool,
    session_revoker: std::sync::Arc<dyn SessionRevoker>,
    disband_cascade: std::sync::Arc<dyn CompanyDisbandCascade>,
}

/// Named-fields input for `upsert_member` — see that function's doc comment.
struct UpsertMemberInput<'a> {
    user_id: &'a str,
    enterprise_id: &'a str,
    display_name: Option<&'a str>,
    department: Option<&'a str>,
    job_title: Option<&'a str>,
    seat_status: &'a str,
    now: i64,
}

impl EnterpriseService {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            session_revoker: std::sync::Arc::new(NoopSessionRevoker),
            disband_cascade: std::sync::Arc::new(NoopCompanyDisbandCascade),
        }
    }

    /// Wire the credential revocation that makes a company removal actually cut
    /// off access. Required on whichever instance serves the removal route —
    /// see [`crate::session_revoker`].
    pub fn with_session_revoker(mut self, revoker: std::sync::Arc<dyn SessionRevoker>) -> Self {
        self.session_revoker = revoker;
        self
    }

    /// Wire the cross-crate cleanup that makes disbanding a company actually
    /// delete what it owns elsewhere. Required on whichever instance serves
    /// the disband route — see [`crate::disband_cascade`].
    pub fn with_disband_cascade(mut self, cascade: std::sync::Arc<dyn CompanyDisbandCascade>) -> Self {
        self.disband_cascade = cascade;
        self
    }

    /// Pool access for sibling modules in this crate (`directory`), so their
    /// `impl EnterpriseService` blocks do not need the field to be public.
    pub(crate) fn pool_ref(&self) -> &SqlitePool {
        &self.pool
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
    #[allow(clippy::too_many_arguments)]
    pub async fn sync_member(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
        personal_external_id: &str,
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

        let seat_status = self
            .resolve_seat_status_and_upsert(user_id, &enterprise_id, display_name, department, job_title, now)
            .await?;
        // Reconcile a pending invite for this exact person, if an admin sent
        // one — purely cleanup, never a gate: they already joined above
        // (auto-join on any SSO login is unchanged), this just clears their
        // "invited" card out of the Members tab now that they have a real one.
        let personal_external_id = personal_external_id.trim();
        if !personal_external_id.is_empty() {
            sqlx::query("DELETE FROM one_enterprise_invites WHERE enterprise_id = ? AND provider = ? AND external_id = ?")
                .bind(&enterprise_id)
                .bind(provider)
                .bind(personal_external_id)
                .execute(&self.pool)
                .await?;
        }
        tracing::info!(
            user_id,
            provider,
            enterprise_id,
            seat_status,
            "enterprise membership synced from SSO"
        );
        Ok(())
    }

    /// Ensure `user_id` has a membership row in `enterprise_id`, for a
    /// project-group join whose tenant belongs to that company (Direction B —
    /// see the `CompanySeatSync` hook in one-org, wired in aionui-app). Unlike
    /// `sync_member`, `enterprise_id` is given directly rather than resolved
    /// from an SSO provider/external id, since there is no IdP profile on this
    /// path; department / job title are likewise unknown here and left unset.
    ///
    /// Same active/pending seat-cap semantics as SSO sync, and idempotent for
    /// the same reason: an existing ACTIVE row is never re-evaluated.
    pub async fn ensure_member(
        &self,
        user_id: &str,
        enterprise_id: &str,
        display_name: Option<&str>,
    ) -> Result<(), EnterpriseError> {
        let now = now_ms() as i64;
        let seat_status = self
            .resolve_seat_status_and_upsert(user_id, enterprise_id, display_name, None, None, now)
            .await?;
        tracing::info!(
            user_id,
            enterprise_id,
            seat_status,
            "enterprise membership synced from project-group join"
        );
        Ok(())
    }

    /// Shared core of `sync_member` / `ensure_member`: resolve the seat status
    /// a new/refreshed row should carry (P0-3 / T6-4 — a member arriving at a
    /// full plan does not get silently dropped, see `resolve_seat_status`'s
    /// doc; they get a row, just not an ACTIVE one, so one-billing's
    /// governance resolution finds them and can deny instead of mistaking a
    /// company member for a personal/no-company user), then upsert it.
    async fn resolve_seat_status_and_upsert(
        &self,
        user_id: &str,
        enterprise_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
        now: i64,
    ) -> Result<&'static str, EnterpriseError> {
        let seat_status = self.resolve_seat_status(user_id, enterprise_id).await?;
        if seat_status == SEAT_STATUS_PENDING {
            tracing::warn!(
                user_id,
                enterprise_id,
                "seat cap reached; member synced without an active seat (pending)"
            );
        }
        self.upsert_member(UpsertMemberInput {
            user_id,
            enterprise_id,
            display_name,
            department,
            job_title,
            seat_status,
            now,
        })
        .await?;
        Ok(seat_status)
    }

    /// What `seat_status` this member's row should carry.
    ///
    /// An already-ACTIVE member is never re-evaluated: a plan downgraded below
    /// today's headcount must not silently strip governance from — or evict —
    /// people who already have a working seat. A PENDING member (or someone
    /// with no row at all) is checked against the current cap every time they
    /// sync, which is what promotes them the moment a seat frees up — there is
    /// no separate "assign a seat" admin action, "log in again" is the only
    /// mechanism and it has to actually work.
    async fn resolve_seat_status(&self, user_id: &str, enterprise_id: &str) -> Result<&'static str, EnterpriseError> {
        let existing: Option<(String, String)> =
            sqlx::query_as("SELECT enterprise_id, seat_status FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        if let Some((existing_enterprise, existing_status)) = existing
            && existing_enterprise == enterprise_id
            && existing_status == SEAT_STATUS_ACTIVE
        {
            return Ok(SEAT_STATUS_ACTIVE);
        }
        if self.active_seat_available(enterprise_id).await? {
            Ok(SEAT_STATUS_ACTIVE)
        } else {
            Ok(SEAT_STATUS_PENDING)
        }
    }

    /// Whether the plan has room for one more ACTIVE seat. Reads the
    /// one-billing license table via the shared pool; the `aionui-common`
    /// matrix is the single source for tier caps. Tolerant of a missing
    /// license table (billing not installed → unlimited) so standalone /
    /// pre-billing behavior is unchanged.
    async fn active_seat_available(&self, enterprise_id: &str) -> Result<bool, EnterpriseError> {
        // Distinguish table-missing (skip) from row-absent (new company → free
        // default).
        let tier = match sqlx::query_as::<_, (String, Option<i64>)>(
            "SELECT tier, seat_limit FROM one_enterprise_license WHERE enterprise_id = ?",
        )
        .bind(enterprise_id)
        .fetch_optional(&self.pool)
        .await
        {
            Err(_) => return Ok(true), // billing not installed → no enforcement
            Ok(Some((tier, Some(override_limit)))) => {
                let _ = tier;
                return self.active_seat_count_below(enterprise_id, override_limit).await;
            }
            Ok(Some((tier, None))) => aionui_common::license::Tier::parse(&tier),
            Ok(None) => aionui_common::license::Tier::Free,
        };
        match aionui_common::license::tier_seat_limit(tier) {
            Some(limit) => self.active_seat_count_below(enterprise_id, limit as i64).await,
            None => Ok(true), // unlimited tier
        }
    }

    async fn active_seat_count_below(&self, enterprise_id: &str, limit: i64) -> Result<bool, EnterpriseError> {
        let used: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ? AND seat_status = ?",
        )
        .bind(enterprise_id)
        .bind(SEAT_STATUS_ACTIVE)
        .fetch_one(&self.pool)
        .await?;
        Ok(used < limit)
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
    ///
    /// `pub` so the T6 directory sync can ask "is there a company here at all",
    /// which is half of its should-I-run gate — a machine with no company has
    /// no directory to sync.
    pub async fn deployment_company_id(&self) -> Result<Option<String>, EnterpriseError> {
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
    ///
    /// A named-fields struct rather than positional args: this crate has
    /// several adjacent `Option<&str>` parameters (display_name, department,
    /// job_title, seat_status) and a mis-ordered call site would compile and
    /// silently write the wrong field to the wrong column.
    async fn upsert_member(&self, input: UpsertMemberInput<'_>) -> Result<(), EnterpriseError> {
        let UpsertMemberInput {
            user_id,
            enterprise_id,
            display_name,
            department,
            job_title,
            seat_status,
            now,
        } = input;
        let display_name = display_name.map(str::trim).filter(|s| !s.is_empty());
        let department = department.map(str::trim).filter(|s| !s.is_empty());
        let job_title = job_title.map(str::trim).filter(|s| !s.is_empty());
        sqlx::query(
            "INSERT INTO one_enterprise_members \
             (user_id, enterprise_id, display_name, department, job_title, role, seat_status, joined_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'member', ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET enterprise_id = excluded.enterprise_id, \
                 display_name = excluded.display_name, department = excluded.department, \
                 job_title = excluded.job_title, seat_status = excluded.seat_status, \
                 updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(enterprise_id)
        .bind(display_name)
        .bind(department)
        .bind(job_title)
        .bind(seat_status)
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
    /// (`system_default_user`) is system_admin by default. Phase 2
    /// multi-membership: role is scoped to the caller's *active* tenant (active
    /// membership first, else most-recently-joined).
    async fn caller_is_system_admin(&self, user_id: &str) -> Result<bool, EnterpriseError> {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT uo.role FROM one_user_org uo WHERE uo.user_id = ? \
             ORDER BY (uo.tenant_id = (SELECT tenant_id FROM one_active_tenant WHERE user_id = uo.user_id)) DESC, \
                      uo.created_at DESC, uo.tenant_id ASC LIMIT 1",
        )
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

    /// Renames the company. `enterprise_id` comes from `RequireCompanyAdmin`
    /// (already role-checked), so this only validates the new name — same
    /// non-empty/trim rule `setup_company` uses at creation time.
    pub async fn rename_company(
        &self,
        user_id: &str,
        enterprise_id: &str,
        name_raw: &str,
    ) -> Result<CompanyOverviewDto, EnterpriseError> {
        let name = name_raw.trim();
        if name.is_empty() {
            return Err(EnterpriseError::NameRequired);
        }
        let now = now_ms() as i64;
        sqlx::query("UPDATE one_enterprises SET display_name = ?, updated_at = ? WHERE id = ?")
            .bind(name)
            .bind(now)
            .bind(enterprise_id)
            .execute(&self.pool)
            .await?;
        tracing::info!(user_id, enterprise_id, "company renamed");
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
                String,
            ),
        >(
            "SELECT m.user_id, u.username, m.display_name, m.department, m.job_title, m.role, m.seat_status \
             FROM one_enterprise_members m LEFT JOIN users u ON u.id = m.user_id \
             WHERE m.enterprise_id = ? ORDER BY m.joined_at ASC",
        )
        .bind(enterprise_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(user_id, username, display_name, department, job_title, role, seat_status)| CompanyMemberDto {
                    user_id,
                    username,
                    display_name,
                    department,
                    job_title,
                    role,
                    seat_status,
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

    /// Remove a member from the company, releasing their seat (P0-2).
    ///
    /// Seats are counted as `one_enterprise_members` rows (see `seat_used` /
    /// the licence check in this file), so deleting the row *is* the seat
    /// reclamation — there is no separate counter to decrement.
    ///
    /// Note this only detaches the user from the **company**; project-group
    /// membership lives in `one_user_org` and is removed separately by
    /// `OrgService::remove_member`. The two tiers are deliberately independent
    /// (企业 ⊃ 项目组), so an offboarding flow calls both.
    ///
    /// It does, however, revoke their credentials — see
    /// [`crate::session_revoker`]. Company membership is the identity tier, and
    /// a member who has been removed from the company but is still holding a
    /// live session is the exact failure the departure flow exists to prevent.
    /// A person who is in no project group has no other removal to fall back
    /// on, so this call has to be sufficient on its own.
    pub async fn remove_member(
        &self,
        enterprise_id: &str,
        actor_user_id: &str,
        target_user_id: &str,
    ) -> Result<(), EnterpriseError> {
        if actor_user_id == target_user_id {
            return Err(EnterpriseError::Forbidden(
                "cannot remove yourself from the company".into(),
            ));
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
        // Same guard as demoting the last admin: a company with zero admins
        // can never be administered again.
        if is_company_admin_role(&current) {
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
        sqlx::query("DELETE FROM one_enterprise_members WHERE user_id = ? AND enterprise_id = ?")
            .bind(target_user_id)
            .bind(enterprise_id)
            .execute(&self.pool)
            .await?;
        // After the delete, never before: a guard rejection above must not cost
        // somebody their session, and a revocation failure must not leave the
        // seat occupied.
        self.session_revoker.revoke_sessions(target_user_id).await;
        Ok(())
    }

    /// Invite a directory person: an admin picked them out of the synced
    /// Feishu directory. Purely a labelled placeholder + shareable link — it
    /// does NOT gate `sync_member`, which still auto-joins any successful SSO
    /// login regardless of whether an invite exists (2026-08-20 product
    /// decision: invites are pre-registration, not an access control list).
    /// Re-inviting the same `(provider, external_id)` replaces the row
    /// (`ON CONFLICT` on the unique index from migration 005) rather than
    /// erroring, so nudging someone a second time just refreshes their card.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_invite(
        &self,
        enterprise_id: &str,
        created_by: &str,
        provider: &str,
        external_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
    ) -> Result<CompanyInviteDto, EnterpriseError> {
        let external_id = external_id.trim();
        if external_id.is_empty() {
            return Err(EnterpriseError::InviteExternalIdRequired);
        }
        let now = now_ms() as i64;
        let id = uuid::Uuid::now_v7().simple().to_string();
        sqlx::query(
            "INSERT INTO one_enterprise_invites \
             (id, enterprise_id, provider, external_id, display_name, department, job_title, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(enterprise_id, provider, external_id) DO UPDATE SET \
                 id = excluded.id, display_name = excluded.display_name, department = excluded.department, \
                 job_title = excluded.job_title, created_by = excluded.created_by, created_at = excluded.created_at",
        )
        .bind(&id)
        .bind(enterprise_id)
        .bind(provider)
        .bind(external_id)
        .bind(display_name)
        .bind(department)
        .bind(job_title)
        .bind(created_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(CompanyInviteDto {
            id,
            provider: provider.to_string(),
            external_id: external_id.to_string(),
            display_name: display_name.map(str::to_string),
            department: department.map(str::to_string),
            job_title: job_title.map(str::to_string),
            created_at: now,
        })
    }

    pub async fn list_invites(&self, enterprise_id: &str) -> Result<Vec<CompanyInviteDto>, EnterpriseError> {
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, Option<String>, i64)>(
            "SELECT id, provider, external_id, display_name, department, job_title, created_at \
             FROM one_enterprise_invites WHERE enterprise_id = ? ORDER BY created_at DESC",
        )
        .bind(enterprise_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, provider, external_id, display_name, department, job_title, created_at)| CompanyInviteDto {
                    id,
                    provider,
                    external_id,
                    display_name,
                    department,
                    job_title,
                    created_at,
                },
            )
            .collect())
    }

    pub async fn revoke_invite(&self, enterprise_id: &str, invite_id: &str) -> Result<(), EnterpriseError> {
        let result = sqlx::query("DELETE FROM one_enterprise_invites WHERE id = ? AND enterprise_id = ?")
            .bind(invite_id)
            .bind(enterprise_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(EnterpriseError::InviteNotFound);
        }
        Ok(())
    }

    /// Permanently deletes the company: every project group it owns
    /// (one-org, via [`crate::disband_cascade`]), every enterprise-scoped
    /// billing/usage record (one-billing, same trait), every company
    /// membership, and the company record itself. Irreversible — there is
    /// no "undo" short of the JSON snapshot `disband_tenants_for_enterprise`
    /// archives on the one-org side.
    ///
    /// Every member's session is revoked, mirroring `remove_member` — a
    /// disbanded company must not leave anyone still logged into it.
    pub async fn disband_company(
        &self,
        actor_user_id: &str,
        enterprise_id: &str,
    ) -> Result<DisbandCompanyResult, EnterpriseError> {
        if !self.is_company_admin_of(actor_user_id, enterprise_id).await? {
            return Err(EnterpriseError::Forbidden(
                "only a company admin can disband the company".into(),
            ));
        }
        let member_ids: Vec<String> =
            sqlx::query_scalar("SELECT user_id FROM one_enterprise_members WHERE enterprise_id = ?")
                .bind(enterprise_id)
                .fetch_all(&self.pool)
                .await?;

        // one-org (project groups) + one-billing (usage/license history)
        // cleanup first: if either fails partway, the company record — and
        // this member's own admin access to retry — stays in place.
        let deleted_project_groups = self.disband_cascade.disband(enterprise_id).await;

        sqlx::query("DELETE FROM one_enterprise_members WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM one_enterprises WHERE id = ?")
            .bind(enterprise_id)
            .execute(&self.pool)
            .await?;

        for member_id in &member_ids {
            self.session_revoker.revoke_sessions(member_id).await;
        }

        tracing::warn!(
            enterprise_id,
            actor_user_id,
            member_count = member_ids.len(),
            project_group_count = deleted_project_groups.len(),
            "company disbanded (企业注销)"
        );
        Ok(DisbandCompanyResult {
            deleted_project_group_ids: deleted_project_groups,
            removed_member_count: member_ids.len() as i64,
        })
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
            "",
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
    async fn free_tier_seat_cap_blocks_new_members_but_not_relogin() {
        let svc = service().await;
        // Simulate one-billing installed with a free-tier license (cap 3).
        sqlx::raw_sql(
            "CREATE TABLE one_enterprise_license (enterprise_id TEXT PRIMARY KEY, tier TEXT NOT NULL DEFAULT 'free', seat_limit INTEGER, expires_at INTEGER, updated_at INTEGER NOT NULL);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();

        // First member creates the company; license it 'free'.
        svc.sync_member("u1", "feishu", "co", "", None, None, None).await.unwrap();
        let eid: String = sqlx::query_scalar("SELECT id FROM one_enterprises LIMIT 1")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO one_enterprise_license (enterprise_id, tier, updated_at) VALUES (?, 'free', 0)")
            .bind(&eid)
            .execute(&svc.pool)
            .await
            .unwrap();

        // Seats 2 and 3 fit (cap 3), seat 4 does not — but T6-4: it must still
        // succeed and leave a row, just not an ACTIVE one. Silently dropping it
        // (the old behavior) is exactly the bug this test now guards against:
        // no row means one-billing's `resolve_enterprise_id` finds nothing and
        // treats a company member as a personal user with zero governance.
        svc.sync_member("u2", "feishu", "co", "", None, None, None).await.unwrap();
        svc.sync_member("u3", "feishu", "co", "", None, None, None).await.unwrap();
        svc.sync_member("u4", "feishu", "co", "", None, None, None).await.unwrap();
        assert_eq!(seat_status_of(&svc, "u4").await, SEAT_STATUS_PENDING);
        assert_eq!(active_seat_count(&svc, &eid).await, 3);

        // An existing ACTIVE member re-logging in is never blocked or
        // re-evaluated, even at the cap.
        svc.sync_member("u1", "feishu", "co", "", Some("赵高"), None, None)
            .await
            .unwrap();
        assert_eq!(seat_status_of(&svc, "u1").await, SEAT_STATUS_ACTIVE);

        // Upgrading the plan does NOT retroactively promote a pending member —
        // there is no background job, only "try again next login".
        sqlx::query("UPDATE one_enterprise_license SET tier = 'team' WHERE enterprise_id = ?")
            .bind(&eid)
            .execute(&svc.pool)
            .await
            .unwrap();
        assert_eq!(seat_status_of(&svc, "u4").await, SEAT_STATUS_PENDING);

        // u4's NEXT login re-checks the cap and promotes them.
        svc.sync_member("u4", "feishu", "co", "", None, None, None).await.unwrap();
        assert_eq!(seat_status_of(&svc, "u4").await, SEAT_STATUS_ACTIVE);
    }

    /// ⚠️ The point of the whole column. If a pending row were never written
    /// (the pre-fix behavior), `resolve_enterprise_id` in one-billing would
    /// find nothing for u4 and treat them as a personal user — every
    /// governance gate silently off. A row must exist so one-billing can find
    /// AND deny them, not find nothing and wave them through.
    #[tokio::test]
    async fn a_member_over_the_seat_cap_still_gets_a_row_one_billing_can_find() {
        let svc = service().await;
        sqlx::raw_sql(
            "CREATE TABLE one_enterprise_license (enterprise_id TEXT PRIMARY KEY, tier TEXT NOT NULL DEFAULT 'free', seat_limit INTEGER, expires_at INTEGER, updated_at INTEGER NOT NULL);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
        svc.sync_member("u1", "feishu", "co", "", None, None, None).await.unwrap();
        let eid: String = sqlx::query_scalar("SELECT id FROM one_enterprises LIMIT 1")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, seat_limit, updated_at) VALUES (?, 'free', 1, 0)",
        )
        .bind(&eid)
        .execute(&svc.pool)
        .await
        .unwrap();

        svc.sync_member("u2", "feishu", "co", "", None, None, None).await.unwrap();

        let row: Option<(String, String)> =
            sqlx::query_as("SELECT enterprise_id, seat_status FROM one_enterprise_members WHERE user_id = 'u2'")
                .fetch_optional(&svc.pool)
                .await
                .unwrap();
        let (row_eid, status) = row.expect("a row must exist for one-billing to find, or governance is bypassed");
        assert_eq!(row_eid, eid);
        assert_eq!(status, SEAT_STATUS_PENDING);
    }

    /// A plan later lowered below today's headcount must not evict or
    /// de-govern people who already had a working seat.
    #[tokio::test]
    async fn lowering_the_cap_never_demotes_an_existing_active_member() {
        let svc = service().await;
        sqlx::raw_sql(
            "CREATE TABLE one_enterprise_license (enterprise_id TEXT PRIMARY KEY, tier TEXT NOT NULL DEFAULT 'free', seat_limit INTEGER, expires_at INTEGER, updated_at INTEGER NOT NULL);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
        svc.sync_member("u1", "feishu", "co", "", None, None, None).await.unwrap();
        let eid: String = sqlx::query_scalar("SELECT id FROM one_enterprises LIMIT 1")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, seat_limit, updated_at) VALUES (?, 'free', 5, 0)",
        )
        .bind(&eid)
        .execute(&svc.pool)
        .await
        .unwrap();
        svc.sync_member("u2", "feishu", "co", "", None, None, None).await.unwrap();
        assert_eq!(seat_status_of(&svc, "u2").await, SEAT_STATUS_ACTIVE);

        // Cap dropped to 1 — below the current headcount of 2.
        sqlx::query("UPDATE one_enterprise_license SET seat_limit = 1 WHERE enterprise_id = ?")
            .bind(&eid)
            .execute(&svc.pool)
            .await
            .unwrap();

        svc.sync_member("u2", "feishu", "co", "", None, None, None).await.unwrap();
        assert_eq!(
            seat_status_of(&svc, "u2").await,
            SEAT_STATUS_ACTIVE,
            "an already-active member must never be re-evaluated against a later, lower cap"
        );
    }

    /// `ensure_member` is the project-group-join counterpart of `sync_member`
    /// (see its doc comment / the `CompanySeatSync` hook in one-org): a
    /// company-owned tenant's invite code registers the joiner as a company
    /// member the same way an SSO login does, respecting the same seat cap.
    #[tokio::test]
    async fn ensure_member_respects_the_seat_cap_like_sso_sync() {
        let svc = service().await;
        sqlx::raw_sql(
            "CREATE TABLE one_enterprise_license (enterprise_id TEXT PRIMARY KEY, tier TEXT NOT NULL DEFAULT 'free', seat_limit INTEGER, expires_at INTEGER, updated_at INTEGER NOT NULL);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, seat_limit, updated_at) VALUES ('ent_test1', 'free', 1, 0)",
        )
        .execute(&svc.pool)
        .await
        .unwrap();

        svc.ensure_member("u1", "ent_test1", Some("张三")).await.unwrap();
        assert_eq!(seat_status_of(&svc, "u1").await, SEAT_STATUS_ACTIVE);

        // Cap of 1 is already full — the second joiner gets a row (so
        // one-billing's governance can find and deny them) but pending, not
        // silently dropped.
        svc.ensure_member("u2", "ent_test1", Some("李四")).await.unwrap();
        assert_eq!(seat_status_of(&svc, "u2").await, SEAT_STATUS_PENDING);

        // Idempotent: calling it again for an already-ACTIVE member changes
        // nothing (never re-evaluated, mirroring `sync_member`).
        svc.ensure_member("u1", "ent_test1", Some("张三")).await.unwrap();
        assert_eq!(seat_status_of(&svc, "u1").await, SEAT_STATUS_ACTIVE);

        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT enterprise_id, display_name FROM one_enterprise_members WHERE user_id = 'u1'")
                .fetch_optional(&svc.pool)
                .await
                .unwrap();
        let (eid, name) = row.expect("membership row must exist");
        assert_eq!(eid, "ent_test1");
        assert_eq!(name.as_deref(), Some("张三"));
    }

    async fn seat_status_of(svc: &EnterpriseService, user_id: &str) -> String {
        sqlx::query_scalar("SELECT seat_status FROM one_enterprise_members WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&svc.pool)
            .await
            .unwrap()
    }

    async fn active_seat_count(svc: &EnterpriseService, enterprise_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ? AND seat_status = 'active'",
        )
        .bind(enterprise_id)
        .fetch_one(&svc.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn same_company_logins_converge_on_one_enterprise_row() {
        let svc = service().await;
        svc.sync_member("u1", "feishu", "tenant_huanle", "", None, Some("研发"), None)
            .await
            .unwrap();
        svc.sync_member("u2", "feishu", "tenant_huanle", "", None, Some("产品"), None)
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
        svc.sync_member("u1", "feishu", "  ", "", Some("x"), None, None)
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
        svc.sync_member("u1", "feishu", "", "", Some("赵高"), None, None)
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
        svc.sync_member("op", "feishu", "", "", Some("Op"), None, None)
            .await
            .unwrap();
        assert!(svc.is_company_admin("op").await.unwrap());
    }

    #[tokio::test]
    async fn sync_member_without_company_is_noop() {
        // Lock-in: no explicit company AND no tenant_key → nothing written. This
        // is the personal / standalone path (which never reaches SSO anyway).
        let svc = service().await;
        svc.sync_member("u1", "feishu", "", "", Some("x"), None, None)
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
            "CREATE TABLE one_user_org (user_id TEXT, tenant_id TEXT, role TEXT NOT NULL DEFAULT 'member', \
             created_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id))",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Phase 2: `caller_is_system_admin` scopes to the active tenant, so the
        // cross-domain `one_active_tenant` table must exist too (empty is fine).
        sqlx::query(
            "CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0)",
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
    async fn rename_company_updates_display_name() {
        let svc = service_with_governance().await;
        svc.setup_company("system_default_user", "Acme").await.unwrap();
        let enterprise_id = svc.company_of("system_default_user").await.unwrap().unwrap();

        let overview = svc
            .rename_company("system_default_user", &enterprise_id, "Acme Corp")
            .await
            .unwrap();

        assert_eq!(overview.name.as_deref(), Some("Acme Corp"));
        // Renaming must not disturb membership/role — regression guard for a
        // rename that accidentally re-touches `one_enterprise_members`.
        assert_eq!(overview.member_count, 1);
        assert_eq!(overview.viewer_role.as_deref(), Some("admin"));
        assert!(svc.is_company_admin("system_default_user").await.unwrap());

        // The route layer resolves `enterprise_id` via `RequireCompanyAdmin`
        // before calling this, but the service method itself must still
        // refuse an empty name — trusting a pre-validated caller here would
        // leave the service unsafe to call from anywhere else later.
        let err = svc.rename_company("system_default_user", &enterprise_id, "   ").await.unwrap_err();
        assert_eq!(err.code(), "COMPANY_NAME_REQUIRED");
        // ...and the rejected empty-name call must not have touched the row.
        let overview = svc.company_overview("system_default_user").await.unwrap().unwrap();
        assert_eq!(overview.name.as_deref(), Some("Acme Corp"));
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
        svc.sync_member("u2", "feishu", "", "", Some("Bob"), None, None)
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

    /// Records who was revoked, so the tests can tell "we removed them" apart
    /// from "we also cut off their access".
    #[derive(Default)]
    struct RecordingRevoker(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl SessionRevoker for RecordingRevoker {
        async fn revoke_sessions(&self, user_id: &str) {
            self.0.lock().unwrap().push(user_id.to_string());
        }
    }

    /// ⚠️ The point of the whole departure flow. Deleting the seat row without
    /// revoking leaves the leaver holding a valid session and a company model
    /// channel token — and somebody in no project group has no second removal
    /// to fall back on.
    #[tokio::test]
    async fn removing_a_company_member_cuts_off_their_access() {
        let revoker = std::sync::Arc::new(RecordingRevoker::default());
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_enterprise_migrations(db.pool()).await.unwrap();
        let svc = EnterpriseService::new(db.pool().clone()).with_session_revoker(revoker.clone());

        svc.sync_member("u1", "feishu", "co", "", None, None, None).await.unwrap();
        svc.sync_member("u2", "feishu", "co", "", None, None, None).await.unwrap();
        let ent: String = sqlx::query_scalar("SELECT id FROM one_enterprises LIMIT 1")
            .fetch_one(&svc.pool)
            .await
            .unwrap();

        svc.remove_member(&ent, "u1", "u2").await.unwrap();
        assert_eq!(revoker.0.lock().unwrap().as_slice(), ["u2"]);
    }

    /// A rejected removal must not cost anybody their session — the guards run
    /// before the revocation for exactly this reason.
    #[tokio::test]
    async fn a_rejected_removal_revokes_nothing() {
        let revoker = std::sync::Arc::new(RecordingRevoker::default());
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_enterprise_migrations(db.pool()).await.unwrap();
        let svc = EnterpriseService::new(db.pool().clone()).with_session_revoker(revoker.clone());

        svc.sync_member("u1", "feishu", "co", "", None, None, None).await.unwrap();
        let ent: String = sqlx::query_scalar("SELECT id FROM one_enterprises LIMIT 1")
            .fetch_one(&svc.pool)
            .await
            .unwrap();

        // Not a member of this company.
        assert!(svc.remove_member(&ent, "u1", "ghost").await.is_err());
        // Removing yourself is refused too.
        assert!(svc.remove_member(&ent, "u1", "u1").await.is_err());
        assert!(revoker.0.lock().unwrap().is_empty());
    }

    /// Records the enterprise ids it was asked to disband, so a test can tell
    /// "the cascade ran" apart from "the cascade ran for the right company".
    #[derive(Default)]
    struct RecordingDisbandCascade(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl CompanyDisbandCascade for RecordingDisbandCascade {
        async fn disband(&self, enterprise_id: &str) -> Vec<String> {
            self.0.lock().unwrap().push(enterprise_id.to_string());
            vec!["tenant-a".to_string(), "tenant-b".to_string()]
        }
    }

    /// ⚠️ The point of the whole disband flow: the company, its cross-crate
    /// cascade (one-org's project groups, one-billing's history — via the
    /// trait), and every member's session must all go together. Leaving any
    /// one of them behind would be "注销" in name only.
    #[tokio::test]
    async fn disbanding_a_company_cascades_and_revokes_every_member() {
        let revoker = std::sync::Arc::new(RecordingRevoker::default());
        let cascade = std::sync::Arc::new(RecordingDisbandCascade::default());
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_enterprise_migrations(db.pool()).await.unwrap();
        sqlx::query(
            "CREATE TABLE one_user_org (user_id TEXT, tenant_id TEXT, role TEXT NOT NULL DEFAULT 'member', \
             created_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id))",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT)")
            .execute(db.pool())
            .await
            .unwrap();
        let svc = EnterpriseService::new(db.pool().clone())
            .with_session_revoker(revoker.clone())
            .with_disband_cascade(cascade.clone());

        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;
        svc.sync_member("u2", "feishu", "", "", Some("Bob"), None, None)
            .await
            .unwrap();
        assert_eq!(svc.list_members(&ent).await.unwrap().len(), 2);

        let result = svc.disband_company("system_default_user", &ent).await.unwrap();
        assert_eq!(result.removed_member_count, 2);
        assert_eq!(result.deleted_project_group_ids, vec!["tenant-a", "tenant-b"]);

        // The cascade ran for THIS company, not some default/empty id.
        assert_eq!(cascade.0.lock().unwrap().as_slice(), [ent.clone()]);
        // Both the admin and the SSO-synced member lost their sessions.
        let mut revoked = revoker.0.lock().unwrap().clone();
        revoked.sort();
        assert_eq!(revoked, ["system_default_user".to_string(), "u2".to_string()]);

        // The company itself is gone, not just emptied.
        assert!(svc.company_overview("system_default_user").await.unwrap().is_none());
        let ent_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprises WHERE id = ?")
            .bind(&ent)
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(ent_count, 0, "the enterprise row must not survive its own disband");
        let member_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ?")
                .bind(&ent)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(
            member_count, 0,
            "no membership row may survive the company it belonged to"
        );
    }

    /// A non-admin asking to disband the company must not get to run the
    /// cascade at all — the destructive part has to be behind the same guard
    /// as everything else here, not just gated by luck at the route layer.
    #[tokio::test]
    async fn disband_company_rejected_for_non_admin() {
        let cascade = std::sync::Arc::new(RecordingDisbandCascade::default());
        let svc = service_with_governance().await.with_disband_cascade(cascade.clone());
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;
        svc.sync_member("bob", "feishu", "", "", Some("Bob"), None, None)
            .await
            .unwrap();

        let err = svc.disband_company("bob", &ent).await.unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
        assert!(
            cascade.0.lock().unwrap().is_empty(),
            "cascade must not run on a rejected disband"
        );
        assert!(svc.company_overview("system_default_user").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn create_invite_then_list_shows_it() {
        let svc = service_with_governance().await;
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;

        let invite = svc
            .create_invite(
                &ent,
                "system_default_user",
                "feishu",
                "ou_zhaogao",
                Some("赵高"),
                Some("信息安全中心"),
                Some("信息安全总监"),
            )
            .await
            .unwrap();
        assert_eq!(invite.display_name.as_deref(), Some("赵高"));

        let invites = svc.list_invites(&ent).await.unwrap();
        assert_eq!(invites.len(), 1);
        assert_eq!(invites[0].external_id, "ou_zhaogao");
    }

    #[tokio::test]
    async fn create_invite_rejects_empty_external_id() {
        let svc = service_with_governance().await;
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let err = svc
            .create_invite(&overview.company_id, "system_default_user", "feishu", "  ", None, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "INVITE_EXTERNAL_ID_REQUIRED");
    }

    #[tokio::test]
    async fn re_inviting_the_same_person_replaces_not_duplicates() {
        let svc = service_with_governance().await;
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;

        svc.create_invite(&ent, "system_default_user", "feishu", "ou_zhaogao", Some("赵高"), None, None)
            .await
            .unwrap();
        svc.create_invite(
            &ent,
            "system_default_user",
            "feishu",
            "ou_zhaogao",
            Some("赵高"),
            Some("新部门"),
            None,
        )
        .await
        .unwrap();

        let invites = svc.list_invites(&ent).await.unwrap();
        assert_eq!(invites.len(), 1, "re-inviting must replace, not duplicate");
        assert_eq!(invites[0].department.as_deref(), Some("新部门"));
    }

    #[tokio::test]
    async fn revoke_invite_removes_it_and_rejects_unknown_id() {
        let svc = service_with_governance().await;
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;
        let invite = svc
            .create_invite(&ent, "system_default_user", "feishu", "ou_zhaogao", None, None, None)
            .await
            .unwrap();

        svc.revoke_invite(&ent, &invite.id).await.unwrap();
        assert!(svc.list_invites(&ent).await.unwrap().is_empty());

        let err = svc.revoke_invite(&ent, &invite.id).await.unwrap_err();
        assert_eq!(err.code(), "INVITE_NOT_FOUND");
    }

    /// The end-to-end point of this whole feature: an admin invites a
    /// specific directory person, that person completes real SSO login, and
    /// their "invited" card disappears from the pending list because they
    /// now have a real membership row. Does NOT touch access — the invite
    /// existing or not must have zero bearing on whether login succeeds
    /// (2026-08-20 product decision), which the second half of this test
    /// locks down with a negative case.
    #[tokio::test]
    async fn sso_login_consumes_the_matching_invite() {
        let svc = service_with_governance().await;
        let overview = svc.setup_company("system_default_user", "Acme").await.unwrap();
        let ent = overview.company_id;
        svc.create_invite(
            &ent,
            "system_default_user",
            "feishu",
            "ou_zhaogao",
            Some("赵高"),
            Some("信息安全中心"),
            Some("信息安全总监"),
        )
        .await
        .unwrap();

        // 赵高 logs in for real: local user_id "u_zhaogao", their own IdP id
        // "ou_zhaogao" matches the invite above.
        svc.sync_member(
            "u_zhaogao",
            "feishu",
            "co",
            "ou_zhaogao",
            Some("赵高"),
            Some("信息安全中心"),
            Some("信息安全总监"),
        )
        .await
        .unwrap();

        assert!(
            svc.list_invites(&ent).await.unwrap().is_empty(),
            "the consumed invite must be gone"
        );
        // ...and they are a real member regardless — the invite was never
        // load-bearing for access.
        assert!(!svc.is_company_admin("u_zhaogao").await.unwrap());
        let members = svc.list_members(&ent).await.unwrap();
        assert!(members.iter().any(|m| m.user_id == "u_zhaogao"));
    }

    #[tokio::test]
    async fn sso_login_without_any_invite_still_joins() {
        // Negative case for the product decision: a totally uninvited login
        // still auto-joins (unchanged existing behavior) — invites are
        // pre-registration, never a gate.
        let svc = service_with_governance().await;
        svc.setup_company("system_default_user", "Acme").await.unwrap();

        svc.sync_member("random_person", "feishu", "co", "ou_unrelated", Some("路人"), None, None)
            .await
            .unwrap();

        let overview = svc.company_overview("system_default_user").await.unwrap().unwrap();
        assert_eq!(overview.member_count, 2, "uninvited login still joins the sole company");
    }
}
