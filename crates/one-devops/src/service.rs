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
    pool: SqlitePool,
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
        Self { pool }
    }

    // -- requirements -----------------------------------------------------

    /// Full requirements forest, children nested, roots + children both
    /// ordered by updated_at DESC (matches the 1one tree endpoint).
    pub async fn requirements_tree(&self) -> Result<Vec<RequirementDto>, DevopsError> {
        let rows = sqlx::query_as::<_, RequirementRow>(
            "SELECT id, parent_id, type, subject, description, status, priority, assigned_to, \
                    milestone_id, autopilot, creator_id, creator_name, created_at, updated_at \
             FROM one_requirements ORDER BY updated_at DESC",
        )
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
            self.require_requirement(parent_id).await?;
        }

        let id = new_id("req");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_requirements \
                (id, parent_id, type, subject, description, status, priority, assigned_to, \
                 milestone_id, autopilot, creator_id, creator_name, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'backlog', ?, NULL, ?, ?, ?, ?, ?, ?)",
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
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(RequirementDto::from_row(self.fetch_requirement(&id).await?))
    }

    /// Create the parsed breakdown children under `parent_id` (A1 L2). Each
    /// item's kind/priority is already clamped to a valid enum. Returns the
    /// created rows. Fields already validated upstream, so a per-child failure
    /// is unexpected and aborts the batch.
    pub async fn create_breakdown_children(
        &self,
        parent_id: &str,
        creator_id: &str,
        creator_name: Option<&str>,
        items: &[crate::breakdown::BreakdownItem],
    ) -> Result<Vec<RequirementDto>, DevopsError> {
        self.require_requirement(parent_id).await?;
        let mut created = Vec::with_capacity(items.len());
        for item in items {
            let child = self
                .create_requirement(
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

    pub async fn update_requirement(&self, id: &str, input: UpdateRequirementInput) -> Result<(), DevopsError> {
        let row = self.require_requirement(id).await?;

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
            self.require_requirement(parent_id).await?;
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
    pub async fn claim_requirement_for_dispatch(&self, id: &str) -> Result<bool, DevopsError> {
        let res = sqlx::query(
            "UPDATE one_requirements SET status = 'developing', updated_at = ? \
             WHERE id = ? AND status IN ('backlog', 'planning')",
        )
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Delete a requirement and its whole subtree (plus their comments).
    pub async fn delete_requirement(&self, id: &str) -> Result<(), DevopsError> {
        self.require_requirement(id).await?;
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

    pub async fn list_comments(&self, requirement_id: &str) -> Result<Vec<RequirementCommentDto>, DevopsError> {
        self.require_requirement(requirement_id).await?;
        Ok(sqlx::query_as::<_, RequirementCommentDto>(
            "SELECT id, requirement_id, author_type, author_id, author_name, body, metadata, created_at \
             FROM one_requirement_comments WHERE requirement_id = ? ORDER BY created_at ASC",
        )
        .bind(requirement_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_comment(
        &self,
        requirement_id: &str,
        author_id: &str,
        author_name: &str,
        body: &str,
    ) -> Result<RequirementCommentDto, DevopsError> {
        self.require_requirement(requirement_id).await?;
        let body = body.trim();
        if body.is_empty() {
            return Err(DevopsError::BadRequest("comment body is required".into()));
        }
        let id = new_id("reqc");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_requirement_comments \
                (id, requirement_id, author_type, author_id, author_name, body, metadata, created_at) \
             VALUES (?, ?, 'user', ?, ?, ?, NULL, ?)",
        )
        .bind(&id)
        .bind(requirement_id)
        .bind(author_id)
        .bind(author_name)
        .bind(body)
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
    pub async fn get_requirement_row(&self, id: &str) -> Result<RequirementRow, DevopsError> {
        self.fetch_requirement(id).await
    }

    /// Insert an agent/autopilot-authored comment carrying optional metadata
    /// JSON. Used by dispatch to record the run linkage on the requirement.
    pub async fn insert_agent_comment(
        &self,
        requirement_id: &str,
        author_type: &str,
        author_id: Option<&str>,
        author_name: &str,
        body: &str,
        metadata: Option<String>,
    ) -> Result<RequirementCommentDto, DevopsError> {
        self.require_requirement(requirement_id).await?;
        let id = new_id("reqc");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_requirement_comments \
                (id, requirement_id, author_type, author_id, author_name, body, metadata, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(requirement_id)
        .bind(author_type)
        .bind(author_id)
        .bind(author_name)
        .bind(body)
        .bind(metadata.as_deref())
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

    async fn fetch_requirement(&self, id: &str) -> Result<RequirementRow, DevopsError> {
        sqlx::query_as::<_, RequirementRow>(
            "SELECT id, parent_id, type, subject, description, status, priority, assigned_to, \
                    milestone_id, autopilot, creator_id, creator_name, created_at, updated_at \
             FROM one_requirements WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("requirement {id}")))
    }

    async fn require_requirement(&self, id: &str) -> Result<RequirementRow, DevopsError> {
        self.fetch_requirement(id).await
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

    // -- registry read ACL (P0-4 fine-grained RBAC) -----------------------

    /// WHERE fragment restricting registry reads for a non-privileged member:
    /// org-wide resources, plus team resources for any project group the viewer
    /// belongs to (reuses P0-1 `one_user_org` multi-membership), and only
    /// `visibility='all'` (admin-only resources stay hidden). Binds
    /// `viewer_user_id` **once**. `prefix` is the column qualifier ("" for a
    /// single-table read, "d." for the `search_rag` join).
    fn member_visibility_where(prefix: &str) -> String {
        format!(
            "({p}scope = 'org' OR ({p}scope = 'team' AND {p}team_id IN \
               (SELECT tenant_id FROM one_user_org WHERE user_id = ?))) AND {p}visibility = 'all'",
            p = prefix
        )
    }

    /// True when the viewer sees every resource unfiltered: an org/system admin,
    /// or a standalone/personal-edition owner (no `one_user_org` row →
    /// `user_org_role` is `None`, the machine owner). Members are filtered.
    async fn viewer_is_privileged(&self, viewer_user_id: &str) -> Result<bool, DevopsError> {
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
    async fn validate_resource_scope<'a>(
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

    pub async fn delete_skill(&self, id: &str) -> Result<(), DevopsError> {
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

    pub async fn delete_mcp_registry(&self, id: &str) -> Result<(), DevopsError> {
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

    pub async fn delete_rag_document(&self, id: &str) -> Result<(), DevopsError> {
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

    pub async fn list_milestones(&self) -> Result<Vec<MilestoneDto>, DevopsError> {
        Ok(sqlx::query_as::<_, MilestoneDto>(
            "SELECT id, title, description, status, due_at, creator_id, creator_name, created_at, updated_at \
             FROM one_milestones ORDER BY \
                CASE status WHEN 'active' THEN 0 WHEN 'completed' THEN 1 ELSE 2 END, updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_milestone(
        &self,
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
                (id, title, description, status, due_at, creator_id, creator_name, created_at, updated_at) \
             VALUES (?, ?, ?, 'active', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(description)
        .bind(due_at)
        .bind(creator_id)
        .bind(creator_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_milestone(&id).await
    }

    pub async fn update_milestone(
        &self,
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
        sqlx::query(
            "UPDATE one_milestones SET \
                title = CASE WHEN ? THEN ? ELSE title END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                due_at = CASE WHEN ? THEN ? ELSE due_at END, \
                updated_at = ? \
             WHERE id = ?",
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
        .execute(&self.pool)
        .await?;
        self.fetch_milestone(id).await
    }

    pub async fn delete_milestone(&self, id: &str) -> Result<(), DevopsError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_milestones WHERE id = ?")
            .bind(id)
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

    async fn fetch_milestone(&self, id: &str) -> Result<MilestoneDto, DevopsError> {
        sqlx::query_as::<_, MilestoneDto>(
            "SELECT id, title, description, status, due_at, creator_id, creator_name, created_at, updated_at \
             FROM one_milestones WHERE id = ?",
        )
        .bind(id)
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
    pub async fn set_document_content(&self, id: &str, content: &str) -> Result<(), DevopsError> {
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
    pub async fn process_rag_document(&self, id: &str) -> Result<i64, DevopsError> {
        let content: Option<String> = sqlx::query_scalar("SELECT content FROM one_rag_documents WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DevopsError::NotFound(format!("rag document {id}")))?;
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
        for (idx, (chunk, vector)) in chunks.iter().zip(vectors.iter()).enumerate() {
            sqlx::query(
                "INSERT INTO one_rag_chunks (id, document_id, chunk_index, content, embedding, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(new_id("ragc"))
            .bind(id)
            .bind(idx as i64)
            .bind(chunk)
            .bind(crate::embedding::pack_embedding(vector))
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        let count = chunks.len() as i64;
        sqlx::query("UPDATE one_rag_documents SET status = 'ready', last_error = NULL, chunk_count = ? WHERE id = ?")
            .bind(count)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        if let Some(dims) = dims {
            let _ = sqlx::query("UPDATE one_rag_config SET dimensions = ? WHERE id = 'default'")
                .bind(dims)
                .execute(&self.pool)
                .await;
        }
        Ok(count)
    }

    /// Embed the query and return the top-k chunks by cosine similarity.
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
        let config = self.load_embedding_config().await?;
        let query_vec = crate::embedding::embed(&config, &[query.to_owned()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| DevopsError::Internal("empty query embedding".into()))?;

        // ACL: a member only retrieves chunks of documents visible to them (org
        // + their project groups, visibility='all'); admins/owner retrieve all.
        // Enforced in the join so an invisible document's chunks never surface.
        const BASE: &str = "SELECT c.document_id, c.chunk_index, c.content, c.embedding, d.title \
                            FROM one_rag_chunks c JOIN one_rag_documents d ON d.id = c.document_id";
        let privileged = self.viewer_is_privileged(viewer_user_id).await?;
        let sql = if privileged {
            BASE.to_string()
        } else {
            format!("{BASE} WHERE {}", Self::member_visibility_where("d."))
        };
        let mut q = sqlx::query_as::<_, (String, i64, String, Vec<u8>, String)>(&sql);
        if !privileged {
            q = q.bind(viewer_user_id);
        }
        let rows: Vec<(String, i64, String, Vec<u8>, String)> = q.fetch_all(&self.pool).await?;

        let mut hits: Vec<RagSearchHit> = rows
            .into_iter()
            .map(|(document_id, chunk_index, content, blob, document_title)| {
                let score = crate::embedding::cosine_similarity(&query_vec, &crate::embedding::unpack_embedding(&blob));
                RagSearchHit {
                    document_id,
                    document_title,
                    chunk_index,
                    content,
                    score,
                }
            })
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k.max(1));
        Ok(hits)
    }

    // -- test plans (A4) --------------------------------------------------

    pub async fn list_test_plans(&self) -> Result<Vec<TestPlanDto>, DevopsError> {
        Ok(sqlx::query_as::<_, TestPlanDto>(
            "SELECT id, title, description, status, requirement_id, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_plans ORDER BY \
                CASE status WHEN 'active' THEN 0 WHEN 'draft' THEN 1 WHEN 'completed' THEN 2 ELSE 3 END, \
                updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_test_plan(
        &self,
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
                (id, title, description, status, requirement_id, creator_id, creator_name, created_at, updated_at) \
             VALUES (?, ?, ?, 'draft', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(description)
        .bind(requirement_id)
        .bind(creator_id)
        .bind(creator_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_test_plan(&id).await
    }

    pub async fn update_test_plan(
        &self,
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
        sqlx::query(
            "UPDATE one_test_plans SET \
                title = CASE WHEN ? THEN ? ELSE title END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                requirement_id = CASE WHEN ? THEN ? ELSE requirement_id END, \
                updated_at = ? \
             WHERE id = ?",
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
        .execute(&self.pool)
        .await?;
        self.fetch_test_plan(id).await
    }

    pub async fn delete_test_plan(&self, id: &str) -> Result<(), DevopsError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_test_plans WHERE id = ?")
            .bind(id)
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

    async fn fetch_test_plan(&self, id: &str) -> Result<TestPlanDto, DevopsError> {
        sqlx::query_as::<_, TestPlanDto>(
            "SELECT id, title, description, status, requirement_id, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_plans WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("test plan {id}")))
    }

    // -- test cases ---------------------------------------------------------

    pub async fn list_test_cases(&self, plan_id: &str) -> Result<Vec<TestCaseDto>, DevopsError> {
        // Verify plan exists first.
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_test_plans WHERE id = ?")
            .bind(plan_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("test plan {plan_id}")));
        }
        Ok(sqlx::query_as::<_, TestCaseDto>(
            "SELECT id, plan_id, title, description, steps, expected, status, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_cases WHERE plan_id = ? ORDER BY created_at ASC",
        )
        .bind(plan_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_test_case(
        &self,
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
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_test_plans WHERE id = ?")
            .bind(plan_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("test plan {plan_id}")));
        }
        let id = new_id("tcase");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_test_cases \
                (id, plan_id, title, description, steps, expected, status, creator_id, creator_name, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(plan_id)
        .bind(title)
        .bind(description)
        .bind(steps)
        .bind(expected)
        .bind(creator_id)
        .bind(creator_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_test_case(&id).await
    }

    pub async fn update_test_case(
        &self,
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
        sqlx::query(
            "UPDATE one_test_cases SET \
                title = CASE WHEN ? THEN ? ELSE title END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                steps = CASE WHEN ? THEN ? ELSE steps END, \
                expected = CASE WHEN ? THEN ? ELSE expected END, \
                updated_at = ? \
             WHERE id = ?",
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
        .execute(&self.pool)
        .await?;
        self.fetch_test_case(id).await
    }

    pub async fn delete_test_case(&self, id: &str) -> Result<(), DevopsError> {
        let deleted = sqlx::query("DELETE FROM one_test_cases WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("test case {id}")));
        }
        Ok(())
    }

    async fn fetch_test_case(&self, id: &str) -> Result<TestCaseDto, DevopsError> {
        sqlx::query_as::<_, TestCaseDto>(
            "SELECT id, plan_id, title, description, steps, expected, status, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_test_cases WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("test case {id}")))
    }

    // -- pipelines (A4) ---------------------------------------------------

    pub async fn list_pipelines(&self) -> Result<Vec<PipelineDto>, DevopsError> {
        Ok(sqlx::query_as::<_, PipelineDto>(
            "SELECT id, name, description, status, trigger, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_pipelines ORDER BY \
                CASE status WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_pipeline(
        &self,
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
                (id, name, description, status, trigger, creator_id, creator_name, created_at, updated_at) \
             VALUES (?, ?, ?, 'active', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(description)
        .bind(trigger)
        .bind(creator_id)
        .bind(creator_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_pipeline(&id).await
    }

    pub async fn update_pipeline(
        &self,
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
        sqlx::query(
            "UPDATE one_pipelines SET \
                name = CASE WHEN ? THEN ? ELSE name END, \
                description = CASE WHEN ? THEN ? ELSE description END, \
                status = CASE WHEN ? THEN ? ELSE status END, \
                trigger = CASE WHEN ? THEN ? ELSE trigger END, \
                updated_at = ? \
             WHERE id = ?",
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
        .execute(&self.pool)
        .await?;
        self.fetch_pipeline(id).await
    }

    pub async fn delete_pipeline(&self, id: &str) -> Result<(), DevopsError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM one_pipelines WHERE id = ?")
            .bind(id)
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

    async fn fetch_pipeline(&self, id: &str) -> Result<PipelineDto, DevopsError> {
        sqlx::query_as::<_, PipelineDto>(
            "SELECT id, name, description, status, trigger, creator_id, creator_name, \
                    created_at, updated_at \
             FROM one_pipelines WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("pipeline {id}")))
    }

    // -- pipeline runs ------------------------------------------------------

    pub async fn list_pipeline_runs(&self, pipeline_id: &str) -> Result<Vec<PipelineRunDto>, DevopsError> {
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_pipelines WHERE id = ?")
            .bind(pipeline_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("pipeline {pipeline_id}")));
        }
        Ok(sqlx::query_as::<_, PipelineRunDto>(
            "SELECT id, pipeline_id, status, triggered_by, started_at, finished_at, log, \
                    created_at, updated_at \
             FROM one_pipeline_runs WHERE pipeline_id = ? ORDER BY created_at DESC LIMIT 100",
        )
        .bind(pipeline_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_pipeline_run(
        &self,
        pipeline_id: &str,
        triggered_by: Option<&str>,
    ) -> Result<PipelineRunDto, DevopsError> {
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM one_pipelines WHERE id = ?")
            .bind(pipeline_id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Err(DevopsError::NotFound(format!("pipeline {pipeline_id}")));
        }
        let id = new_id("run");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_pipeline_runs \
                (id, pipeline_id, status, triggered_by, started_at, finished_at, log, created_at, updated_at) \
             VALUES (?, ?, 'pending', ?, NULL, NULL, NULL, ?, ?)",
        )
        .bind(&id)
        .bind(pipeline_id)
        .bind(triggered_by)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.fetch_pipeline_run(&id).await
    }

    pub async fn update_pipeline_run(
        &self,
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
        sqlx::query(
            "UPDATE one_pipeline_runs SET \
                status = CASE WHEN ? THEN ? ELSE status END, \
                started_at = CASE WHEN ? THEN ? ELSE started_at END, \
                finished_at = CASE WHEN ? THEN ? ELSE finished_at END, \
                log = CASE WHEN ? THEN ? ELSE log END, \
                updated_at = ? \
             WHERE id = ?",
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
        .execute(&self.pool)
        .await?;
        self.fetch_pipeline_run(id).await
    }

    async fn fetch_pipeline_run(&self, id: &str) -> Result<PipelineRunDto, DevopsError> {
        sqlx::query_as::<_, PipelineRunDto>(
            "SELECT id, pipeline_id, status, triggered_by, started_at, finished_at, log, \
                    created_at, updated_at \
             FROM one_pipeline_runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DevopsError::NotFound(format!("pipeline run {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(svc.claim_requirement_for_dispatch(&req.id).await.unwrap());
        assert_eq!(svc.get_requirement_row(&req.id).await.unwrap().status, "developing");

        // Second claim loses — the requirement is no longer in a pre-dev status,
        // so a concurrent dispatch/autopilot can't fire a duplicate run.
        assert!(!svc.claim_requirement_for_dispatch(&req.id).await.unwrap());

        // A requirement already past pre-dev is likewise not claimable here.
        let planning = svc
            .create_requirement(
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
            &planning.id,
            UpdateRequirementInput {
                status: Some("planning".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(svc.claim_requirement_for_dispatch(&planning.id).await.unwrap());
        assert!(!svc.claim_requirement_for_dispatch(&planning.id).await.unwrap());
    }

    #[tokio::test]
    async fn requirement_crud_and_tree_nesting() {
        let svc = service().await;
        let epic = svc
            .create_requirement(
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

        let tree = svc.requirements_tree().await.unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].id, epic.id);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].id, story.id);

        svc.update_requirement(
            &story.id,
            UpdateRequirementInput {
                status: Some("developing".into()),
                assigned_to: Some(Some("agent-1".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tree = svc.requirements_tree().await.unwrap();
        assert_eq!(tree[0].children[0].status, "developing");
        assert_eq!(tree[0].children[0].assigned_to.as_deref(), Some("agent-1"));

        // Deleting the epic removes the subtree.
        svc.delete_requirement(&epic.id).await.unwrap();
        assert!(svc.requirements_tree().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn autopilot_flag_persists_and_toggles() {
        let svc = service().await;
        let req = svc
            .create_requirement(
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
            &req.id,
            UpdateRequirementInput {
                priority: Some("high".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        svc.update_requirement(
            &req.id,
            UpdateRequirementInput {
                autopilot: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tree = svc.requirements_tree().await.unwrap();
        let refreshed = tree.iter().find(|r| r.id == req.id).unwrap();
        assert!(!refreshed.autopilot);
        assert_eq!(refreshed.priority, "high");
    }

    #[tokio::test]
    async fn requirement_validation_rejects_bad_values() {
        let svc = service().await;
        let err = svc
            .create_requirement(
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
                "u1",
                Some("Alice"),
                CreateRequirementInput {
                    subject: "with comments".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.create_comment(&req.id, "u1", "Alice", "first!").await.unwrap();
        let comments = svc.list_comments(&req.id).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "first!");
        assert_eq!(comments[0].author_type, "user");

        let err = svc.create_comment(&req.id, "u1", "Alice", "  ").await.unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
        let err = svc.list_comments("missing").await.unwrap_err();
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
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('memberA', 'tA', 'member'), ('memberB', 'tB', 'member'), ('admin1', 'tA', 'org_admin');
             INSERT INTO one_active_tenant (user_id, tenant_id) VALUES ('memberA', 'tA'), ('memberB', 'tB'), ('admin1', 'tA');",
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
        // and admin-only (org-wide but visibility='admin'). admin1 authors them.
        for (name, scope, team, vis) in [
            ("org-skill", "org", None, "all"),
            ("a-skill", "team", Some("tA"), "all"),
            ("b-skill", "team", Some("tB"), "all"),
            ("secret-skill", "org", None, "admin"),
        ] {
            svc.upsert_skill(None, name, "", "", true, false, scope, team, vis, "admin1")
                .await
                .unwrap();
        }
        for (name, scope, team, vis) in [
            ("org-mcp", "org", None, "all"),
            ("a-mcp", "team", Some("tA"), "all"),
            ("b-mcp", "team", Some("tB"), "all"),
            ("secret-mcp", "org", None, "admin"),
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
                "admin1",
            )
            .await
            .unwrap();
        }
        for (title, scope, team, vis) in [
            ("org-doc", "org", None, "all"),
            ("a-doc", "team", Some("tA"), "all"),
            ("b-doc", "team", Some("tB"), "all"),
            ("secret-doc", "org", None, "admin"),
        ] {
            svc.register_rag_document(title, None, None, None, scope, team, vis, "admin1")
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
        for (title, scope, team, vis) in [
            ("org-doc", "org", None, "all"),
            ("a-doc", "team", Some("tA"), "all"),
            ("b-doc", "team", Some("tB"), "all"),
            ("secret-doc", "org", None, "admin"),
        ] {
            let doc = svc
                .register_rag_document(title, None, None, None, scope, team, vis, "admin1")
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
        svc.delete_skill(&skill.id).await.unwrap();
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
        svc.delete_mcp_registry(&mcp.id).await.unwrap();

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
        svc.delete_rag_document(&doc.id).await.unwrap();
        assert!(svc.list_rag_documents("u1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn milestone_crud_and_requirement_link_clearing() {
        let svc = service().await;
        let m = svc
            .create_milestone(
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
            .update_milestone(&m.id, Some("v1.0 GA"), Some(None), Some("completed"), None)
            .await
            .unwrap();
        assert_eq!(m.title, "v1.0 GA");
        assert_eq!(m.status, "completed");
        assert!(m.description.is_none());
        assert_eq!(m.due_at, Some(1_800_000_000_000));

        let err = svc
            .update_milestone(&m.id, None, None, Some("bogus"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // A requirement pointing at the milestone gets its link cleared on delete.
        let req = svc
            .create_requirement(
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

        svc.delete_milestone(&m.id).await.unwrap();
        assert!(svc.list_milestones().await.unwrap().is_empty());
        let tree = svc.requirements_tree().await.unwrap();
        assert_eq!(tree[0].milestone_id, None);

        let err = svc.delete_milestone("missing").await.unwrap_err();
        assert!(matches!(err, DevopsError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_plan_and_case_crud() {
        let svc = service().await;

        // Create plan
        let plan = svc
            .create_test_plan("u1", Some("Alice"), "登录冒烟测试", Some("覆盖 SSO 和密码登录"), None)
            .await
            .unwrap();
        assert_eq!(plan.status, "draft");
        assert_eq!(svc.list_test_plans().await.unwrap().len(), 1);

        // Update plan
        let plan = svc
            .update_test_plan(&plan.id, Some("登录回归测试"), None, Some("active"), None)
            .await
            .unwrap();
        assert_eq!(plan.status, "active");

        // Create cases
        let c1 = svc
            .create_test_case(&plan.id, "u1", Some("Alice"), "密码登录成功", None, None, None)
            .await
            .unwrap();
        assert_eq!(c1.status, "pending");
        let c2 = svc
            .create_test_case(&plan.id, "u1", Some("Alice"), "错误密码被拒", None, None, None)
            .await
            .unwrap();

        let cases = svc.list_test_cases(&plan.id).await.unwrap();
        assert_eq!(cases.len(), 2);

        // Update case status
        let c1 = svc
            .update_test_case(&c1.id, None, Some("passed"), None, None, None)
            .await
            .unwrap();
        assert_eq!(c1.status, "passed");

        let err = svc
            .update_test_case(&c2.id, None, Some("bogus"), None, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // Delete case
        svc.delete_test_case(&c2.id).await.unwrap();
        assert_eq!(svc.list_test_cases(&plan.id).await.unwrap().len(), 1);

        // Delete plan cascades to remaining cases
        svc.delete_test_plan(&plan.id).await.unwrap();
        assert!(svc.list_test_plans().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pipeline_and_run_crud() {
        let svc = service().await;

        // Create pipeline
        let pipe = svc
            .create_pipeline(
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
        assert_eq!(svc.list_pipelines().await.unwrap().len(), 1);

        // Update pipeline
        let pipe = svc
            .update_pipeline(&pipe.id, None, None, Some("disabled"), None)
            .await
            .unwrap();
        assert_eq!(pipe.status, "disabled");

        let err = svc
            .update_pipeline(&pipe.id, None, None, Some("bad"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // Create run
        let run = svc.create_pipeline_run(&pipe.id, Some("u1")).await.unwrap();
        assert_eq!(run.status, "pending");

        let run = svc
            .update_pipeline_run(&run.id, Some("running"), Some(Some(1_800_000_000_000)), None, None)
            .await
            .unwrap();
        assert_eq!(run.status, "running");
        assert_eq!(run.started_at, Some(1_800_000_000_000));

        let run = svc
            .update_pipeline_run(
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

        let runs = svc.list_pipeline_runs(&pipe.id).await.unwrap();
        assert_eq!(runs.len(), 1);

        // Delete pipeline cascades to runs
        svc.delete_pipeline(&pipe.id).await.unwrap();
        assert!(svc.list_pipelines().await.unwrap().is_empty());
    }
}
