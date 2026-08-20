//! one-devops service — requirements board + collaboration registries.

use std::collections::HashMap;

use sqlx::SqlitePool;

use aionui_common::now_ms;

use crate::embedding::EmbeddingConfig;
use crate::error::DevopsError;
use crate::models::{
    MILESTONE_STATUSES, McpRegistryDto, MilestoneDto, PIPELINE_RUN_STATUSES, PIPELINE_STATUSES, PIPELINE_TRIGGERS,
    PipelineDto, PipelineRunDto, REQUIREMENT_PRIORITIES, REQUIREMENT_STATUSES, REQUIREMENT_TYPES, RagConfigDto,
    RagDocumentDto, RagSearchHit, RequirementCommentDto, RequirementDto, RequirementRow, SkillRegistryDto,
    TEST_CASE_STATUSES, TEST_PLAN_STATUSES, TestCaseDto, TestPlanDto,
};

pub struct DevopsService {
    pub(crate) pool: SqlitePool,
    /// Deployment data-encryption key, for the one thing in this crate that
    /// holds a real credential: company model channels
    /// (`provider_channel`). `None` when the app did not wire one — channel
    /// writes then fail loudly rather than storing a key in the clear, which
    /// is the failure mode that would actually hurt.
    pub(crate) encryption_key: Option<[u8; 32]>,
}

#[derive(Debug, Default)]
pub struct CreateRequirementInput {
    pub parent_id: Option<String>,
    pub kind: Option<String>,
    pub subject: String,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub milestone_id: Option<String>,
    pub autopilot: Option<bool>,
}

#[derive(Debug, Default)]
pub struct UpdateRequirementInput {
    pub subject: Option<String>,
    pub description: Option<Option<String>>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub assigned_to: Option<Option<String>>,
    pub parent_id: Option<Option<String>>,
    pub milestone_id: Option<Option<String>>,
    pub autopilot: Option<bool>,
}

fn new_id(prefix: &str) -> String {
    // Full UUIDv7. The previous `[..12]` truncation kept only the leading
    // 48 bits — which in v7 are purely the millisecond timestamp — so two
    // ids minted in the same millisecond collided (UNIQUE constraint
    // failures under bursts, e.g. requirement breakdown inserting children).
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}

fn validate_one_of(value: &str, allowed: &[&str], label: &str) -> Result<(), DevopsError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(DevopsError::BadRequest(format!(
            "invalid {label}: {value} (allowed: {})",
            allowed.join("/")
        )))
    }
}

impl DevopsService {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            encryption_key: None,
        }
    }

    /// Wire the deployment's data-encryption key so company model channels can
    /// store their credential. Builder rather than a `new` parameter so the
    /// many existing call sites (and every test) stay untouched.
    pub fn with_encryption_key(mut self, key: [u8; 32]) -> Self {
        self.encryption_key = Some(key);
        self
    }

    // -- requirements -----------------------------------------------------

    /// Full requirements forest, children nested, roots + children both
    /// ordered by updated_at DESC (matches the 1one tree endpoint).
    pub async fn requirements_tree(&self, tenant_id: &str) -> Result<Vec<RequirementDto>, DevopsError> {
        let rows = sqlx::query_as::<_, RequirementRow>(
            "SELECT id, parent_id, type, subject, description, status, priority, assigned_to, \
                    milestone_id, autopilot, creator_id, creator_name, created_at, updated_at \
             FROM one_requirements WHERE tenant_id = ? ORDER BY updated_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        let mut nodes: Vec<RequirementDto> = rows.into_iter().map(RequirementDto::from_row).collect();
        // Detach children from the flat list into their parents. Orphans
        // (parent deleted concurrently) surface as roots rather than vanish.
        let ids: std::collections::HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let mut children_of: HashMap<String, Vec<RequirementDto>> = HashMap::new();
        let mut roots: Vec<RequirementDto> = Vec::new();
        for node in nodes.drain(..) {
            match node.parent_id.clone().filter(|p| ids.contains(p)) {
                Some(parent) => children_of.entry(parent).or_default().push(node),
                None => roots.push(node),
            }
        }
        fn attach(node: &mut RequirementDto, children_of: &mut HashMap<String, Vec<RequirementDto>>) {
            if let Some(mut children) = children_of.remove(&node.id) {
                for child in &mut children {
                    attach(child, children_of);
                }
                node.children = children;
            }
        }
        for root in &mut roots {
            attach(root, &mut children_of);
        }
        Ok(roots)
    }

    pub async fn create_requirement(
        &self,
        tenant_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        input: CreateRequirementInput,
    ) -> Result<RequirementDto, DevopsError> {
        let subject = input.subject.trim();
        if subject.is_empty() {
            return Err(DevopsError::BadRequest("subject is required".into()));
        }
        let kind = input.kind.as_deref().unwrap_or("task");
        validate_one_of(kind, REQUIREMENT_TYPES, "type")?;
        let priority = input.priority.as_deref().unwrap_or("medium");
        validate_one_of(priority, REQUIREMENT_PRIORITIES, "priority")?;
        if let Some(parent_id) = input.parent_id.as_deref() {
            self.require_requirement(tenant_id, parent_id).await?;
        }

        let id = new_id("req");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_requirements \
                (id, parent_id, type, subject, description, status, priority, assigned_to, \
                 milestone_id, autopilot, creator_id, creator_name, tenant_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'backlog', ?, NULL, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.parent_id)
        .bind(kind)
        .bind(subject)
        .bind(&input.description)
        .bind(priority)
        .bind(&input.milestone_id)
        .bind(input.autopilot.unwrap_or(false))
        .bind(creator_id)
        .bind(creator_name)
        .bind(tenant_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(RequirementDto::from_row(self.fetch_requirement(tenant_id, &id).await?))
    }

    /// Create the parsed breakdown children under `parent_id` (A1 L2). Each
    /// item's kind/priority is already clamped to a valid enum. Returns the
    /// created rows. Fields already validated upstream, so a per-child failure
    /// is unexpected and aborts the batch.
    pub async fn create_breakdown_children(
        &self,
        tenant_id: &str,
        parent_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        items: &[crate::breakdown::BreakdownItem],
    ) -> Result<Vec<RequirementDto>, DevopsError> {
        self.require_requirement(tenant_id, parent_id).await?;
        let mut created = Vec::with_capacity(items.len());
        for item in items {
            let child = self
                .create_requirement(
                    tenant_id,
                    creator_id,
                    creator_name,
                    CreateRequirementInput {
                        parent_id: Some(parent_id.to_owned()),
                        kind: Some(item.kind.clone()),
                        subject: item.subject.clone(),
                        description: item.description.clone(),
                        priority: Some(item.priority.clone()),
                        milestone_id: None,
                        autopilot: None,
                    },
                )
                .await?;
            created.push(child);
        }
        Ok(created)
    }

    pub async fn update_requirement(
        &self,
        tenant_id: &str,
        id: &str,
        input: UpdateRequirementInput,
    ) -> Result<(), DevopsError> {
        let row = self.require_requirement(tenant_id, id).await?;

        if let Some(status) = input.status.as_deref() {
            validate_one_of(status, REQUIREMENT_STATUSES, "status")?;
        }
        if let Some(priority) = input.priority.as_deref() {
            validate_one_of(priority, REQUIREMENT_PRIORITIES, "priority")?;
        }
        if let Some(Some(parent_id)) = input.parent_id.as_ref() {
            if parent_id == id {
                return Err(DevopsError::BadRequest("a requirement cannot be its own parent".into()));
            }
            self.require_requirement(tenant_id, parent_id).await?;
        }
        let subject = match input.subject.as_deref().map(str::trim) {
            Some("") => return Err(DevopsError::BadRequest("subject cannot be empty".into())),
            Some(subject) => Some(subject.to_owned()),
            None => None,
        };

        sqlx::query(
            "UPDATE one_requirements SET \
                subject = COALESCE(?, subject), \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = COALESCE(?, status), \
                priority = COALESCE(?, priority), \
                assigned_to = CASE WHEN ? THEN ? ELSE assigned_to END, \
                parent_id = CASE WHEN ? THEN ? ELSE parent_id END, \
                milestone_id = CASE WHEN ? THEN ? ELSE milestone_id END, \
                autopilot = COALESCE(?, autopilot), \
                updated_at = ? \
             WHERE id = ?",
        )
        .bind(&subject)
        .bind(input.description.is_some())
        .bind(input.description.clone().flatten())
        .bind(&input.status)
        .bind(&input.priority)
        .bind(input.assigned_to.is_some())
        .bind(input.assigned_to.clone().flatten())
        .bind(input.parent_id.is_some())
        .bind(input.parent_id.clone().flatten())
        .bind(input.milestone_id.is_some())
        .bind(input.milestone_id.clone().flatten())
        .bind(input.autopilot)
        .bind(now_ms())
        .bind(&row.id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically claim a requirement for dispatch by transitioning it from a
    /// pre-dev status (`backlog`/`planning`) to `developing` in a single
    /// conditional UPDATE. Returns `true` iff THIS call won the claim
    /// (`rows_affected == 1`).
    ///
    /// This closes a TOCTOU race: `dispatch_core` and `maybe_autopilot` used to
    /// read the status, run the (quota-costing) digital-employee turn, and only
    /// then advance the status — so a concurrent manual dispatch + autopilot (or
    /// a double click) could both observe `backlog`, both fire a run, and burn
    /// quota twice. Claiming before the run guarantees exactly one winner.
    /// Requirements already in `developing` or a later status are not claimable
    /// here; the caller decides whether a deliberate re-dispatch is still allowed.
    pub async fn claim_requirement_for_dispatch(&self, tenant_id: &str, id: &str) -> Result<bool, DevopsError> {
        let res = sqlx::query(
            "UPDATE one_requirements SET status = 'developing', updated_at = ? \
             WHERE id = ? AND tenant_id = ? AND status IN ('backlog', 'planning')",
        )
        .bind(now_ms())
        .bind(id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Delete a requirement and its whole subtree (plus their comments).
    pub async fn delete_requirement(&self, tenant_id: &str, id: &str) -> Result<(), DevopsError> {
        self.require_requirement(tenant_id, id).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "WITH RECURSIVE subtree(id) AS (\
                 SELECT id FROM one_requirements WHERE id = ? \
                 UNION ALL \
                 SELECT r.id FROM one_requirements r JOIN subtree s ON r.parent_id = s.id\
             ) \
             DELETE FROM one_requirement_comments WHERE requirement_id IN (SELECT id FROM subtree)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "WITH RECURSIVE subtree(id) AS (\
                 SELECT id FROM one_requirements WHERE id = ? \
                 UNION ALL \
                 SELECT r.id FROM one_requirements r JOIN subtree s ON r.parent_id = s.id\
             ) \
             DELETE FROM one_requirements WHERE id IN (SELECT id FROM subtree)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_comments(
        &self,
        tenant_id: &str,
        requirement_id: &str,
    ) -> Result<Vec<RequirementCommentDto>, DevopsError> {
        self.require_requirement(tenant_id, requirement_id).await?;
        Ok(sqlx::query_as::<_, RequirementCommentDto>(
            "SELECT id, requirement_id, author_type, author_id, author_name, body, metadata, created_at \
             FROM one_requirement_comments WHERE requirement_id = ? AND tenant_id = ? ORDER BY created_at ASC",
        )
        .bind(requirement_id)
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_comment(
        &self,
        tenant_id: &str,
        requirement_id: &str,
        author_id: &str,
        author_name: &str,
        body: &str,
    ) -> Result<RequirementCommentDto, DevopsError> {
        self.require_requirement(tenant_id, requirement_id).await?;
        let body = body.trim();
        if body.is_empty() {
            return Err(DevopsError::BadRequest("comment body is required".into()));
        }
        let id = new_id("reqc");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_requirement_comments \
                (id, requirement_id, author_type, author_id, author_name, body, metadata, tenant_id, created_at) \
             VALUES (?, ?, 'user', ?, ?, ?, NULL, ?, ?)",
        )
        .bind(&id)
        .bind(requirement_id)
        .bind(author_id)
        .bind(author_name)
        .bind(body)
        .bind(tenant_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(RequirementCommentDto {
            id,
            requirement_id: requirement_id.to_owned(),
            author_type: "user".into(),
            author_id: Some(author_id.to_owned()),
            author_name: author_name.to_owned(),
            body: body.to_owned(),
            metadata: None,
            created_at: now,
        })
    }

    /// Public requirement fetch for orchestration (dispatch). Errors NotFound
    /// when the id is unknown.
    pub async fn get_requirement_row(&self, tenant_id: &str, id: &str) -> Result<RequirementRow, DevopsError> {
        self.fetch_requirement(tenant_id, id).await
    }

    /// Insert an agent/autopilot-authored comment carrying optional metadata
    /// JSON. Used by dispatch to record the run linkage on the requirement.
    // See create_test_case's comment for why this carries the allow — same
    // 012 tenant-scope fix, same "defer the struct refactor" call.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_comment(
        &self,
        tenant_id: &str,
        requirement_id: &str,
        author_type: &str,
        author_id: Option<&str>,
        author_name: &str,
        body: &str,
        metadata: Option<String>,
    ) -> Result<RequirementCommentDto, DevopsError> {
        self.require_requirement(tenant_id, requirement_id).await?;
        let id = new_id("reqc");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_requirement_comments \
                (id, requirement_id, author_type, author_id, author_name, body, metadata, tenant_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(requirement_id)
        .bind(author_type)
        .bind(author_id)
        .bind(author_name)
        .bind(body)
        .bind(metadata.as_deref())
        .bind(tenant_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(RequirementCommentDto {
            id,
            requirement_id: requirement_id.to_owned(),
            author_type: author_type.to_owned(),
            author_id: author_id.map(str::to_owned),
            author_name: author_name.to_owned(),
            body: body.to_owned(),
            metadata,
            created_at: now,
        })
    }

    async fn fetch_requirement(&self, tenant_id: &str, id: &str) -> Result<RequirementRow, DevopsError> {
        sqlx::query_as::<_, RequirementRow>(
            "SELECT id, parent_id, type, subject, description, status, priority, assigned_to, \
                    milestone_id, autopilot, creator_id, creator_name, created_at, updated_at \
             FROM one_requirements WHERE id = ? AND tenant_id = ?",
        )
        .bind(id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("requirement {id}")))
    }

    async fn require_requirement(&self, tenant_id: &str, id: &str) -> Result<RequirementRow, DevopsError> {
        self.fetch_requirement(tenant_id, id).await
    }

    /// Best-effort audit trail for policy-changing actions (registry writes,
    /// requirement dispatch/breakdown). Writes into one-org's `one_audit_logs`
    /// (shared pool). Silently skips when the table is absent (standalone /
    /// one-org not initialized) and never fails the originating request.
    pub async fn audit(&self, tenant_id: &str, user_id: &str, action: &str, resource: Option<&str>) {
        let result = sqlx::query(
            "INSERT INTO one_audit_logs (id, tenant_id, user_id, action, resource, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(new_id("audit"))
        .bind(tenant_id)
        .bind(user_id)
        .bind(action)
        .bind(resource)
        .bind(now_ms())
        .execute(&self.pool)
        .await;
        if let Err(e) = result {
            tracing::debug!(error = %e, action, "one-devops audit skipped (table absent or write failed)");
        }
    }

    /// Enterprise role of `user_id`, or `None` when the user has no org row
    /// (standalone / personal mode — the sole machine owner).
    ///
    /// Reads one-org's `one_user_org` table (same SQLite pool). Returns
    /// `Ok(None)` when the table itself does not exist, so a standalone
    /// deployment that never ran one-org migrations keeps working unchanged.
    ///
    /// Phase 2 multi-membership: role is scoped to the user's *active* tenant
    /// (active membership first, else most-recently-joined) — mirrors
    /// `OrgService::active_tenant_id`.
    pub async fn user_org_role(&self, user_id: &str) -> Result<Option<String>, DevopsError> {
        let result = sqlx::query_scalar::<_, String>(
            "SELECT uo.role FROM one_user_org uo WHERE uo.user_id = ? \
             ORDER BY (uo.tenant_id = (SELECT tenant_id FROM one_active_tenant WHERE user_id = uo.user_id)) DESC, \
                      uo.created_at DESC, uo.tenant_id ASC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await;
        match result {
            Ok(role) => Ok(role),
            // Table missing = one-org never initialized = standalone.
            Err(sqlx::Error::Database(e)) if e.message().contains("no such table") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// `user_id`'s currently active project group, or `None` for a standalone
    /// deployment (no `one_user_org` row) or one where one-org's migrations
    /// never ran. Same missing-table fallback as `user_org_role`.
    async fn active_tenant_id(&self, user_id: &str) -> Result<Option<String>, DevopsError> {
        let result = sqlx::query_scalar::<_, String>("SELECT tenant_id FROM one_active_tenant WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await;
        match result {
            Ok(tenant_id) => Ok(tenant_id),
            Err(sqlx::Error::Database(e)) if e.message().contains("no such table") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether `actor_user_id` may modify an *existing* registry row scoped to
    /// `team_id`. `require_registry_admin` (routes.rs) already confirmed the
    /// actor is an admin of *some* project group before this runs; what it
    /// cannot confirm is *which* one, since a user's admin role is resolved
    /// against their own active tenant regardless of which resource they are
    /// about to touch. Without this check, an admin of project group A could
    /// delete or overwrite project group B's distributed skill/MCP/RAG
    /// document just by knowing its id — team scope existed to keep resources
    /// inside their owning group, and the write path never enforced it.
    ///
    /// Org-scoped resources (`team_id = None`) stay reachable by any registry
    /// admin — that mirrors the read-side ACL (`member_visibility_where`),
    /// which shows `scope = 'org'` rows to every member regardless of team.
    /// A standalone/personal owner (no `one_user_org` row) is unrestricted,
    /// matching `viewer_is_privileged`'s "None role = machine owner" default.
    pub(crate) async fn actor_can_touch_team(
        &self,
        actor_user_id: &str,
        team_id: Option<&str>,
    ) -> Result<bool, DevopsError> {
        let Some(team_id) = team_id else {
            return Ok(true);
        };
        if self.user_org_role(actor_user_id).await?.is_none() {
            return Ok(true);
        }
        Ok(self.active_tenant_id(actor_user_id).await?.as_deref() == Some(team_id))
    }

    // -- ownership transfer (P1-2 offboarding) ----------------------------

    /// Gate for admin-only devops operations. Reuses `viewer_is_privileged`,
    /// so a standalone/personal owner (no `one_user_org` row) passes — they own
    /// the machine — while a plain enterprise member is refused.
    pub async fn ensure_privileged(&self, user_id: &str) -> Result<(), DevopsError> {
        if self.viewer_is_privileged(user_id).await? {
            return Ok(());
        }
        Err(DevopsError::Forbidden(
            "only an administrator can perform this operation".into(),
        ))
    }

    /// Tables whose owner column is `created_by`. These are the three shared
    /// registries and they carry the full P0-4 ACL triple
    /// (`scope`/`team_id`/`visibility`), so a transfer can — and must — be
    /// restricted to the tenant being offboarded from.
    const REGISTRY_OWNER_TABLES: [&'static str; 3] = ["one_skill_registry", "one_mcp_registry", "one_rag_documents"];

    /// Tables whose owner column is `creator_id`, plus a denormalized
    /// `creator_name` that has to move with it — otherwise the boards keep
    /// displaying the departed employee's name next to the new owner's id.
    ///
    /// These are deployment-global: they have **no** `scope`/`team_id` columns,
    /// so there is no tenant dimension to scope the transfer by (see
    /// `transfer_ownership`'s doc comment).
    const BOARD_OWNER_TABLES: [&'static str; 5] = [
        "one_requirements",
        "one_milestones",
        "one_test_plans",
        "one_test_cases",
        "one_pipelines",
    ];

    /// How many team resources `user_id` currently owns, so the UI can ask
    /// "this member owns N team resources — hand them to whom?" *before*
    /// removing them.
    pub async fn count_owned_resources(&self, user_id: &str, tenant_id: &str) -> Result<i64, DevopsError> {
        let mut total = 0i64;
        for table in Self::REGISTRY_OWNER_TABLES {
            let n: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE created_by = ? AND (scope = 'org' OR team_id = ?)"
            ))
            .bind(user_id)
            .bind(tenant_id)
            .fetch_one(&self.pool)
            .await?;
            total += n;
        }
        for table in Self::BOARD_OWNER_TABLES {
            let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE creator_id = ?"))
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
            total += n;
        }
        Ok(total)
    }

    /// Reassign every team resource owned by `from_user` to `to_user` (P1-2).
    ///
    /// Team assets must not walk out the door with a departing employee: the
    /// three registries plus the boards all record an owner, and once that
    /// owner is gone nobody can administer those rows.
    ///
    /// **Tenant safety.** `to_user` must be a member of `tenant_id`, so assets
    /// can never be handed to someone outside the project group. For the three
    /// registries the update is additionally filtered to rows that belong to
    /// this tenant (`scope = 'org' OR team_id = ?`), so an admin of group A
    /// cannot reassign group B's resources. The board tables have no tenant
    /// columns at all — they are global to the deployment — so there is no
    /// cross-tenant boundary to enforce there, and all of the user's rows move.
    ///
    /// Runs in a single transaction: a partial transfer would leave assets
    /// split between a departed user and their successor.
    pub async fn transfer_ownership(
        &self,
        from_user: &str,
        to_user: &str,
        tenant_id: &str,
    ) -> Result<i64, DevopsError> {
        if from_user == to_user {
            return Err(DevopsError::BadRequest(
                "source and target owner are the same user".into(),
            ));
        }

        // The recipient must be inside the tenant we are transferring within.
        // Missing table = standalone/personal edition, where there is no
        // membership model and thus nothing to enforce.
        let recipient_in_tenant =
            match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM one_user_org WHERE user_id = ? AND tenant_id = ?")
                .bind(to_user)
                .bind(tenant_id)
                .fetch_one(&self.pool)
                .await
            {
                Ok(n) => n > 0,
                Err(sqlx::Error::Database(e)) if e.message().contains("no such table") => true,
                Err(e) => return Err(e.into()),
            };
        if !recipient_in_tenant {
            return Err(DevopsError::BadRequest(format!(
                "target owner {to_user} is not a member of project group {tenant_id}"
            )));
        }

        // `updated_at` is deliberately left alone: an ownership handover is not
        // a content edit, and bumping it would reshuffle every list that sorts
        // by recency.
        let to_name = self.lookup_creator_name(to_user).await;
        let mut tx = self.pool.begin().await?;
        let mut moved = 0i64;

        for table in Self::REGISTRY_OWNER_TABLES {
            let res = sqlx::query(&format!(
                "UPDATE {table} SET created_by = ? WHERE created_by = ? AND (scope = 'org' OR team_id = ?)"
            ))
            .bind(to_user)
            .bind(from_user)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
            moved += res.rows_affected() as i64;
        }

        for table in Self::BOARD_OWNER_TABLES {
            // `creator_name` is denormalized for display; move it with the id
            // or the board shows the departed employee as the owner.
            let res = sqlx::query(&format!(
                "UPDATE {table} SET creator_id = ?, creator_name = ? WHERE creator_id = ?"
            ))
            .bind(to_user)
            .bind(to_name.as_deref())
            .bind(from_user)
            .execute(&mut *tx)
            .await?;
            moved += res.rows_affected() as i64;
        }

        tx.commit().await?;
        tracing::info!(
            from_user,
            to_user,
            tenant_id,
            moved,
            "transferred team resource ownership"
        );
        Ok(moved)
    }

    /// Display name for a user id, for the denormalized `creator_name` columns.
    /// A missing `users` table (standalone) or absent row just yields `None`.
    async fn lookup_creator_name(&self, user_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>("SELECT username FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .ok()
            .flatten()
            .flatten()
    }

    // -- registry read ACL (P0-4 fine-grained RBAC) -----------------------

    /// WHERE fragment restricting registry reads for a non-privileged member:
    /// org-wide resources, plus team resources for any project group the viewer
    /// belongs to (reuses P0-1 `one_user_org` multi-membership), and only
    /// `visibility='all'` (admin-only resources stay hidden). Binds
    /// `viewer_user_id` **once**. `prefix` is the column qualifier ("" for a
    /// single-table read, "d." for the `search_rag` join).
    pub(crate) fn member_visibility_where(prefix: &str) -> String {
        format!(
            "({p}scope = 'org' OR ({p}scope = 'team' AND {p}team_id IN \
               (SELECT tenant_id FROM one_user_org WHERE user_id = ?))) AND {p}visibility = 'all'",
            p = prefix
        )
    }

    /// True when the viewer sees every resource unfiltered: an org/system admin,
    /// or a standalone/personal-edition owner (no `one_user_org` row →
    /// `user_org_role` is `None`, the machine owner). Members are filtered.
    pub(crate) async fn viewer_is_privileged(&self, viewer_user_id: &str) -> Result<bool, DevopsError> {
        Ok(match self.user_org_role(viewer_user_id).await? {
            None => true,
            Some(role) => role == "org_admin" || role == "system_admin" || role == "admin",
        })
    }

    /// Validate a registry write's scope/visibility. When scope is `team` the
    /// `team_id` must be a real project group (`one_tenants`, one-org's table,
    /// read via the shared pool — same cross-crate precedent as
    /// `user_org_role`). Returns the normalized team_id (forced `None` for org
    /// scope so an org resource never carries a stray team binding).
    pub(crate) async fn validate_resource_scope<'a>(
        &self,
        created_by: &str,
        scope: &str,
        team_id: Option<&'a str>,
        visibility: &str,
    ) -> Result<Option<&'a str>, DevopsError> {
        use aionui_common::license::Feature;

        if !matches!(scope, "org" | "team") {
            return Err(DevopsError::BadRequest("scope must be 'org' or 'team'".into()));
        }
        if !matches!(visibility, "all" | "admin") {
            return Err(DevopsError::BadRequest("visibility must be 'all' or 'admin'".into()));
        }
        // P0-3 license gate: team-scoped distribution and admin-only visibility
        // are paid-tier features. Personal / no-enterprise authors pass (the
        // gate resolves to allowed). Enforced only for licensed companies.
        if visibility == "admin"
            && !self
                .enterprise_feature_allowed(created_by, Feature::AdminOnlyVisibility)
                .await?
        {
            return Err(DevopsError::Forbidden(
                "admin-only visibility requires an upgraded plan".into(),
            ));
        }
        if scope == "org" {
            return Ok(None);
        }
        if !self
            .enterprise_feature_allowed(created_by, Feature::TeamResourceScope)
            .await?
        {
            return Err(DevopsError::Forbidden(
                "team-scoped distribution requires an upgraded plan".into(),
            ));
        }
        let tid = team_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DevopsError::BadRequest("team scope requires a project group".into()))?;
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_tenants WHERE id = ?")
            .bind(tid)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::BadRequest(format!("project group '{tid}' not found")));
        }
        Ok(Some(tid))
    }

    /// Whether the author's company plan includes `feature`. Resolves the
    /// author's SSO company (`one_enterprise_members`) → tier
    /// (`one_enterprise_license`) → the `aionui-common` matrix. No enterprise,
    /// or billing not installed → allowed (the personal-edition red line).
    async fn enterprise_feature_allowed(
        &self,
        user_id: &str,
        feature: aionui_common::license::Feature,
    ) -> Result<bool, DevopsError> {
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
        let tier = tier
            .map(|t| aionui_common::license::Tier::parse(&t))
            .unwrap_or(aionui_common::license::Tier::Free);
        Ok(aionui_common::license::tier_allows(tier, feature))
    }

    // -- skill registry ---------------------------------------------------

    pub async fn list_skills(&self, viewer_user_id: &str) -> Result<Vec<SkillRegistryDto>, DevopsError> {
        const COLS: &str = "id, name, description, content, enabled, auto_active, scope, team_id, visibility, \
                            created_by, created_at, updated_at";
        let privileged = self.viewer_is_privileged(viewer_user_id).await?;
        let sql = if privileged {
            format!("SELECT {COLS} FROM one_skill_registry ORDER BY updated_at DESC")
        } else {
            format!(
                "SELECT {COLS} FROM one_skill_registry WHERE {} ORDER BY updated_at DESC",
                Self::member_visibility_where("")
            )
        };
        let mut q = sqlx::query_as::<_, SkillRegistryDto>(&sql);
        if !privileged {
            q = q.bind(viewer_user_id);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_skill(
        &self,
        id: Option<&str>,
        name: &str,
        description: &str,
        content: &str,
        enabled: bool,
        auto_active: bool,
        scope: &str,
        team_id: Option<&str>,
        visibility: &str,
        created_by: &str,
    ) -> Result<SkillRegistryDto, DevopsError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DevopsError::BadRequest("name is required".into()));
        }
        let team_id = self
            .validate_resource_scope(created_by, scope, team_id, visibility)
            .await?;
        // The INCOMING team_id, checked on every write (create and update
        // alike): without this, an actor who legitimately owns some resource
        // could re-scope it INTO a team they don't administer in the same
        // call the current-row check below guards against re-scoping OUT of
        // one they don't own.
        if !self.actor_can_touch_team(created_by, team_id).await? {
            return Err(DevopsError::Forbidden(
                "cannot assign this skill to a different project group".into(),
            ));
        }
        // D7: names must be unique. A duplicate team skill name would
        // materialize two SKILL.md dirs on every member and shadow each other
        // (and can mask a built-in skill) — last-write-wins is unsafe for a
        // distributed capability.
        let name_taken: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_skill_registry WHERE name = ? AND id != ?")
                .bind(name)
                .bind(id.unwrap_or(""))
                .fetch_one(&self.pool)
                .await?;
        if name_taken {
            return Err(DevopsError::BadRequest(format!(
                "a team skill named '{name}' already exists"
            )));
        }
        let now = now_ms();
        let id = match id {
            Some(existing) => {
                // The row's CURRENT scope/team_id, not the incoming one: an
                // actor editing must already own what the row belongs to today,
                // otherwise they could both overwrite another team's resource
                // and re-scope it away from that team in the same call.
                let current_team_id: Option<String> =
                    sqlx::query_scalar("SELECT team_id FROM one_skill_registry WHERE id = ?")
                        .bind(existing)
                        .fetch_optional(&self.pool)
                        .await?
                        .ok_or_else(|| DevopsError::NotFound(format!("skill {existing}")))?;
                if !self
                    .actor_can_touch_team(created_by, current_team_id.as_deref())
                    .await?
                {
                    return Err(DevopsError::Forbidden(
                        "this skill belongs to a different project group".into(),
                    ));
                }
                let updated = sqlx::query(
                    "UPDATE one_skill_registry SET name = ?, description = ?, content = ?, enabled = ?, auto_active = ?, \
                     scope = ?, team_id = ?, visibility = ?, updated_at = ? WHERE id = ?",
                )
                .bind(name)
                .bind(description)
                .bind(content)
                .bind(enabled)
                .bind(auto_active)
                .bind(scope)
                .bind(team_id)
                .bind(visibility)
                .bind(now)
                .bind(existing)
                .execute(&self.pool)
                .await?;
                if updated.rows_affected() == 0 {
                    return Err(DevopsError::NotFound(format!("skill {existing}")));
                }
                existing.to_owned()
            }
            None => {
                let id = new_id("oskill");
                sqlx::query(
                    "INSERT INTO one_skill_registry \
                        (id, name, description, content, enabled, auto_active, scope, team_id, visibility, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(name)
                .bind(description)
                .bind(content)
                .bind(enabled)
                .bind(auto_active)
                .bind(scope)
                .bind(team_id)
                .bind(visibility)
                .bind(created_by)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                id
            }
        };
        sqlx::query_as::<_, SkillRegistryDto>(
            "SELECT id, name, description, content, enabled, auto_active, scope, team_id, visibility, created_by, created_at, updated_at \
             FROM one_skill_registry WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn delete_skill(&self, actor_user_id: &str, id: &str) -> Result<(), DevopsError> {
        let team_id: Option<String> = sqlx::query_scalar("SELECT team_id FROM one_skill_registry WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DevopsError::NotFound(format!("skill {id}")))?;
        if !self.actor_can_touch_team(actor_user_id, team_id.as_deref()).await? {
            return Err(DevopsError::Forbidden(
                "this skill belongs to a different project group".into(),
            ));
        }
        let deleted = sqlx::query("DELETE FROM one_skill_registry WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("skill {id}")));
        }
        Ok(())
    }

    // -- mcp registry -----------------------------------------------------

    pub async fn list_mcp_registry(&self, viewer_user_id: &str) -> Result<Vec<McpRegistryDto>, DevopsError> {
        const COLS: &str = "id, name, type, endpoint, enabled, has_keys, secrets_json, scope, team_id, visibility, \
                            created_by, created_at, updated_at";
        let privileged = self.viewer_is_privileged(viewer_user_id).await?;
        let sql = if privileged {
            format!("SELECT {COLS} FROM one_mcp_registry ORDER BY updated_at DESC")
        } else {
            format!(
                "SELECT {COLS} FROM one_mcp_registry WHERE {} ORDER BY updated_at DESC",
                Self::member_visibility_where("")
            )
        };
        let mut q = sqlx::query_as::<_, McpRegistryDto>(&sql);
        if !privileged {
            q = q.bind(viewer_user_id);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_mcp_registry(
        &self,
        id: Option<&str>,
        name: &str,
        r#type: &str,
        endpoint: &str,
        enabled: bool,
        has_keys: bool,
        secrets_json: Option<&str>,
        scope: &str,
        team_id: Option<&str>,
        visibility: &str,
        created_by: &str,
    ) -> Result<McpRegistryDto, DevopsError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DevopsError::BadRequest("name is required".into()));
        }
        let team_id = self
            .validate_resource_scope(created_by, scope, team_id, visibility)
            .await?;
        // See the identical comment in `upsert_skill` — guards both creating
        // into, and re-scoping into, a team the actor doesn't administer.
        if !self.actor_can_touch_team(created_by, team_id).await? {
            return Err(DevopsError::Forbidden(
                "cannot assign this MCP server to a different project group".into(),
            ));
        }
        if !matches!(r#type, "stdio" | "sse") {
            return Err(DevopsError::BadRequest(format!(
                "invalid type: {type} (allowed: stdio/sse)",
                r#type = r#type
            )));
        }
        // D7: MCP connector names must be unique — the member's local MCP
        // config keys on name (upsert-by-name), so duplicates would clobber.
        let name_taken: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_mcp_registry WHERE name = ? AND id != ?")
                .bind(name)
                .bind(id.unwrap_or(""))
                .fetch_one(&self.pool)
                .await?;
        if name_taken {
            return Err(DevopsError::BadRequest(format!(
                "a team MCP named '{name}' already exists"
            )));
        }
        let now = now_ms();
        let id = match id {
            Some(existing) => {
                // Same reasoning as upsert_skill: check the row's CURRENT
                // team_id before applying whatever the request wants it to be.
                let current_team_id: Option<String> =
                    sqlx::query_scalar("SELECT team_id FROM one_mcp_registry WHERE id = ?")
                        .bind(existing)
                        .fetch_optional(&self.pool)
                        .await?
                        .ok_or_else(|| DevopsError::NotFound(format!("mcp registry entry {existing}")))?;
                if !self
                    .actor_can_touch_team(created_by, current_team_id.as_deref())
                    .await?
                {
                    return Err(DevopsError::Forbidden(
                        "this MCP server belongs to a different project group".into(),
                    ));
                }
                let updated = sqlx::query(
                    "UPDATE one_mcp_registry SET name = ?, type = ?, endpoint = ?, enabled = ?, has_keys = ?, secrets_json = ?, \
                     scope = ?, team_id = ?, visibility = ?, updated_at = ? WHERE id = ?",
                )
                .bind(name)
                .bind(r#type)
                .bind(endpoint)
                .bind(enabled)
                .bind(has_keys)
                .bind(secrets_json)
                .bind(scope)
                .bind(team_id)
                .bind(visibility)
                .bind(now)
                .bind(existing)
                .execute(&self.pool)
                .await?;
                if updated.rows_affected() == 0 {
                    return Err(DevopsError::NotFound(format!("mcp registry entry {existing}")));
                }
                existing.to_owned()
            }
            None => {
                let id = new_id("omcp");
                sqlx::query(
                    "INSERT INTO one_mcp_registry \
                        (id, name, type, endpoint, enabled, has_keys, scope, team_id, visibility, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(name)
                .bind(r#type)
                .bind(endpoint)
                .bind(enabled)
                .bind(has_keys)
                .bind(scope)
                .bind(team_id)
                .bind(visibility)
                .bind(created_by)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                id
            }
        };
        sqlx::query_as::<_, McpRegistryDto>(
            "SELECT id, name, type, endpoint, enabled, has_keys, secrets_json, scope, team_id, visibility, created_by, created_at, updated_at \
             FROM one_mcp_registry WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn delete_mcp_registry(&self, actor_user_id: &str, id: &str) -> Result<(), DevopsError> {
        let team_id: Option<String> = sqlx::query_scalar("SELECT team_id FROM one_mcp_registry WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DevopsError::NotFound(format!("mcp registry entry {id}")))?;
        if !self.actor_can_touch_team(actor_user_id, team_id.as_deref()).await? {
            return Err(DevopsError::Forbidden(
                "this MCP server belongs to a different project group".into(),
            ));
        }
        let deleted = sqlx::query("DELETE FROM one_mcp_registry WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("mcp registry entry {id}")));
        }
        Ok(())
    }

    // -- rag documents (metadata registry) ---------------------------------

    pub async fn list_rag_documents(&self, viewer_user_id: &str) -> Result<Vec<RagDocumentDto>, DevopsError> {
        const COLS: &str = "id, title, file_path, file_size, mime_type, status, last_error, chunk_count, \
                            scope, team_id, visibility, created_by, created_at";
        let privileged = self.viewer_is_privileged(viewer_user_id).await?;
        let sql = if privileged {
            format!("SELECT {COLS} FROM one_rag_documents ORDER BY created_at DESC")
        } else {
            format!(
                "SELECT {COLS} FROM one_rag_documents WHERE {} ORDER BY created_at DESC",
                Self::member_visibility_where("")
            )
        };
        let mut q = sqlx::query_as::<_, RagDocumentDto>(&sql);
        if !privileged {
            q = q.bind(viewer_user_id);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register_rag_document(
        &self,
        title: &str,
        file_path: Option<&str>,
        file_size: Option<i64>,
        mime_type: Option<&str>,
        scope: &str,
        team_id: Option<&str>,
        visibility: &str,
        created_by: &str,
    ) -> Result<RagDocumentDto, DevopsError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DevopsError::BadRequest("title is required".into()));
        }
        let team_id = self
            .validate_resource_scope(created_by, scope, team_id, visibility)
            .await?;
        // See the identical comment in `upsert_skill`. This function has no
        // update-existing mode, so this is the only guard registration needs.
        if !self.actor_can_touch_team(created_by, team_id).await? {
            return Err(DevopsError::Forbidden(
                "cannot register this document to a different project group".into(),
            ));
        }
        let id = new_id("orag");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_rag_documents \
                (id, title, file_path, file_size, mime_type, status, last_error, chunk_count, scope, team_id, visibility, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', NULL, 0, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(file_path)
        .bind(file_size)
        .bind(mime_type)
        .bind(scope)
        .bind(team_id)
        .bind(visibility)
        .bind(created_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query_as::<_, RagDocumentDto>(
            "SELECT id, title, file_path, file_size, mime_type, status, last_error, chunk_count, \
                    scope, team_id, visibility, created_by, created_at \
             FROM one_rag_documents WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn delete_rag_document(&self, actor_user_id: &str, id: &str) -> Result<(), DevopsError> {
        let team_id: Option<String> = sqlx::query_scalar("SELECT team_id FROM one_rag_documents WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DevopsError::NotFound(format!("rag document {id}")))?;
        if !self.actor_can_touch_team(actor_user_id, team_id.as_deref()).await? {
            return Err(DevopsError::Forbidden(
                "this document belongs to a different project group".into(),
            ));
        }
        // Drop the lexical rows FIRST: they are located through
        // `one_rag_chunks`, so once the chunks are gone there is no way left to
        // find them and the document's full text would sit in the FTS index
        // forever. Retrieval would not surface it (the join filters orphans),
        // but "deleted" has to mean the text is actually gone from disk.
        crate::retrieval::delete_document(&self.pool, id).await?;

        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_rag_documents WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("rag document {id}")));
        }
        sqlx::query("DELETE FROM one_rag_chunks WHERE document_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // -- milestones -------------------------------------------------------

    pub async fn list_milestones(&self, tenant_id: &str) -> Result<Vec<MilestoneDto>, DevopsError> {
        Ok(sqlx::query_as::<_, MilestoneDto>(
            "SELECT id, title, description, status, due_at, creator_id, creator_name, created_at, updated_at \
             FROM one_milestones WHERE tenant_id = ? ORDER BY \
                CASE status WHEN 'active' THEN 0 WHEN 'completed' THEN 1 ELSE 2 END, updated_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_milestone(
        &self,
        tenant_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        title: &str,
        description: Option<&str>,
        due_at: Option<i64>,
    ) -> Result<MilestoneDto, DevopsError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DevopsError::BadRequest("title is required".into()));
        }
        let id = new_id("mile");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_milestones \
                (id, title, description, status, due_at, creator_id, creator_name, tenant_id, created_at, updated_at) \
             VALUES (?, ?, ?, 'active', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(description)
        .bind(due_at)
        .bind(creator_id)
        .bind(creator_name)
        .bind(tenant_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_milestone(tenant_id, &id).await
    }

    pub async fn update_milestone(
        &self,
        tenant_id: &str,
        id: &str,
        title: Option<&str>,
        description: Option<Option<&str>>,
        status: Option<&str>,
        due_at: Option<Option<i64>>,
    ) -> Result<MilestoneDto, DevopsError> {
        if let Some(status) = status {
            validate_one_of(status, MILESTONE_STATUSES, "milestone status")?;
        }
        let now = now_ms();
        // CASE WHEN ? guards mirror update_requirement: absent field = keep,
        // present = overwrite (Option<Option<_>> distinguishes null-clear).
        let res = sqlx::query(
            "UPDATE one_milestones SET \
                title = CASE WHEN ? THEN ? ELSE title END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                due_at = CASE WHEN ? THEN ? ELSE due_at END, \
                updated_at = ? \
             WHERE id = ? AND tenant_id = ?",
        )
        .bind(title.is_some())
        .bind(title)
        .bind(description.is_some())
        .bind(description.flatten())
        .bind(status.is_some())
        .bind(status)
        .bind(due_at.is_some())
        .bind(due_at.flatten())
        .bind(now)
        .bind(id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("milestone {id}")));
        }
        self.fetch_milestone(tenant_id, id).await
    }

    pub async fn delete_milestone(&self, tenant_id: &str, id: &str) -> Result<(), DevopsError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_milestones WHERE id = ? AND tenant_id = ?")
            .bind(id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("milestone {id}")));
        }
        // Clear the soft link on requirements that pointed here.
        sqlx::query("UPDATE one_requirements SET milestone_id = NULL WHERE milestone_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn fetch_milestone(&self, tenant_id: &str, id: &str) -> Result<MilestoneDto, DevopsError> {
        sqlx::query_as::<_, MilestoneDto>(
            "SELECT id, title, description, status, due_at, creator_id, creator_name, created_at, updated_at \
             FROM one_milestones WHERE id = ? AND tenant_id = ?",
        )
        .bind(id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("milestone {id}")))
    }

    // -- RAG pipeline (A2) ------------------------------------------------

    pub async fn get_rag_config(&self) -> Result<RagConfigDto, DevopsError> {
        let row: Option<(String, String, String, Option<i64>, i64)> = sqlx::query_as(
            "SELECT base_url, api_key, model, dimensions, updated_at FROM one_rag_config WHERE id = 'default'",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((base_url, api_key, model, dimensions, updated_at)) => RagConfigDto {
                base_url,
                model,
                has_key: !api_key.trim().is_empty(),
                dimensions,
                updated_at,
            },
            None => RagConfigDto {
                base_url: String::new(),
                model: String::new(),
                has_key: false,
                dimensions: None,
                updated_at: 0,
            },
        })
    }

    /// Upsert the embedding config. `api_key = None` keeps the stored key
    /// (so the UI can save base_url/model without re-entering the secret).
    pub async fn set_rag_config(
        &self,
        base_url: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Result<RagConfigDto, DevopsError> {
        let now = now_ms();
        // Preserve the existing key when the caller omits it.
        let key = match api_key {
            Some(k) => k.to_owned(),
            None => sqlx::query_scalar::<_, String>("SELECT api_key FROM one_rag_config WHERE id = 'default'")
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or_default(),
        };
        sqlx::query(
            "INSERT INTO one_rag_config (id, base_url, api_key, model, updated_at) \
             VALUES ('default', ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET base_url = excluded.base_url, api_key = excluded.api_key, \
                model = excluded.model, updated_at = excluded.updated_at",
        )
        .bind(base_url.trim())
        .bind(&key)
        .bind(model.trim())
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_rag_config().await
    }

    async fn load_embedding_config(&self) -> Result<EmbeddingConfig, DevopsError> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT base_url, api_key, model FROM one_rag_config WHERE id = 'default'")
                .fetch_optional(&self.pool)
                .await?;
        let (base_url, api_key, model) =
            row.ok_or_else(|| DevopsError::BadRequest("RAG embedding endpoint not configured".into()))?;
        Ok(EmbeddingConfig {
            base_url,
            api_key,
            model,
        })
    }

    /// Set a document's inline content (the text to embed on process).
    pub async fn set_document_content(&self, actor_user_id: &str, id: &str, content: &str) -> Result<(), DevopsError> {
        let team_id: Option<String> = sqlx::query_scalar("SELECT team_id FROM one_rag_documents WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DevopsError::NotFound(format!("rag document {id}")))?;
        if !self.actor_can_touch_team(actor_user_id, team_id.as_deref()).await? {
            return Err(DevopsError::Forbidden(
                "this document belongs to a different project group".into(),
            ));
        }
        let updated = sqlx::query("UPDATE one_rag_documents SET content = ? WHERE id = ?")
            .bind(content)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("rag document {id}")));
        }
        Ok(())
    }

    /// Process a document: chunk its content, embed each chunk, replace its
    /// chunk rows, and update status/chunk_count. Records the dimension on
    /// first success. Returns the chunk count.
    pub async fn process_rag_document(&self, actor_user_id: &str, id: &str) -> Result<i64, DevopsError> {
        let row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT content, team_id FROM one_rag_documents WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let (content, team_id) = row.ok_or_else(|| DevopsError::NotFound(format!("rag document {id}")))?;
        if !self.actor_can_touch_team(actor_user_id, team_id.as_deref()).await? {
            return Err(DevopsError::Forbidden(
                "this document belongs to a different project group".into(),
            ));
        }
        let content = content.unwrap_or_default();
        let chunks = crate::embedding::chunk_text(&content, 800, 100);
        if chunks.is_empty() {
            return Err(DevopsError::BadRequest("document has no content to process".into()));
        }

        let config = self.load_embedding_config().await?;
        let vectors = match crate::embedding::embed(&config, &chunks).await {
            Ok(v) => v,
            Err(e) => {
                let _ = sqlx::query("UPDATE one_rag_documents SET status = 'error', last_error = ? WHERE id = ?")
                    .bind(e.to_string())
                    .bind(id)
                    .execute(&self.pool)
                    .await;
                return Err(e);
            }
        };
        let dims = vectors.first().map(|v| v.len() as i64);

        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM one_rag_chunks WHERE document_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let mut lexical_rows: Vec<(String, String)> = Vec::with_capacity(chunks.len());
        for (idx, (chunk, vector)) in chunks.iter().zip(vectors.iter()).enumerate() {
            let chunk_id = new_id("ragc");
            sqlx::query(
                "INSERT INTO one_rag_chunks (id, document_id, chunk_index, content, embedding, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&chunk_id)
            .bind(id)
            .bind(idx as i64)
            .bind(chunk)
            .bind(crate::embedding::pack_embedding(vector))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            lexical_rows.push((chunk_id, chunk.clone()));
        }
        let count = chunks.len() as i64;
        sqlx::query("UPDATE one_rag_documents SET status = 'ready', last_error = NULL, chunk_count = ? WHERE id = ?")
            .bind(count)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        // The lexical index is derived, so it is refreshed only after the
        // chunk rows commit. A failure here leaves the index stale, not the
        // data wrong, and is recoverable by re-processing — hence a warning
        // rather than failing the upload the user just waited on.
        if let Err(e) = crate::retrieval::sync_document(&self.pool, id, &lexical_rows).await {
            tracing::warn!(error = %e, document_id = id, "lexical index update failed; keyword search may be stale");
        }

        if let Some(dims) = dims {
            let _ = sqlx::query("UPDATE one_rag_config SET dimensions = ? WHERE id = 'default'")
                .bind(dims)
                .execute(&self.pool)
                .await;
        }
        Ok(count)
    }

    /// Rebuild the lexical (BM25) index from the SQLite chunk table.
    ///
    /// Runs at startup for installs whose knowledge base predates hybrid
    /// retrieval. Reads only text already in SQLite — no embedding calls, so it
    /// costs nothing and works even with the embedding endpoint unreachable.
    /// Self-skips once the index is populated, so it is safe on every boot.
    pub async fn rebuild_lexical_index(&self) -> Result<usize, DevopsError> {
        crate::retrieval::rebuild_index(&self.pool).await
    }

    /// Retrieve the top-k knowledge-base chunks visible to `viewer_user_id`.
    ///
    /// Hybrid: a dense-vector ranking (cosine over the stored embeddings) is
    /// fused with a BM25 ranking from FTS5 using Reciprocal Rank Fusion. Dense
    /// retrieval alone reliably misses rare literal tokens — error codes,
    /// ticket ids, product names — which is most of what people actually type
    /// into a company knowledge base.
    ///
    /// Both rankers apply the viewer's visibility predicate in SQL before
    /// ranking, so an invisible document can never take a top-k slot.
    pub async fn search_rag(
        &self,
        viewer_user_id: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RagSearchHit>, DevopsError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(DevopsError::BadRequest("query is required".into()));
        }
        let limit = top_k.max(1);
        let privileged = self.viewer_is_privileged(viewer_user_id).await?;
        let acl_predicate = if privileged {
            None
        } else {
            Some(Self::member_visibility_where("d."))
        };

        // Dense half. The ACL lives in the join, exactly as before.
        let config = self.load_embedding_config().await?;
        let query_vec = crate::embedding::embed(&config, &[query.to_owned()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| DevopsError::Internal("empty query embedding".into()))?;

        const BASE: &str = "SELECT c.id, c.document_id, c.chunk_index, c.content, c.embedding, d.title                             FROM one_rag_chunks c JOIN one_rag_documents d ON d.id = c.document_id";
        let sql = match acl_predicate.as_deref() {
            None => BASE.to_string(),
            Some(predicate) => format!("{BASE} WHERE {predicate}"),
        };
        let mut q = sqlx::query_as::<_, (String, String, i64, String, Vec<u8>, String)>(&sql);
        if acl_predicate.is_some() {
            q = q.bind(viewer_user_id);
        }
        let rows: Vec<(String, String, i64, String, Vec<u8>, String)> = q.fetch_all(&self.pool).await?;

        let mut by_id: HashMap<String, (RagSearchHit, f32)> = HashMap::with_capacity(rows.len());
        let mut dense: Vec<(String, f32)> = Vec::with_capacity(rows.len());
        for (chunk_id, document_id, chunk_index, content, blob, document_title) in rows {
            let cosine = crate::embedding::cosine_similarity(&query_vec, &crate::embedding::unpack_embedding(&blob));
            dense.push((chunk_id.clone(), cosine));
            by_id.insert(
                chunk_id,
                (
                    RagSearchHit {
                        document_id,
                        document_title,
                        chunk_index,
                        content,
                        score: cosine,
                    },
                    cosine,
                ),
            );
        }
        dense.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let candidates = crate::retrieval::candidate_limit(limit);
        let dense_ranked: Vec<String> = dense.into_iter().take(candidates).map(|(id, _)| id).collect();

        // Lexical half. Best-effort: with FTS5 absent or the query unparseable
        // this returns empty and the result degrades to dense-only.
        let lexical_ranked: Vec<String> = crate::retrieval::lexical_candidates(
            &self.pool,
            query,
            acl_predicate.as_deref(),
            viewer_user_id,
            candidates,
        )
        .await?
        .into_iter()
        .map(|hit| hit.chunk_id)
        .collect();

        if lexical_ranked.is_empty() {
            // Nothing to fuse — keep the dense scores, which callers already
            // threshold on (e.g. the task-dispatch injection uses >= 0.35).
            let mut hits: Vec<RagSearchHit> = dense_ranked
                .into_iter()
                .filter_map(|id| by_id.remove(&id).map(|(hit, _)| hit))
                .collect();
            hits.truncate(limit);
            return Ok(hits);
        }

        let mut hits = Vec::with_capacity(limit);
        for (chunk_id, _fused_score) in crate::retrieval::rrf_fuse(&dense_ranked, &lexical_ranked) {
            let Some((hit, cosine)) = by_id.remove(&chunk_id) else {
                // A lexical hit whose chunk the dense query did not return —
                // only possible if the two queries raced a concurrent write.
                continue;
            };
            // Report the cosine, not the RRF score: RRF values are tiny
            // rank-derived numbers with no absolute meaning, and callers
            // threshold `score` as a similarity.
            hits.push(RagSearchHit { score: cosine, ..hit });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    // -- test plans (A4) --------------------------------------------------

    pub async fn list_test_plans(&self, tenant_id: &str) -> Result<Vec<TestPlanDto>, DevopsError> {
        Ok(sqlx::query_as::<_, TestPlanDto>(
            "SELECT id, title, description, status, requirement_id, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_plans WHERE tenant_id = ? ORDER BY \
                CASE status WHEN 'active' THEN 0 WHEN 'draft' THEN 1 WHEN 'completed' THEN 2 ELSE 3 END, \
                updated_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_test_plan(
        &self,
        tenant_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        title: &str,
        description: Option<&str>,
        requirement_id: Option<&str>,
    ) -> Result<TestPlanDto, DevopsError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DevopsError::BadRequest("title is required".into()));
        }
        let id = new_id("tplan");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_test_plans \
                (id, title, description, status, requirement_id, creator_id, creator_name, tenant_id, created_at, updated_at) \
             VALUES (?, ?, ?, 'draft', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(description)
        .bind(requirement_id)
        .bind(creator_id)
        .bind(creator_name)
        .bind(tenant_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_test_plan(tenant_id, &id).await
    }

    pub async fn update_test_plan(
        &self,
        tenant_id: &str,
        id: &str,
        title: Option<&str>,
        description: Option<Option<&str>>,
        status: Option<&str>,
        requirement_id: Option<Option<&str>>,
    ) -> Result<TestPlanDto, DevopsError> {
        if let Some(status) = status {
            validate_one_of(status, TEST_PLAN_STATUSES, "test plan status")?;
        }
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE one_test_plans SET \
                title = CASE WHEN ? THEN ? ELSE title END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                requirement_id = CASE WHEN ? THEN ? ELSE requirement_id END, \
                updated_at = ? \
             WHERE id = ? AND tenant_id = ?",
        )
        .bind(title.is_some())
        .bind(title)
        .bind(description.is_some())
        .bind(description.flatten())
        .bind(status.is_some())
        .bind(status)
        .bind(requirement_id.is_some())
        .bind(requirement_id.flatten())
        .bind(now)
        .bind(id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("test plan {id}")));
        }
        self.fetch_test_plan(tenant_id, id).await
    }

    pub async fn delete_test_plan(&self, tenant_id: &str, id: &str) -> Result<(), DevopsError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_test_plans WHERE id = ? AND tenant_id = ?")
            .bind(id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("test plan {id}")));
        }
        sqlx::query("DELETE FROM one_test_cases WHERE plan_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn fetch_test_plan(&self, tenant_id: &str, id: &str) -> Result<TestPlanDto, DevopsError> {
        sqlx::query_as::<_, TestPlanDto>(
            "SELECT id, title, description, status, requirement_id, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_plans WHERE id = ? AND tenant_id = ?",
        )
        .bind(id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("test plan {id}")))
    }

    // -- test cases ---------------------------------------------------------

    pub async fn list_test_cases(&self, tenant_id: &str, plan_id: &str) -> Result<Vec<TestCaseDto>, DevopsError> {
        // Verify the plan exists AND belongs to the caller's tenant first.
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_test_plans WHERE id = ? AND tenant_id = ?")
            .bind(plan_id)
            .bind(tenant_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("test plan {plan_id}")));
        }
        Ok(sqlx::query_as::<_, TestCaseDto>(
            "SELECT id, plan_id, title, description, steps, expected, status, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_cases WHERE plan_id = ? AND tenant_id = ? ORDER BY created_at ASC",
        )
        .bind(plan_id)
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?)
    }

    // The 012 tenant-scope fix (see migration header) added `tenant_id` to
    // every one of this family's methods, pushing this one past clippy's
    // default arg-count threshold. Not restructured into an input struct
    // like `CreateRequirementInput` because that would also touch its one
    // route-handler call site for no behavior change — deferred, not skipped.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_test_case(
        &self,
        tenant_id: &str,
        plan_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        title: &str,
        description: Option<&str>,
        steps: Option<&str>,
        expected: Option<&str>,
    ) -> Result<TestCaseDto, DevopsError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DevopsError::BadRequest("title is required".into()));
        }
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_test_plans WHERE id = ? AND tenant_id = ?")
            .bind(plan_id)
            .bind(tenant_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("test plan {plan_id}")));
        }
        let id = new_id("tcase");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_test_cases \
                (id, plan_id, title, description, steps, expected, status, creator_id, creator_name, tenant_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(plan_id)
        .bind(title)
        .bind(description)
        .bind(steps)
        .bind(expected)
        .bind(creator_id)
        .bind(creator_name)
        .bind(tenant_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_test_case(tenant_id, &id).await
    }

    // See create_test_case above for why this carries the same allow.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_test_case(
        &self,
        tenant_id: &str,
        id: &str,
        title: Option<&str>,
        status: Option<&str>,
        description: Option<Option<&str>>,
        steps: Option<Option<&str>>,
        expected: Option<Option<&str>>,
    ) -> Result<TestCaseDto, DevopsError> {
        if let Some(status) = status {
            validate_one_of(status, TEST_CASE_STATUSES, "test case status")?;
        }
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE one_test_cases SET \
                title = CASE WHEN ? THEN ? ELSE title END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                steps = CASE WHEN ? THEN ? ELSE steps END, \
                expected = CASE WHEN ? THEN ? ELSE expected END, \
                updated_at = ? \
             WHERE id = ? AND tenant_id = ?",
        )
        .bind(title.is_some())
        .bind(title)
        .bind(status.is_some())
        .bind(status)
        .bind(description.is_some())
        .bind(description.flatten())
        .bind(steps.is_some())
        .bind(steps.flatten())
        .bind(expected.is_some())
        .bind(expected.flatten())
        .bind(now)
        .bind(id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("test case {id}")));
        }
        self.fetch_test_case(tenant_id, id).await
    }

    pub async fn delete_test_case(&self, tenant_id: &str, id: &str) -> Result<(), DevopsError> {
        let deleted = sqlx::query("DELETE FROM one_test_cases WHERE id = ? AND tenant_id = ?")
            .bind(id)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("test case {id}")));
        }
        Ok(())
    }

    async fn fetch_test_case(&self, tenant_id: &str, id: &str) -> Result<TestCaseDto, DevopsError> {
        sqlx::query_as::<_, TestCaseDto>(
            "SELECT id, plan_id, title, description, steps, expected, status, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_cases WHERE id = ? AND tenant_id = ?",
        )
        .bind(id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("test case {id}")))
    }

    // -- pipelines (A4) ---------------------------------------------------

    pub async fn list_pipelines(&self, tenant_id: &str) -> Result<Vec<PipelineDto>, DevopsError> {
        Ok(sqlx::query_as::<_, PipelineDto>(
            "SELECT id, name, description, status, trigger, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_pipelines WHERE tenant_id = ? ORDER BY \
                CASE status WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_pipeline(
        &self,
        tenant_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        name: &str,
        description: Option<&str>,
        trigger: Option<&str>,
    ) -> Result<PipelineDto, DevopsError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DevopsError::BadRequest("name is required".into()));
        }
        let trigger = trigger.unwrap_or("manual");
        validate_one_of(trigger, PIPELINE_TRIGGERS, "pipeline trigger")?;
        let id = new_id("pipe");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_pipelines \
                (id, name, description, status, trigger, creator_id, creator_name, tenant_id, created_at, updated_at) \
             VALUES (?, ?, ?, 'active', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(description)
        .bind(trigger)
        .bind(creator_id)
        .bind(creator_name)
        .bind(tenant_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_pipeline(tenant_id, &id).await
    }

    pub async fn update_pipeline(
        &self,
        tenant_id: &str,
        id: &str,
        name: Option<&str>,
        description: Option<Option<&str>>,
        status: Option<&str>,
        trigger: Option<&str>,
    ) -> Result<PipelineDto, DevopsError> {
        if let Some(status) = status {
            validate_one_of(status, PIPELINE_STATUSES, "pipeline status")?;
        }
        if let Some(trigger) = trigger {
            validate_one_of(trigger, PIPELINE_TRIGGERS, "pipeline trigger")?;
        }
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE one_pipelines SET \
                name = CASE WHEN ? THEN ? ELSE name END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                trigger = CASE WHEN ? THEN ? ELSE trigger END, \
                updated_at = ? \
             WHERE id = ? AND tenant_id = ?",
        )
        .bind(name.is_some())
        .bind(name)
        .bind(description.is_some())
        .bind(description.flatten())
        .bind(status.is_some())
        .bind(status)
        .bind(trigger.is_some())
        .bind(trigger)
        .bind(now)
        .bind(id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("pipeline {id}")));
        }
        self.fetch_pipeline(tenant_id, id).await
    }

    pub async fn delete_pipeline(&self, tenant_id: &str, id: &str) -> Result<(), DevopsError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_pipelines WHERE id = ? AND tenant_id = ?")
            .bind(id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("pipeline {id}")));
        }
        sqlx::query("DELETE FROM one_pipeline_runs WHERE pipeline_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn fetch_pipeline(&self, tenant_id: &str, id: &str) -> Result<PipelineDto, DevopsError> {
        sqlx::query_as::<_, PipelineDto>(
            "SELECT id, name, description, status, trigger, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_pipelines WHERE id = ? AND tenant_id = ?",
        )
        .bind(id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("pipeline {id}")))
    }

    // -- pipeline runs ------------------------------------------------------

    pub async fn list_pipeline_runs(
        &self,
        tenant_id: &str,
        pipeline_id: &str,
    ) -> Result<Vec<PipelineRunDto>, DevopsError> {
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_pipelines WHERE id = ? AND tenant_id = ?")
            .bind(pipeline_id)
            .bind(tenant_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("pipeline {pipeline_id}")));
        }
        Ok(sqlx::query_as::<_, PipelineRunDto>(
            "SELECT id, pipeline_id, status, triggered_by, started_at, finished_at, log, \
                    created_at, updated_at \
             FROM one_pipeline_runs WHERE pipeline_id = ? AND tenant_id = ? ORDER BY created_at DESC LIMIT 100",
        )
        .bind(pipeline_id)
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_pipeline_run(
        &self,
        tenant_id: &str,
        pipeline_id: &str,
        triggered_by: Option<&str>,
    ) -> Result<PipelineRunDto, DevopsError> {
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_pipelines WHERE id = ? AND tenant_id = ?")
            .bind(pipeline_id)
            .bind(tenant_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("pipeline {pipeline_id}")));
        }
        let id = new_id("run");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_pipeline_runs \
                (id, pipeline_id, status, triggered_by, started_at, finished_at, log, tenant_id, created_at, updated_at) \
             VALUES (?, ?, 'pending', ?, NULL, NULL, NULL, ?, ?, ?)",
        )
        .bind(&id)
        .bind(pipeline_id)
        .bind(triggered_by)
        .bind(tenant_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_pipeline_run(tenant_id, &id).await
    }

    pub async fn update_pipeline_run(
        &self,
        tenant_id: &str,
        id: &str,
        status: Option<&str>,
        started_at: Option<Option<i64>>,
        finished_at: Option<Option<i64>>,
        log: Option<Option<&str>>,
    ) -> Result<PipelineRunDto, DevopsError> {
        if let Some(status) = status {
            validate_one_of(status, PIPELINE_RUN_STATUSES, "pipeline run status")?;
        }
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE one_pipeline_runs SET \
                status = CASE WHEN ? THEN ? ELSE status END, \
                started_at = CASE WHEN ? THEN ? ELSE started_at END, \
                finished_at = CASE WHEN ? THEN ? ELSE finished_at END, \
                log = CASE WHEN ? THEN ? ELSE log END, \
                updated_at = ? \
             WHERE id = ? AND tenant_id = ?",
        )
        .bind(status.is_some())
        .bind(status)
        .bind(started_at.is_some())
        .bind(started_at.flatten())
        .bind(finished_at.is_some())
        .bind(finished_at.flatten())
        .bind(log.is_some())
        .bind(log.flatten())
        .bind(now)
        .bind(id)
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("pipeline run {id}")));
        }
        self.fetch_pipeline_run(tenant_id, id).await
    }

    async fn fetch_pipeline_run(&self, tenant_id: &str, id: &str) -> Result<PipelineRunDto, DevopsError> {
        sqlx::query_as::<_, PipelineRunDto>(
            "SELECT id, pipeline_id, status, triggered_by, started_at, finished_at, log, \
                    created_at, updated_at \
             FROM one_pipeline_runs WHERE id = ? AND tenant_id = ?",
        )
        .bind(id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("pipeline run {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dlp_service::UpsertDlpRule;
    use crate::migrate::run_one_devops_migrations;

    async fn service() -> DevopsService {
        // max_connections(1): every new pool connection to `sqlite::memory:`
        // opens its own empty database, so a second pooled connection would
        // intermittently see "no such table".
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        run_one_devops_migrations(&pool).await.unwrap();
        DevopsService::new(pool)
    }

    #[tokio::test]
    async fn audit_writes_when_table_present_and_skips_when_absent() {
        let svc = service().await;

        // Standalone: one_audit_logs table absent → silent no-op, no panic.
        svc.audit("default", "u1", "devops.skill.upsert", Some("s1")).await;

        // Enterprise: table present → the action is recorded.
        sqlx::raw_sql(
            "CREATE TABLE one_audit_logs (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, user_id TEXT, username TEXT, action TEXT NOT NULL, resource TEXT, ip_address TEXT, user_agent TEXT, created_at INTEGER NOT NULL);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
        svc.audit("t1", "admin1", "devops.skill.delete", Some("s2")).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_audit_logs WHERE action = 'devops.skill.delete'")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn registry_names_must_be_unique() {
        let svc = service().await;
        svc.upsert_skill(None, "review", "d", "c", true, false, "org", None, "all", "u1")
            .await
            .unwrap();
        // Same name, different (new) record → rejected.
        let err = svc
            .upsert_skill(None, "review", "d2", "c2", true, false, "org", None, "all", "u1")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "BAD_REQUEST");
        // Updating the existing record keeps its own name → allowed.
        let first = svc.list_skills("u1").await.unwrap().pop().unwrap();
        svc.upsert_skill(
            Some(&first.id),
            "review",
            "d3",
            "c3",
            false,
            true,
            "org",
            None,
            "all",
            "u1",
        )
        .await
        .unwrap();

        svc.upsert_mcp_registry(
            None,
            "search",
            "sse",
            "https://a/sse",
            true,
            false,
            None,
            "org",
            None,
            "all",
            "u1",
        )
        .await
        .unwrap();
        let err = svc
            .upsert_mcp_registry(
                None,
                "search",
                "sse",
                "https://b/sse",
                true,
                false,
                None,
                "org",
                None,
                "all",
                "u1",
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "BAD_REQUEST");
    }

    #[tokio::test]
    async fn claim_for_dispatch_is_won_once_then_blocks_and_ignores_non_predev() {
        let svc = service().await;
        let req = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "派活抢占".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // First claim on a fresh (backlog) requirement wins and advances status.
        assert!(svc.claim_requirement_for_dispatch("t1", &req.id).await.unwrap());
        assert_eq!(
            svc.get_requirement_row("t1", &req.id).await.unwrap().status,
            "developing"
        );

        // Second claim loses — the requirement is no longer in a pre-dev status,
        // so a concurrent dispatch/autopilot can't fire a duplicate run.
        assert!(!svc.claim_requirement_for_dispatch("t1", &req.id).await.unwrap());

        // A requirement already past pre-dev is likewise not claimable here.
        let planning = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "P".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.update_requirement(
            "t1",
            &planning.id,
            UpdateRequirementInput {
                status: Some("planning".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(svc.claim_requirement_for_dispatch("t1", &planning.id).await.unwrap());
        assert!(!svc.claim_requirement_for_dispatch("t1", &planning.id).await.unwrap());
    }

    #[tokio::test]
    async fn requirement_crud_and_tree_nesting() {
        let svc = service().await;
        let epic = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    kind: Some("epic".into()),
                    subject: "Big epic".into(),
                    priority: Some("high".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let story = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    parent_id: Some(epic.id.clone()),
                    kind: Some("story".into()),
                    subject: "Child story".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let tree = svc.requirements_tree("t1").await.unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].id, epic.id);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].id, story.id);

        svc.update_requirement(
            "t1",
            &story.id,
            UpdateRequirementInput {
                status: Some("developing".into()),
                assigned_to: Some(Some("agent-1".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tree = svc.requirements_tree("t1").await.unwrap();
        assert_eq!(tree[0].children[0].status, "developing");
        assert_eq!(tree[0].children[0].assigned_to.as_deref(), Some("agent-1"));

        // Deleting the epic removes the subtree.
        svc.delete_requirement("t1", &epic.id).await.unwrap();
        assert!(svc.requirements_tree("t1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn autopilot_flag_persists_and_toggles() {
        let svc = service().await;
        let req = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "auto".into(),
                    autopilot: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(req.autopilot);
        // Default is off.
        let plain = svc
            .create_requirement(
                "t1",
                "u1",
                None,
                CreateRequirementInput {
                    subject: "manual".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!plain.autopilot);

        // Toggling other fields leaves autopilot untouched; explicit toggle flips it.
        svc.update_requirement(
            "t1",
            &req.id,
            UpdateRequirementInput {
                priority: Some("high".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        svc.update_requirement(
            "t1",
            &req.id,
            UpdateRequirementInput {
                autopilot: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tree = svc.requirements_tree("t1").await.unwrap();
        let refreshed = tree.iter().find(|r| r.id == req.id).unwrap();
        assert!(!refreshed.autopilot);
        assert_eq!(refreshed.priority, "high");
    }

    #[tokio::test]
    async fn requirement_validation_rejects_bad_values() {
        let svc = service().await;
        let err = svc
            .create_requirement(
                "t1",
                "u1",
                None,
                CreateRequirementInput {
                    subject: "  ".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        let req = svc
            .create_requirement(
                "t1",
                "u1",
                None,
                CreateRequirementInput {
                    subject: "ok".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let err = svc
            .update_requirement(
                "t1",
                &req.id,
                UpdateRequirementInput {
                    status: Some("nonsense".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
        let err = svc
            .update_requirement(
                "t1",
                &req.id,
                UpdateRequirementInput {
                    parent_id: Some(Some(req.id.clone())),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
    }

    #[tokio::test]
    async fn comments_roundtrip() {
        let svc = service().await;
        let req = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "with comments".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.create_comment("t1", &req.id, "u1", "Alice", "first!")
            .await
            .unwrap();
        let comments = svc.list_comments("t1", &req.id).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "first!");
        assert_eq!(comments[0].author_type, "user");

        let err = svc
            .create_comment("t1", &req.id, "u1", "Alice", "  ")
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
        let err = svc.list_comments("t1", "missing").await.unwrap_err();
        assert!(matches!(err, DevopsError::NotFound(_)));
    }

    #[tokio::test]
    async fn user_org_role_standalone_and_enterprise() {
        let svc = service().await;

        // Standalone: one_user_org table never created (one-org not initialized)
        // -> None, and registry writes stay owner-open.
        assert_eq!(svc.user_org_role("u1").await.unwrap(), None);

        // Enterprise: role rows resolve, distinguishing member from admin.
        // Phase 2: `user_org_role` scopes to the active tenant, so the
        // cross-crate `one_active_tenant` table must exist too (empty is fine).
        sqlx::raw_sql(
            "CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id));
             CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0);
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('member1', 't1', 'member');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('admin1', 't1', 'org_admin');",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
        assert_eq!(svc.user_org_role("member1").await.unwrap().as_deref(), Some("member"));
        assert_eq!(svc.user_org_role("admin1").await.unwrap().as_deref(), Some("org_admin"));
        assert_eq!(svc.user_org_role("stranger").await.unwrap(), None);
    }

    /// Seed a two-group enterprise: memberA∈Group A, memberB∈Group B, admin1 is
    /// org_admin. Shared by the P0-4 read-ACL tests.
    async fn seed_two_group_enterprise(svc: &DevopsService) {
        sqlx::raw_sql(
            "CREATE TABLE one_tenants (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id));
             CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0);
             INSERT INTO one_tenants (id, name) VALUES ('tA', 'Group A'), ('tB', 'Group B');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('memberA', 'tA', 'member'), ('memberB', 'tB', 'member'), ('admin1', 'tA', 'org_admin'), ('admin2', 'tB', 'org_admin');
             INSERT INTO one_active_tenant (user_id, tenant_id) VALUES ('memberA', 'tA'), ('memberB', 'tB'), ('admin1', 'tA'), ('admin2', 'tB');",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn registry_read_acl_filters_by_team_and_role() {
        let svc = service().await;
        seed_two_group_enterprise(&svc).await;

        // Four resources per registry: org-wide, Group-A-only, Group-B-only,
        // and admin-only (org-wide but visibility='admin'). Each team's own
        // admin authors its team-scoped resource — admin1 (org_admin of tA
        // only) creating a tB-scoped resource is exactly the cross-tenant
        // write the ownership check exists to reject, so admin2 (tB's admin)
        // creates that one instead.
        for (name, scope, team, vis, author) in [
            ("org-skill", "org", None, "all", "admin1"),
            ("a-skill", "team", Some("tA"), "all", "admin1"),
            ("b-skill", "team", Some("tB"), "all", "admin2"),
            ("secret-skill", "org", None, "admin", "admin1"),
        ] {
            svc.upsert_skill(None, name, "", "", true, false, scope, team, vis, author)
                .await
                .unwrap();
        }
        for (name, scope, team, vis, author) in [
            ("org-mcp", "org", None, "all", "admin1"),
            ("a-mcp", "team", Some("tA"), "all", "admin1"),
            ("b-mcp", "team", Some("tB"), "all", "admin2"),
            ("secret-mcp", "org", None, "admin", "admin1"),
        ] {
            svc.upsert_mcp_registry(
                None,
                name,
                "sse",
                "https://x/sse",
                true,
                false,
                None,
                scope,
                team,
                vis,
                author,
            )
            .await
            .unwrap();
        }
        for (title, scope, team, vis, author) in [
            ("org-doc", "org", None, "all", "admin1"),
            ("a-doc", "team", Some("tA"), "all", "admin1"),
            ("b-doc", "team", Some("tB"), "all", "admin2"),
            ("secret-doc", "org", None, "admin", "admin1"),
        ] {
            svc.register_rag_document(title, None, None, None, scope, team, vis, author)
                .await
                .unwrap();
        }

        // memberA (Group A): org + Group-A only; never Group B, never admin-only.
        let a_skills: Vec<String> = svc
            .list_skills("memberA")
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(a_skills.len(), 2, "memberA sees org + Group A skills");
        assert!(a_skills.contains(&"org-skill".to_string()));
        assert!(a_skills.contains(&"a-skill".to_string()));
        assert!(
            !a_skills.contains(&"b-skill".to_string()),
            "Group B hidden from memberA"
        );
        assert!(
            !a_skills.contains(&"secret-skill".to_string()),
            "admin-only hidden from member"
        );
        assert_eq!(svc.list_mcp_registry("memberA").await.unwrap().len(), 2);
        assert_eq!(svc.list_rag_documents("memberA").await.unwrap().len(), 2);

        // memberB (Group B): org + Group-B only.
        let b_skills: Vec<String> = svc
            .list_skills("memberB")
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(b_skills.len(), 2);
        assert!(b_skills.contains(&"b-skill".to_string()));
        assert!(!b_skills.contains(&"a-skill".to_string()));
        assert_eq!(svc.list_mcp_registry("memberB").await.unwrap().len(), 2);
        assert_eq!(svc.list_rag_documents("memberB").await.unwrap().len(), 2);

        // admin1 (org_admin) sees everything, including both groups + admin-only.
        assert_eq!(svc.list_skills("admin1").await.unwrap().len(), 4);
        assert_eq!(svc.list_mcp_registry("admin1").await.unwrap().len(), 4);
        assert_eq!(svc.list_rag_documents("admin1").await.unwrap().len(), 4);

        // Standalone/personal owner (no one_user_org row) sees everything too —
        // the machine owner is never filtered (red line).
        assert_eq!(svc.list_skills("nobody").await.unwrap().len(), 4);
        assert_eq!(svc.list_rag_documents("nobody").await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn search_rag_visibility_join_scopes_documents_to_viewer() {
        // search_rag's embedding call needs a live endpoint, so exercise its ACL
        // predicate directly: run the exact `d.`-qualified join filter it builds
        // and assert which documents a member can retrieve chunks from.
        let svc = service().await;
        seed_two_group_enterprise(&svc).await;
        // Each team's own admin authors its team-scoped document — see the
        // identical comment in `registry_read_acl_filters_by_team_and_role`.
        for (title, scope, team, vis, author) in [
            ("org-doc", "org", None, "all", "admin1"),
            ("a-doc", "team", Some("tA"), "all", "admin1"),
            ("b-doc", "team", Some("tB"), "all", "admin2"),
            ("secret-doc", "org", None, "admin", "admin1"),
        ] {
            let doc = svc
                .register_rag_document(title, None, None, None, scope, team, vis, author)
                .await
                .unwrap();
            sqlx::query("INSERT INTO one_rag_chunks (id, document_id, chunk_index, content, embedding, created_at) VALUES (?, ?, 0, ?, ?, 0)")
                .bind(new_id("chunk"))
                .bind(&doc.id)
                .bind(format!("{title} body"))
                .bind(Vec::<u8>::new())
                .execute(&svc.pool)
                .await
                .unwrap();
        }

        let sql = format!(
            "SELECT d.title FROM one_rag_chunks c JOIN one_rag_documents d ON d.id = c.document_id WHERE {}",
            DevopsService::member_visibility_where("d.")
        );
        let titles: Vec<String> = sqlx::query_scalar(&sql)
            .bind("memberA")
            .fetch_all(&svc.pool)
            .await
            .unwrap();
        assert_eq!(titles.len(), 2, "memberA retrieves only org + Group A chunks");
        assert!(titles.contains(&"org-doc".to_string()));
        assert!(titles.contains(&"a-doc".to_string()));
        assert!(!titles.contains(&"b-doc".to_string()));
        assert!(!titles.contains(&"secret-doc".to_string()));
    }

    #[tokio::test]
    async fn free_tier_company_cannot_write_team_scoped_or_admin_only() {
        let svc = service().await;
        seed_two_group_enterprise(&svc).await;
        // Install billing + put admin1's company on the free tier.
        sqlx::raw_sql(
            "CREATE TABLE one_enterprise_members (user_id TEXT PRIMARY KEY, enterprise_id TEXT NOT NULL, role TEXT);
             CREATE TABLE one_enterprise_license (enterprise_id TEXT PRIMARY KEY, tier TEXT NOT NULL, seat_limit INTEGER, expires_at INTEGER, updated_at INTEGER);
             INSERT INTO one_enterprise_members (user_id, enterprise_id, role) VALUES ('admin1', 'ent1', 'admin');
             INSERT INTO one_enterprise_license (enterprise_id, tier, updated_at) VALUES ('ent1', 'free', 0);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();

        // Free tier: team scope + admin-only visibility are both gated.
        let err = svc
            .upsert_skill(None, "s", "", "", true, false, "team", Some("tA"), "all", "admin1")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
        let err = svc
            .upsert_skill(None, "s2", "", "", true, false, "org", None, "admin", "admin1")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "FORBIDDEN");
        // Plain org/all still works on free tier.
        svc.upsert_skill(None, "s3", "", "", true, false, "org", None, "all", "admin1")
            .await
            .unwrap();

        // Upgrade to enterprise → both allowed.
        sqlx::query("UPDATE one_enterprise_license SET tier = 'enterprise' WHERE enterprise_id = 'ent1'")
            .execute(&svc.pool)
            .await
            .unwrap();
        svc.upsert_skill(None, "s4", "", "", true, false, "team", Some("tA"), "admin", "admin1")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn registry_write_rejects_invalid_scope_or_unknown_group() {
        let svc = service().await;
        seed_two_group_enterprise(&svc).await;

        // Unknown project group.
        let err = svc
            .upsert_skill(None, "s", "", "", true, false, "team", Some("ghost"), "all", "admin1")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "BAD_REQUEST");

        // team scope without a team_id.
        let err = svc
            .upsert_skill(None, "s2", "", "", true, false, "team", None, "all", "admin1")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "BAD_REQUEST");

        // Bad scope / visibility values.
        assert_eq!(
            svc.upsert_skill(None, "s3", "", "", true, false, "planet", None, "all", "admin1")
                .await
                .unwrap_err()
                .code(),
            "BAD_REQUEST"
        );
        assert_eq!(
            svc.register_rag_document("d", None, None, None, "org", None, "secret", "admin1")
                .await
                .unwrap_err()
                .code(),
            "BAD_REQUEST"
        );

        // A valid team-scoped write to an existing group succeeds and persists.
        let ok = svc
            .upsert_skill(None, "s4", "", "", true, false, "team", Some("tA"), "admin", "admin1")
            .await
            .unwrap();
        assert_eq!(ok.scope, "team");
        assert_eq!(ok.team_id.as_deref(), Some("tA"));
        assert_eq!(ok.visibility, "admin");
    }

    /// An admin of one project group must not be able to delete, edit, or
    /// re-scope another project group's team-distributed skill/MCP/RAG
    /// document just by knowing its id — `require_registry_admin` (routes.rs)
    /// only confirms "admin of *some* group", not "admin of *this* group's
    /// resource", and the write path used to stop there.
    #[tokio::test]
    async fn registry_write_is_rejected_across_project_groups() {
        let svc = service().await;
        // admin2 (org_admin of Group B, active tenant tB) is the cross-group
        // attacker this test exists for — `seed_two_group_enterprise` sets
        // it up.
        seed_two_group_enterprise(&svc).await;

        let skill = svc
            .upsert_skill(
                None,
                "a-only-skill",
                "d",
                "c",
                true,
                false,
                "team",
                Some("tA"),
                "all",
                "admin1",
            )
            .await
            .unwrap();
        let mcp = svc
            .upsert_mcp_registry(
                None,
                "a-only-mcp",
                "sse",
                "https://a/sse",
                true,
                false,
                None,
                "team",
                Some("tA"),
                "all",
                "admin1",
            )
            .await
            .unwrap();
        let doc = svc
            .register_rag_document("a-only-doc", None, None, None, "team", Some("tA"), "all", "admin1")
            .await
            .unwrap();

        // admin2 (Group B admin) cannot delete Group A's resources.
        assert!(matches!(
            svc.delete_skill("admin2", &skill.id).await.unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.delete_mcp_registry("admin2", &mcp.id).await.unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.delete_rag_document("admin2", &doc.id).await.unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.set_document_content("admin2", &doc.id, "hijacked")
                .await
                .unwrap_err(),
            DevopsError::Forbidden(_)
        ));

        // admin2 cannot edit it either — not the content, and not a re-scope
        // to steal it out of Group A. The row must survive untouched, not
        // merely "the call returned an error" (the update could still have
        // partially applied before a later check tripped).
        assert!(matches!(
            svc.upsert_skill(
                Some(&skill.id),
                "hijacked-name",
                "d",
                "c",
                true,
                false,
                "org",
                None,
                "all",
                "admin2",
            )
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.upsert_mcp_registry(
                Some(&mcp.id),
                "hijacked-mcp",
                "sse",
                "https://evil/sse",
                true,
                false,
                None,
                "org",
                None,
                "all",
                "admin2",
            )
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));

        // Everything is still there, unchanged, and still owned by Group A.
        let still_there = svc.list_skills("admin1").await.unwrap();
        let skill_row = still_there.iter().find(|s| s.id == skill.id).unwrap();
        assert_eq!(skill_row.name, "a-only-skill", "name must not have been overwritten");
        assert_eq!(skill_row.scope, "team");
        assert_eq!(
            skill_row.team_id.as_deref(),
            Some("tA"),
            "must not have been re-scoped away from Group A"
        );

        let mcp_row = svc
            .list_mcp_registry("admin1")
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.id == mcp.id)
            .unwrap();
        assert_eq!(mcp_row.name, "a-only-mcp");
        assert_eq!(
            mcp_row.endpoint, "https://a/sse",
            "endpoint must not have been overwritten"
        );
        assert_eq!(mcp_row.team_id.as_deref(), Some("tA"));

        // The rightful Group A admin can still manage its own resources.
        svc.delete_skill("admin1", &skill.id).await.unwrap();
        svc.delete_mcp_registry("admin1", &mcp.id).await.unwrap();
        svc.delete_rag_document("admin1", &doc.id).await.unwrap();

        // Org-scoped resources stay reachable by any registry admin regardless
        // of project group — that mirrors the read-side ACL, which shows
        // `scope = 'org'` rows to every member. Only team scope is restricted.
        let org_skill = svc
            .upsert_skill(
                None,
                "org-wide-skill",
                "",
                "",
                true,
                false,
                "org",
                None,
                "all",
                "admin1",
            )
            .await
            .unwrap();
        svc.delete_skill("admin2", &org_skill.id).await.unwrap();

        // A standalone/personal owner (no `one_user_org` row) is unrestricted —
        // matches `viewer_is_privileged`'s existing "None role = machine owner".
        let personal_skill = svc
            .upsert_skill(
                None,
                "personal-skill",
                "",
                "",
                true,
                false,
                "team",
                Some("tA"),
                "all",
                "admin1",
            )
            .await
            .unwrap();
        svc.delete_skill("nobody", &personal_skill.id).await.unwrap();
    }

    /// The gap `registry_write_is_rejected_across_project_groups` doesn't
    /// cover: that test only exercises editing/deleting a row that already
    /// belongs to another team. This covers the two ways a write can target
    /// a team the actor doesn't own from the START — CREATING a new
    /// resource under it, and re-scoping the actor's OWN resource INTO it
    /// (a legitimate tA admin "donating" content into tB without tB's
    /// admin ever consenting).
    #[tokio::test]
    async fn cannot_create_or_rescope_into_a_team_the_actor_does_not_own() {
        let svc = service().await;
        seed_two_group_enterprise(&svc).await;

        // admin1 (org_admin of tA only) tries to author a brand-new
        // tB-scoped resource in each registry.
        assert!(matches!(
            svc.upsert_skill(None, "s", "", "", true, false, "team", Some("tB"), "all", "admin1")
                .await
                .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.upsert_mcp_registry(
                None,
                "m",
                "sse",
                "https://x/sse",
                true,
                false,
                None,
                "team",
                Some("tB"),
                "all",
                "admin1",
            )
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.register_rag_document("d", None, None, None, "team", Some("tB"), "all", "admin1")
                .await
                .unwrap_err(),
            DevopsError::Forbidden(_)
        ));

        // admin1 legitimately creates a tA skill, then tries to re-scope it
        // into tB in the same edit that also changes its content — neither
        // half should apply.
        let skill = svc
            .upsert_skill(None, "mine", "d", "c", true, false, "team", Some("tA"), "all", "admin1")
            .await
            .unwrap();
        assert!(matches!(
            svc.upsert_skill(
                Some(&skill.id),
                "mine",
                "d",
                "c",
                true,
                false,
                "team",
                Some("tB"),
                "all",
                "admin1",
            )
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        let unchanged = svc.list_skills("admin1").await.unwrap();
        assert_eq!(
            unchanged.iter().find(|s| s.id == skill.id).unwrap().team_id.as_deref(),
            Some("tA"),
            "must not have been re-scoped to tB"
        );
    }

    /// Model channels carry a live credential (`api_key_encrypted`) and DLP
    /// rules gate content compliance — neither is covered by the
    /// skill/MCP/RAG tests above, and both had NO ownership check at all
    /// before this fix (unlike skill/MCP, which at least checked the
    /// current row on update).
    #[tokio::test]
    async fn provider_channels_and_dlp_rules_reject_cross_team_writes() {
        // Provider channels encrypt the credential at write time, so this
        // test (unlike the others in this module) needs the deployment data
        // key — `provider_channel.rs`'s own test module sets one up the
        // same way.
        let svc = service().await.with_encryption_key([7u8; 32]);
        seed_two_group_enterprise(&svc).await;

        // Create into another team.
        assert!(matches!(
            svc.upsert_provider_channel(
                None,
                "chan",
                "openai",
                "https://gateway.example",
                Some("sk-real"),
                "[]",
                None,
                true,
                "team",
                Some("tB"),
                "all",
                "admin1",
            )
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.upsert_dlp_rule(UpsertDlpRule {
                id: None,
                name: "rule",
                matcher: "keyword",
                pattern: "secret",
                action: "block",
                enabled: true,
                scope: "team",
                team_id: Some("tB"),
                created_by: "admin1",
            })
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));

        // admin2 (tB's own admin) legitimately creates one, then admin1
        // (tA's admin) cannot update its base_url/key, re-scope it, or
        // delete it — the credential-hijack and rule-tampering scenarios.
        let channel = svc
            .upsert_provider_channel(
                None,
                "b-chan",
                "openai",
                "https://real-gateway.example",
                Some("sk-real"),
                "[]",
                None,
                true,
                "team",
                Some("tB"),
                "all",
                "admin2",
            )
            .await
            .unwrap();
        assert!(matches!(
            svc.upsert_provider_channel(
                Some(&channel.id),
                "b-chan",
                "openai",
                "https://attacker.example",
                Some("sk-stolen"),
                "[]",
                None,
                true,
                "team",
                Some("tB"),
                "all",
                "admin1",
            )
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.delete_provider_channel("admin1", &channel.id).await.unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        let still_real = svc.list_provider_channels("admin2").await.unwrap();
        assert_eq!(
            still_real
                .iter()
                .find(|c| c.id == channel.id)
                .unwrap()
                .upstream_base_url,
            "https://real-gateway.example",
            "base_url must not have been hijacked"
        );

        let rule = svc
            .upsert_dlp_rule(UpsertDlpRule {
                id: None,
                name: "b-rule",
                matcher: "keyword",
                pattern: "secret",
                action: "block",
                enabled: true,
                scope: "team",
                team_id: Some("tB"),
                created_by: "admin2",
            })
            .await
            .unwrap();
        assert!(matches!(
            svc.upsert_dlp_rule(UpsertDlpRule {
                id: Some(&rule.id),
                name: "b-rule",
                matcher: "keyword",
                pattern: "secret",
                action: "log", // weakened from block
                enabled: true,
                scope: "team",
                team_id: Some("tB"),
                created_by: "admin1",
            })
            .await
            .unwrap_err(),
            DevopsError::Forbidden(_)
        ));
        assert!(matches!(
            svc.delete_dlp_rule("admin1", &rule.id).await.unwrap_err(),
            DevopsError::Forbidden(_)
        ));
    }

    #[tokio::test]
    async fn registries_crud() {
        let svc = service().await;

        let skill = svc
            .upsert_skill(
                None,
                "review",
                "code review",
                "...",
                true,
                false,
                "org",
                None,
                "all",
                "u1",
            )
            .await
            .unwrap();
        assert!(!skill.auto_active);
        assert_eq!(skill.scope, "org");
        assert_eq!(skill.visibility, "all");
        let skill = svc
            .upsert_skill(
                Some(&skill.id),
                "review",
                "better desc",
                "...",
                false,
                true,
                "org",
                None,
                "all",
                "u1",
            )
            .await
            .unwrap();
        assert!(!skill.enabled);
        assert!(skill.auto_active, "admin can flip a skill to auto-active");
        assert_eq!(svc.list_skills("u1").await.unwrap().len(), 1);
        svc.delete_skill("u1", &skill.id).await.unwrap();
        assert!(svc.list_skills("u1").await.unwrap().is_empty());

        let mcp = svc
            .upsert_mcp_registry(
                None,
                "search",
                "sse",
                "https://mcp.corp/sse",
                true,
                true,
                None,
                "org",
                None,
                "all",
                "u1",
            )
            .await
            .unwrap();
        assert!(mcp.has_keys);
        assert_eq!(svc.list_mcp_registry("u1").await.unwrap().len(), 1);
        let err = svc
            .upsert_mcp_registry(None, "bad", "ws", "", true, false, None, "org", None, "all", "u1")
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
        svc.delete_mcp_registry("u1", &mcp.id).await.unwrap();

        let doc = svc
            .register_rag_document(
                "handbook.pdf",
                Some("/data/handbook.pdf"),
                Some(1024),
                Some("application/pdf"),
                "org",
                None,
                "all",
                "u1",
            )
            .await
            .unwrap();
        assert_eq!(doc.status, "pending");
        assert_eq!(svc.list_rag_documents("u1").await.unwrap().len(), 1);
        svc.delete_rag_document("u1", &doc.id).await.unwrap();
        assert!(svc.list_rag_documents("u1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn milestone_crud_and_requirement_link_clearing() {
        let svc = service().await;
        let m = svc
            .create_milestone(
                "t1",
                "u1",
                Some("Alice"),
                "v1.0 发布",
                Some("首个灰度"),
                Some(1_800_000_000_000),
            )
            .await
            .unwrap();
        assert_eq!(m.status, "active");

        let m = svc
            .update_milestone("t1", &m.id, Some("v1.0 GA"), Some(None), Some("completed"), None)
            .await
            .unwrap();
        assert_eq!(m.title, "v1.0 GA");
        assert_eq!(m.status, "completed");
        assert!(m.description.is_none());
        assert_eq!(m.due_at, Some(1_800_000_000_000));

        let err = svc
            .update_milestone("t1", &m.id, None, None, Some("bogus"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // A requirement pointing at the milestone gets its link cleared on delete.
        let req = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "linked".into(),
                    milestone_id: Some(m.id.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(req.milestone_id.as_deref(), Some(m.id.as_str()));

        svc.delete_milestone("t1", &m.id).await.unwrap();
        assert!(svc.list_milestones("t1").await.unwrap().is_empty());
        let tree = svc.requirements_tree("t1").await.unwrap();
        assert_eq!(tree[0].milestone_id, None);

        let err = svc.delete_milestone("t1", "missing").await.unwrap_err();
        assert!(matches!(err, DevopsError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_plan_and_case_crud() {
        let svc = service().await;

        // Create plan
        let plan = svc
            .create_test_plan(
                "t1",
                "u1",
                Some("Alice"),
                "登录冒烟测试",
                Some("覆盖 SSO 和密码登录"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(plan.status, "draft");
        assert_eq!(svc.list_test_plans("t1").await.unwrap().len(), 1);

        // Update plan
        let plan = svc
            .update_test_plan("t1", &plan.id, Some("登录回归测试"), None, Some("active"), None)
            .await
            .unwrap();
        assert_eq!(plan.status, "active");

        // Create cases
        let c1 = svc
            .create_test_case("t1", &plan.id, "u1", Some("Alice"), "密码登录成功", None, None, None)
            .await
            .unwrap();
        assert_eq!(c1.status, "pending");
        let c2 = svc
            .create_test_case("t1", &plan.id, "u1", Some("Alice"), "错误密码被拒", None, None, None)
            .await
            .unwrap();

        let cases = svc.list_test_cases("t1", &plan.id).await.unwrap();
        assert_eq!(cases.len(), 2);

        // Update case status
        let c1 = svc
            .update_test_case("t1", &c1.id, None, Some("passed"), None, None, None)
            .await
            .unwrap();
        assert_eq!(c1.status, "passed");

        let err = svc
            .update_test_case("t1", &c2.id, None, Some("bogus"), None, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // Delete case
        svc.delete_test_case("t1", &c2.id).await.unwrap();
        assert_eq!(svc.list_test_cases("t1", &plan.id).await.unwrap().len(), 1);

        // Delete plan cascades to remaining cases
        svc.delete_test_plan("t1", &plan.id).await.unwrap();
        assert!(svc.list_test_plans("t1").await.unwrap().is_empty());
    }

    /// Deleting a knowledge-base document must also erase its text from the
    /// lexical index. Retrieval already filters orphaned FTS rows out via the
    /// join, so this is about data retention rather than leakage: a document
    /// the operator deleted must not leave its full text sitting on disk.
    #[tokio::test]
    async fn deleting_a_rag_document_erases_its_lexical_rows() {
        let svc = service().await;
        let doc = svc
            .register_rag_document("Confidential", None, None, None, "org", None, "all", "admin1")
            .await
            .unwrap();

        // Stand in for `process_rag_document`, whose embedding call needs a
        // live endpoint: write the chunk and its lexical mirror directly.
        sqlx::query(
            "INSERT INTO one_rag_chunks (id, document_id, chunk_index, content, embedding, created_at) \
             VALUES ('c1', ?, 0, 'the merger closes on the third of March', X'', 0)",
        )
        .bind(&doc.id)
        .execute(&svc.pool)
        .await
        .unwrap();
        crate::retrieval::sync_document(
            &svc.pool,
            &doc.id,
            &[("c1".to_string(), "the merger closes on the third of March".to_string())],
        )
        .await
        .unwrap();

        let indexed = || async {
            sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {} WHERE content MATCH '\"merger\"'",
                crate::retrieval::FTS_TABLE
            ))
            .fetch_one(&svc.pool)
            .await
            .unwrap()
        };
        assert_eq!(indexed().await, 1, "precondition: the text is in the lexical index");

        svc.delete_rag_document("admin1", &doc.id).await.unwrap();
        assert_eq!(indexed().await, 0, "deleted document text must not remain on disk");
    }

    #[tokio::test]
    async fn pipeline_and_run_crud() {
        let svc = service().await;

        // Create pipeline
        let pipe = svc
            .create_pipeline(
                "t1",
                "u1",
                Some("Alice"),
                "CI 主流水线",
                Some("main 分支推送触发"),
                Some("push"),
            )
            .await
            .unwrap();
        assert_eq!(pipe.status, "active");
        assert_eq!(pipe.trigger, "push");
        assert_eq!(svc.list_pipelines("t1").await.unwrap().len(), 1);

        // Update pipeline
        let pipe = svc
            .update_pipeline("t1", &pipe.id, None, None, Some("disabled"), None)
            .await
            .unwrap();
        assert_eq!(pipe.status, "disabled");

        let err = svc
            .update_pipeline("t1", &pipe.id, None, None, Some("bad"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // Create run
        let run = svc.create_pipeline_run("t1", &pipe.id, Some("u1")).await.unwrap();
        assert_eq!(run.status, "pending");

        let run = svc
            .update_pipeline_run(
                "t1",
                &run.id,
                Some("running"),
                Some(Some(1_800_000_000_000)),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(run.status, "running");
        assert_eq!(run.started_at, Some(1_800_000_000_000));

        let run = svc
            .update_pipeline_run(
                "t1",
                &run.id,
                Some("success"),
                None,
                Some(Some(1_800_001_000_000)),
                Some(Some("Build OK")),
            )
            .await
            .unwrap();
        assert_eq!(run.status, "success");
        assert!(run.log.as_deref() == Some("Build OK"));

        let runs = svc.list_pipeline_runs("t1", &pipe.id).await.unwrap();
        assert_eq!(runs.len(), 1);

        // Delete pipeline cascades to runs
        svc.delete_pipeline("t1", &pipe.id).await.unwrap();
        assert!(svc.list_pipelines("t1").await.unwrap().is_empty());
    }

    /// The security fix this locks down: requirements / milestones / test
    /// plans / test cases / pipelines used to have no tenant column at all,
    /// so any org_admin on any tenant of a shared server could read and
    /// write every other tenant's collaboration data. For each of the five
    /// resource families, seed a row under tenant "t1" and prove tenant "t2"
    /// (a) never sees it in a list and (b) gets NotFound trying to
    /// update/delete it by id — not a silent no-op that would still leak via
    /// a changed response shape, and not the wrong-tenant row unexpectedly
    /// succeeding.
    #[tokio::test]
    async fn collaboration_resources_are_isolated_per_tenant() {
        let svc = service().await;

        // -- requirements --
        let req = svc
            .create_requirement(
                "t1",
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "t1 only".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(svc.requirements_tree("t2").await.unwrap().is_empty());
        assert!(matches!(
            svc.get_requirement_row("t2", &req.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert!(matches!(
            svc.update_requirement(
                "t2",
                &req.id,
                UpdateRequirementInput {
                    subject: Some("hijacked".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert_eq!(svc.get_requirement_row("t1", &req.id).await.unwrap().subject, "t1 only");
        assert!(matches!(
            svc.delete_requirement("t2", &req.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        // A t2 caller cannot even discover the t1 requirement exists by
        // trying to comment on it or use it as a parent/breakdown target.
        assert!(matches!(
            svc.create_comment("t2", &req.id, "u2", "Eve", "hi").await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        // The requirement is still perfectly usable from its own tenant.
        assert!(svc.get_requirement_row("t1", &req.id).await.is_ok());

        // -- milestones --
        let m = svc
            .create_milestone("t1", "u1", Some("Alice"), "t1 milestone", None, None)
            .await
            .unwrap();
        assert!(svc.list_milestones("t2").await.unwrap().is_empty());
        assert!(matches!(
            svc.update_milestone("t2", &m.id, Some("hijacked"), None, None, None)
                .await
                .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        // The failed cross-tenant update must not have silently written
        // through anyway — an UPDATE missing its own tenant filter can still
        // succeed even when a *later* refetch is correctly scoped, silently
        // corrupting the other tenant's row while still reporting NotFound.
        assert_eq!(svc.fetch_milestone("t1", &m.id).await.unwrap().title, "t1 milestone");
        assert!(matches!(
            svc.delete_milestone("t2", &m.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));

        // -- test plans + cases --
        let plan = svc
            .create_test_plan("t1", "u1", Some("Alice"), "t1 plan", None, None)
            .await
            .unwrap();
        let case = svc
            .create_test_case("t1", &plan.id, "u1", Some("Alice"), "t1 case", None, None, None)
            .await
            .unwrap();
        assert!(svc.list_test_plans("t2").await.unwrap().is_empty());
        assert!(matches!(
            svc.list_test_cases("t2", &plan.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert!(matches!(
            svc.create_test_case("t2", &plan.id, "u2", None, "sneaky", None, None, None)
                .await
                .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert!(matches!(
            svc.update_test_plan("t2", &plan.id, Some("hijacked"), None, None, None)
                .await
                .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert_eq!(svc.fetch_test_plan("t1", &plan.id).await.unwrap().title, "t1 plan");
        assert!(matches!(
            svc.update_test_case("t2", &case.id, Some("hijacked"), None, None, None, None)
                .await
                .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert_eq!(svc.fetch_test_case("t1", &case.id).await.unwrap().title, "t1 case");
        assert!(matches!(
            svc.delete_test_case("t2", &case.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert!(matches!(
            svc.delete_test_plan("t2", &plan.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));

        // -- pipelines + runs --
        let pipe = svc
            .create_pipeline("t1", "u1", Some("Alice"), "t1 pipeline", None, None)
            .await
            .unwrap();
        let run = svc.create_pipeline_run("t1", &pipe.id, Some("u1")).await.unwrap();
        assert!(svc.list_pipelines("t2").await.unwrap().is_empty());
        assert!(matches!(
            svc.list_pipeline_runs("t2", &pipe.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert!(matches!(
            svc.create_pipeline_run("t2", &pipe.id, Some("u2")).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert!(matches!(
            svc.update_pipeline_run("t2", &run.id, Some("success"), None, None, None)
                .await
                .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert_eq!(svc.fetch_pipeline_run("t1", &run.id).await.unwrap().status, "pending");
        assert!(matches!(
            svc.update_pipeline("t2", &pipe.id, Some("hijacked"), None, None, None)
                .await
                .unwrap_err(),
            DevopsError::NotFound(_)
        ));
        assert_eq!(svc.fetch_pipeline("t1", &pipe.id).await.unwrap().name, "t1 pipeline");
        assert!(matches!(
            svc.delete_pipeline("t2", &pipe.id).await.unwrap_err(),
            DevopsError::NotFound(_)
        ));

        // Everything is still there and untouched from t1's own perspective.
        assert_eq!(svc.requirements_tree("t1").await.unwrap().len(), 1);
        assert_eq!(svc.list_milestones("t1").await.unwrap().len(), 1);
        assert_eq!(svc.list_test_plans("t1").await.unwrap().len(), 1);
        assert_eq!(svc.list_pipelines("t1").await.unwrap().len(), 1);
    }
}
