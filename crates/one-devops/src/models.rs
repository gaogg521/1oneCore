//! one-devops row types + wire DTOs (camelCase, matching the other one-*
//! crates' convention).

use serde::Serialize;
use sqlx::FromRow;

pub const REQUIREMENT_TYPES: &[&str] = &["epic", "feature", "story", "bug", "task"];
pub const REQUIREMENT_STATUSES: &[&str] = &["backlog", "planning", "developing", "testing", "completed"];
pub const REQUIREMENT_PRIORITIES: &[&str] = &["low", "medium", "high", "urgent"];
pub const MILESTONE_STATUSES: &[&str] = &["active", "completed", "archived"];

#[derive(Debug, Clone, FromRow)]
pub struct RequirementRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub r#type: String,
    pub subject: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub milestone_id: Option<String>,
    pub creator_id: String,
    pub creator_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementDto {
    pub id: String,
    pub parent_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub subject: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub milestone_id: Option<String>,
    pub creator_id: String,
    pub creator_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub children: Vec<RequirementDto>,
}

impl RequirementDto {
    pub fn from_row(row: RequirementRow) -> Self {
        Self {
            id: row.id,
            parent_id: row.parent_id,
            kind: row.r#type,
            subject: row.subject,
            description: row.description,
            status: row.status,
            priority: row.priority,
            assigned_to: row.assigned_to,
            milestone_id: row.milestone_id,
            creator_id: row.creator_id,
            creator_name: row.creator_name,
            created_at: row.created_at,
            updated_at: row.updated_at,
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementCommentDto {
    pub id: String,
    pub requirement_id: String,
    pub author_type: String,
    pub author_id: Option<String>,
    pub author_name: String,
    pub body: String,
    pub metadata: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRegistryDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub enabled: bool,
    pub scope: String,
    pub team_id: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRegistryDto {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub endpoint: String,
    pub enabled: bool,
    pub has_keys: bool,
    pub scope: String,
    pub team_id: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RagDocumentDto {
    pub id: String,
    pub title: String,
    pub file_path: Option<String>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
    pub status: String,
    pub last_error: Option<String>,
    pub chunk_count: i64,
    pub scope: String,
    pub team_id: Option<String>,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MilestoneDto {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub due_at: Option<i64>,
    pub creator_id: String,
    pub creator_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
