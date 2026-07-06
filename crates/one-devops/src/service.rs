//! one-devops service — requirements board + collaboration registries.

use std::collections::HashMap;

use sqlx::SqlitePool;

use aionui_common::now_ms;

use crate::error::DevopsError;
use crate::embedding::EmbeddingConfig;
use crate::models::{
    McpRegistryDto, MILESTONE_STATUSES, MilestoneDto, PIPELINE_RUN_STATUSES, PIPELINE_STATUSES, PIPELINE_TRIGGERS,
    PipelineDto, PipelineRunDto, RagConfigDto, RagDocumentDto, RagSearchHit, REQUIREMENT_PRIORITIES, REQUIREMENT_STATUSES,
    REQUIREMENT_TYPES, RequirementCommentDto, RequirementDto, RequirementRow, SkillRegistryDto, TEST_CASE_STATUSES,
    TEST_PLAN_STATUSES, TestCaseDto, TestPlanDto,
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
    format!("{prefix}_{}", &uuid::Uuid::now_v7().simple().to_string()[..12])
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
                .create_requirement(creator_id, creator_name, CreateRequirementInput {
                    parent_id: Some(parent_id.to_owned()),
                    kind: Some(item.kind.clone()),
                    subject: item.subject.clone(),
                    description: item.description.clone(),
                    priority: Some(item.priority.clone()),
                    milestone_id: None,
                    autopilot: None,
                })
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

    // -- skill registry ---------------------------------------------------

    pub async fn list_skills(&self) -> Result<Vec<SkillRegistryDto>, DevopsError> {
        Ok(sqlx::query_as::<_, SkillRegistryDto>(
            "SELECT id, name, description, content, enabled, scope, team_id, created_by, created_at, updated_at \
             FROM one_skill_registry ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn upsert_skill(
        &self,
        id: Option<&str>,
        name: &str,
        description: &str,
        content: &str,
        enabled: bool,
        created_by: &str,
    ) -> Result<SkillRegistryDto, DevopsError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DevopsError::BadRequest("name is required".into()));
        }
        let now = now_ms();
        let id = match id {
            Some(existing) => {
                let updated = sqlx::query(
                    "UPDATE one_skill_registry SET name = ?, description = ?, content = ?, enabled = ?, updated_at = ? WHERE id = ?",
                )
                .bind(name)
                .bind(description)
                .bind(content)
                .bind(enabled)
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
                        (id, name, description, content, enabled, scope, team_id, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 'org', NULL, ?, ?, ?)",
                )
                .bind(&id)
                .bind(name)
                .bind(description)
                .bind(content)
                .bind(enabled)
                .bind(created_by)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                id
            }
        };
        sqlx::query_as::<_, SkillRegistryDto>(
            "SELECT id, name, description, content, enabled, scope, team_id, created_by, created_at, updated_at \
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

    pub async fn list_mcp_registry(&self) -> Result<Vec<McpRegistryDto>, DevopsError> {
        Ok(sqlx::query_as::<_, McpRegistryDto>(
            "SELECT id, name, type, endpoint, enabled, has_keys, scope, team_id, created_by, created_at, updated_at \
             FROM one_mcp_registry ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
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
        created_by: &str,
    ) -> Result<McpRegistryDto, DevopsError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DevopsError::BadRequest("name is required".into()));
        }
        if !matches!(r#type, "stdio" | "sse") {
            return Err(DevopsError::BadRequest(format!("invalid type: {type} (allowed: stdio/sse)", r#type = r#type)));
        }
        let now = now_ms();
        let id = match id {
            Some(existing) => {
                let updated = sqlx::query(
                    "UPDATE one_mcp_registry SET name = ?, type = ?, endpoint = ?, enabled = ?, has_keys = ?, updated_at = ? WHERE id = ?",
                )
                .bind(name)
                .bind(r#type)
                .bind(endpoint)
                .bind(enabled)
                .bind(has_keys)
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
                        (id, name, type, endpoint, enabled, has_keys, scope, team_id, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, 'org', NULL, ?, ?, ?)",
                )
                .bind(&id)
                .bind(name)
                .bind(r#type)
                .bind(endpoint)
                .bind(enabled)
                .bind(has_keys)
                .bind(created_by)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                id
            }
        };
        sqlx::query_as::<_, McpRegistryDto>(
            "SELECT id, name, type, endpoint, enabled, has_keys, scope, team_id, created_by, created_at, updated_at \
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

    pub async fn list_rag_documents(&self) -> Result<Vec<RagDocumentDto>, DevopsError> {
        Ok(sqlx::query_as::<_, RagDocumentDto>(
            "SELECT id, title, file_path, file_size, mime_type, status, last_error, chunk_count, \
                    scope, team_id, created_by, created_at \
             FROM one_rag_documents ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn register_rag_document(
        &self,
        title: &str,
        file_path: Option<&str>,
        file_size: Option<i64>,
        mime_type: Option<&str>,
        created_by: &str,
    ) -> Result<RagDocumentDto, DevopsError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(DevopsError::BadRequest("title is required".into()));
        }
        let id = new_id("orag");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_rag_documents \
                (id, title, file_path, file_size, mime_type, status, last_error, chunk_count, scope, team_id, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', NULL, 0, 'org', NULL, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(file_path)
        .bind(file_size)
        .bind(mime_type)
        .bind(created_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query_as::<_, RagDocumentDto>(
            "SELECT id, title, file_path, file_size, mime_type, status, last_error, chunk_count, \
                    scope, team_id, created_by, created_at \
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
            None => RagConfigDto { base_url: String::new(), model: String::new(), has_key: false, dimensions: None, updated_at: 0 },
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
        let (base_url, api_key, model) = row.ok_or_else(|| {
            DevopsError::BadRequest("RAG embedding endpoint not configured".into())
        })?;
        Ok(EmbeddingConfig { base_url, api_key, model })
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
    pub async fn search_rag(&self, query: &str, top_k: usize) -> Result<Vec<RagSearchHit>, DevopsError> {
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

        let rows: Vec<(String, i64, String, Vec<u8>, String)> = sqlx::query_as(
            "SELECT c.document_id, c.chunk_index, c.content, c.embedding, d.title \
             FROM one_rag_chunks c JOIN one_rag_documents d ON d.id = c.document_id",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut hits: Vec<RagSearchHit> = rows
            .into_iter()
            .map(|(document_id, chunk_index, content, blob, document_title)| {
                let score = crate::embedding::cosine_similarity(&query_vec, &crate::embedding::unpack_embedding(&blob));
                RagSearchHit { document_id, document_title, chunk_index, content, score }
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
    async fn requirement_crud_and_tree_nesting() {
        let svc = service().await;
        let epic = svc
            .create_requirement("u1", Some("Alice"), CreateRequirementInput {
                kind: Some("epic".into()),
                subject: "Big epic".into(),
                priority: Some("high".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let story = svc
            .create_requirement("u1", Some("Alice"), CreateRequirementInput {
                parent_id: Some(epic.id.clone()),
                kind: Some("story".into()),
                subject: "Child story".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let tree = svc.requirements_tree().await.unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].id, epic.id);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].id, story.id);

        svc.update_requirement(&story.id, UpdateRequirementInput {
            status: Some("developing".into()),
            assigned_to: Some(Some("agent-1".into())),
            ..Default::default()
        })
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
            .create_requirement("u1", Some("Alice"), CreateRequirementInput {
                subject: "auto".into(),
                autopilot: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(req.autopilot);
        // Default is off.
        let plain = svc
            .create_requirement("u1", None, CreateRequirementInput { subject: "manual".into(), ..Default::default() })
            .await
            .unwrap();
        assert!(!plain.autopilot);

        // Toggling other fields leaves autopilot untouched; explicit toggle flips it.
        svc.update_requirement(&req.id, UpdateRequirementInput {
            priority: Some("high".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        svc.update_requirement(&req.id, UpdateRequirementInput { autopilot: Some(false), ..Default::default() })
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
            .create_requirement("u1", None, CreateRequirementInput {
                subject: "  ".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        let req = svc
            .create_requirement("u1", None, CreateRequirementInput {
                subject: "ok".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let err = svc
            .update_requirement(&req.id, UpdateRequirementInput {
                status: Some("nonsense".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
        let err = svc
            .update_requirement(&req.id, UpdateRequirementInput {
                parent_id: Some(Some(req.id.clone())),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
    }

    #[tokio::test]
    async fn comments_roundtrip() {
        let svc = service().await;
        let req = svc
            .create_requirement("u1", Some("Alice"), CreateRequirementInput {
                subject: "with comments".into(),
                ..Default::default()
            })
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
    async fn registries_crud() {
        let svc = service().await;

        let skill = svc.upsert_skill(None, "review", "code review", "...", true, "u1").await.unwrap();
        let skill = svc
            .upsert_skill(Some(&skill.id), "review", "better desc", "...", false, "u1")
            .await
            .unwrap();
        assert!(!skill.enabled);
        assert_eq!(svc.list_skills().await.unwrap().len(), 1);
        svc.delete_skill(&skill.id).await.unwrap();
        assert!(svc.list_skills().await.unwrap().is_empty());

        let mcp = svc
            .upsert_mcp_registry(None, "search", "sse", "https://mcp.corp/sse", true, true, "u1")
            .await
            .unwrap();
        assert!(mcp.has_keys);
        assert_eq!(svc.list_mcp_registry().await.unwrap().len(), 1);
        let err = svc
            .upsert_mcp_registry(None, "bad", "ws", "", true, false, "u1")
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));
        svc.delete_mcp_registry(&mcp.id).await.unwrap();

        let doc = svc
            .register_rag_document("handbook.pdf", Some("/data/handbook.pdf"), Some(1024), Some("application/pdf"), "u1")
            .await
            .unwrap();
        assert_eq!(doc.status, "pending");
        assert_eq!(svc.list_rag_documents().await.unwrap().len(), 1);
        svc.delete_rag_document(&doc.id).await.unwrap();
        assert!(svc.list_rag_documents().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn milestone_crud_and_requirement_link_clearing() {
        let svc = service().await;
        let m = svc
            .create_milestone("u1", Some("Alice"), "v1.0 发布", Some("首个灰度"), Some(1_800_000_000_000))
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

        let err = svc.update_milestone(&m.id, None, None, Some("bogus"), None).await.unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)));

        // A requirement pointing at the milestone gets its link cleared on delete.
        let req = svc
            .create_requirement("u1", Some("Alice"), CreateRequirementInput {
                subject: "linked".into(),
                milestone_id: Some(m.id.clone()),
                ..Default::default()
            })
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
        let plan = svc.update_test_plan(&plan.id, Some("登录回归测试"), None, Some("active"), None).await.unwrap();
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
        let c1 = svc.update_test_case(&c1.id, None, Some("passed"), None, None, None).await.unwrap();
        assert_eq!(c1.status, "passed");

        let err = svc.update_test_case(&c2.id, None, Some("bogus"), None, None, None).await.unwrap_err();
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
            .create_pipeline("u1", Some("Alice"), "CI 主流水线", Some("main 分支推送触发"), Some("push"))
            .await
            .unwrap();
        assert_eq!(pipe.status, "active");
        assert_eq!(pipe.trigger, "push");
        assert_eq!(svc.list_pipelines().await.unwrap().len(), 1);

        // Update pipeline
        let pipe = svc.update_pipeline(&pipe.id, None, None, Some("disabled"), None).await.unwrap();
        assert_eq!(pipe.status, "disabled");

        let err = svc.update_pipeline(&pipe.id, None, None, Some("bad"), None).await.unwrap_err();
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
