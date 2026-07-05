//! one-devops service — requirements board + collaboration registries.

use std::collections::HashMap;

use sqlx::SqlitePool;

use aionui_common::now_ms;

use crate::error::DevopsError;
use crate::models::{
    McpRegistryDto, RagDocumentDto, REQUIREMENT_PRIORITIES, REQUIREMENT_STATUSES, REQUIREMENT_TYPES,
    RequirementCommentDto, RequirementDto, RequirementRow, SkillRegistryDto,
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
                    milestone_id, creator_id, creator_name, created_at, updated_at \
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
                 milestone_id, creator_id, creator_name, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'backlog', ?, NULL, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.parent_id)
        .bind(kind)
        .bind(subject)
        .bind(&input.description)
        .bind(priority)
        .bind(&input.milestone_id)
        .bind(creator_id)
        .bind(creator_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(RequirementDto::from_row(self.fetch_requirement(&id).await?))
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
                    milestone_id, creator_id, creator_name, created_at, updated_at \
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
        let deleted = sqlx::query("DELETE FROM one_rag_documents WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("rag document {id}")));
        }
        Ok(())
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
}
