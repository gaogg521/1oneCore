//! one-devops row types + wire DTOs (camelCase, matching the other one-*
//! crates' convention).

use serde::Serialize;
use sqlx::FromRow;

pub const REQUIREMENT_TYPES: &[&str] = &["epic", "feature", "story", "bug", "task"];
pub const REQUIREMENT_STATUSES: &[&str] = &["backlog", "planning", "developing", "testing", "completed"];
pub const REQUIREMENT_PRIORITIES: &[&str] = &["low", "medium", "high", "urgent"];
pub const MILESTONE_STATUSES: &[&str] = &["active", "completed", "archived"];
pub const TEST_PLAN_STATUSES: &[&str] = &["draft", "active", "completed", "archived"];
pub const TEST_CASE_STATUSES: &[&str] = &["pending", "passed", "failed", "blocked", "skipped"];
pub const PIPELINE_STATUSES: &[&str] = &["active", "disabled"];
pub const PIPELINE_TRIGGERS: &[&str] = &["manual", "push", "schedule"];
pub const PIPELINE_RUN_STATUSES: &[&str] = &["pending", "running", "success", "failed", "cancelled"];

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
    pub autopilot: bool,
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
    pub autopilot: bool,
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
            autopilot: row.autopilot,
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
    /// Mixed distribution model: `true` = member agents load this skill
    /// automatically (admin-required); `false` = member opts in per assistant.
    pub auto_active: bool,
    pub scope: String,
    pub team_id: Option<String>,
    /// Read visibility (P0-4): `'all'` = every member in scope; `'admin'` =
    /// org/system admins only.
    pub visibility: String,
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
    /// stdio `env` / sse `headers` JSON object, distributed to members so the
    /// connector actually authenticates locally (D5). May be null.
    pub secrets_json: Option<String>,
    pub scope: String,
    pub team_id: Option<String>,
    /// Read visibility (P0-4): `'all'` | `'admin'`.
    pub visibility: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A company-provisioned model channel, as seen by anyone.
///
/// ⚠️ There is deliberately **no field for the credential**. The real API key
/// lives encrypted in `one_provider_registry.api_key_encrypted` and is decrypted
/// only inside the model proxy; it must never reach a member's machine, and the
/// cheapest way to guarantee that is for the type carrying channels around to
/// have nowhere to put it. `hasKey` says whether one is configured, which is
/// all an admin UI needs to render.
#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderChannelDto {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// Where the proxy forwards to. Visible to admins in the console; it is not
    /// a secret, and a member needs it for nothing (they talk to the proxy).
    pub upstream_base_url: String,
    pub has_key: bool,
    /// JSON array of model names offered on this channel.
    pub models: String,
    /// JSON object of per-model settings, same shape as `providers.model_settings`.
    pub model_settings: Option<String>,
    pub enabled: bool,
    pub scope: String,
    pub team_id: Option<String>,
    /// Read visibility (P0-4): `'all'` | `'admin'`.
    pub visibility: String,
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
    /// Read visibility (P0-4): `'all'` | `'admin'`.
    pub visibility: String,
    pub created_by: String,
    pub created_at: i64,
}

// -- test plans -----------------------------------------------------------

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestPlanDto {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub requirement_id: Option<String>,
    pub creator_id: String,
    pub creator_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCaseDto {
    pub id: String,
    pub plan_id: String,
    pub title: String,
    pub description: Option<String>,
    pub steps: Option<String>,
    pub expected: Option<String>,
    pub status: String,
    pub creator_id: String,
    pub creator_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

// -- pipelines ------------------------------------------------------------

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub trigger: String,
    pub creator_id: String,
    pub creator_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineRunDto {
    pub id: String,
    pub pipeline_id: String,
    pub status: String,
    pub triggered_by: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub log: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// RAG embedding config as returned to the UI. The api_key is never echoed;
/// `has_key` reports whether one is stored.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RagConfigDto {
    pub base_url: String,
    pub model: String,
    pub has_key: bool,
    pub dimensions: Option<i64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RagSearchHit {
    pub document_id: String,
    pub document_title: String,
    pub chunk_index: i64,
    pub content: String,
    pub score: f32,
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
