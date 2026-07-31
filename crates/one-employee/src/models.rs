//! Row types and API DTOs for one-employee.

use aionui_common::ProviderWithModel;
use serde::Serialize;

pub const TRIGGER_MANUAL: &str = "manual";
pub const TRIGGER_CRON: &str = "cron";
/// A blocking run driven by devops breakdown (agent asked to split an
/// epic/feature into child requirements). Distinguished in run history.
pub const TRIGGER_BREAKDOWN: &str = "breakdown";

pub const RUN_RUNNING: &str = "running";
pub const RUN_SUCCESS: &str = "success";
pub const RUN_FAILED: &str = "failed";

/// Digital employee definition (mirror of 1ONE `personal_agents`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PersonalAgentRow {
    pub id: String,
    pub owner_user_id: String,
    pub tenant_id: String,
    pub name: String,
    pub description: Option<String>,
    /// The *effective* backend ("claude", "aionrs", …). Still the gate that
    /// decides whether a top-level conversation model may be sent (aionrs-only).
    pub agent_type: String,
    /// Legacy column, kept only as a fallback source for `assistant_id`
    /// (mirrors `CronAgentConfig`'s handling of the same legacy field).
    pub custom_agent_id: Option<String>,
    pub cli_path: Option<String>,
    /// Persona / assistant definition id. `None` keeps the pre-004
    /// backend-only behaviour.
    pub assistant_id: Option<String>,
    /// `agent_metadata.id` to run the persona under when the user manually
    /// overrode the backend the persona would otherwise imply.
    pub agent_id_override: Option<String>,
    /// Plain model id, for ACP backends.
    pub model_id: Option<String>,
    /// `ProviderWithModel` JSON, for aionrs.
    pub model: Option<String>,
    pub automation_config: String,
    pub schedule: Option<String>,
    pub schedule_enabled: i64,
    pub next_run_at: Option<i64>,
    /// 'private' (owner-only) or 'shared' (usable by any same-tenant member).
    pub visibility: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonalAgentDto {
    pub id: String,
    pub owner_user_id: String,
    pub tenant_id: String,
    pub name: String,
    pub description: Option<String>,
    pub agent_type: String,
    pub custom_agent_id: Option<String>,
    pub cli_path: Option<String>,
    pub assistant_id: Option<String>,
    pub agent_id_override: Option<String>,
    pub model_id: Option<String>,
    /// Parsed back into a struct rather than left as an opaque JSON string.
    /// Note `ProviderWithModel` has no `rename_all`, so its own fields stay
    /// snake_case (`provider_id` / `use_model`) even inside this camelCase DTO
    /// — same wire shape the frontend already consumes for cron jobs.
    pub model: Option<ProviderWithModel>,
    pub automation_config: serde_json::Value,
    pub schedule: Option<serde_json::Value>,
    pub schedule_enabled: bool,
    pub next_run_at: Option<i64>,
    pub visibility: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<PersonalAgentRow> for PersonalAgentDto {
    fn from(row: PersonalAgentRow) -> Self {
        let automation_config = serde_json::from_str(&row.automation_config).unwrap_or_else(|_| serde_json::json!({}));
        let schedule = row
            .schedule
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        // An unparseable model column degrades to `None` rather than failing the
        // whole listing; the run path treats that as "no model selected" and the
        // create/update guard prevents writing one in the first place.
        let model = row
            .model
            .as_deref()
            .and_then(|s| serde_json::from_str::<ProviderWithModel>(s).ok());
        Self {
            id: row.id,
            owner_user_id: row.owner_user_id,
            tenant_id: row.tenant_id,
            name: row.name,
            description: row.description,
            agent_type: row.agent_type,
            custom_agent_id: row.custom_agent_id,
            cli_path: row.cli_path,
            assistant_id: row.assistant_id,
            agent_id_override: row.agent_id_override,
            model_id: row.model_id,
            model,
            automation_config,
            schedule,
            schedule_enabled: row.schedule_enabled != 0,
            next_run_at: row.next_run_at,
            visibility: row.visibility,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// One digital-employee execution (structured replacement for the legacy
/// `automationConfig.runHistory` JSON blob).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmployeeRunRow {
    pub id: String,
    pub agent_id: String,
    pub owner_user_id: String,
    pub tenant_id: String,
    pub team_id: Option<String>,
    pub slot_id: Option<String>,
    pub conversation_id: String,
    pub turn_id: Option<String>,
    pub status: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub trigger_source: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}
