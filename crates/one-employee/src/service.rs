//! Digital employee orchestration service.
//!
//! Translation of the 1ONE ClaudeCode reference
//! (`src/process/digitalEmployee/DigitalEmployeeRunService.ts`), rebuilt on
//! upstream in-process primitives instead of HTTP-to-self:
//!
//! - conversation creation → `ConversationService::create` (same call the
//!   aionui-cron `JobExecutor` makes for `ExecutionMode::NewConversation`)
//! - "wait until idle" (cronBusyGuard) → `ConversationService::run_agent_turn`,
//!   which awaits the whole turn and reports Completed/Failed directly
//! - runHistory JSON blob → `one_employee_runs` table

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

use aionui_ai_agent::AgentRegistry;
use aionui_api_types::CreateConversationRequest;
use aionui_common::{AgentType, now_ms};
use aionui_conversation::{ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationService};
use aionui_db::{ConversationRowUpdate, IConversationRepository};

use crate::error::EmployeeError;
use crate::models::{
    EmployeeRunRow, PersonalAgentDto, PersonalAgentRow, RUN_FAILED, RUN_RUNNING, RUN_SUCCESS, TRIGGER_MANUAL,
};

pub struct EmployeeService {
    pool: SqlitePool,
    conversation_service: Arc<ConversationService>,
    conversation_repo: Arc<dyn IConversationRepository>,
    agent_registry: Arc<AgentRegistry>,
    work_dir: PathBuf,
}

pub struct CreateEmployeeInput {
    pub name: String,
    pub description: Option<String>,
    pub agent_type: String,
    pub custom_agent_id: Option<String>,
    pub cli_path: Option<String>,
    pub automation_config: Option<serde_json::Value>,
}

pub struct UpdateEmployeeInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub automation_config: Option<serde_json::Value>,
}

fn short_id(prefix: &str) -> String {
    let uuid = uuid::Uuid::now_v7().simple().to_string();
    format!("{prefix}_{}", &uuid[..12])
}

/// `MM/DD HH:mm` (UTC) — same run-conversation naming shape as the TS
/// reference. The name is cosmetic; exact local-time parity is not
/// load-bearing, so we avoid a chrono dependency.
fn format_run_timestamp(now_ms_value: i64) -> String {
    fn leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
    let secs = now_ms_value / 1000;
    let mut rem_days = secs / 86_400;
    let mut year = 1970i64;
    loop {
        let year_days = if leap(year) { 366 } else { 365 };
        if rem_days < year_days {
            break;
        }
        rem_days -= year_days;
        year += 1;
    }
    let month_lengths = [31, if leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1u32;
    for len in month_lengths {
        if rem_days < len {
            break;
        }
        rem_days -= len;
        month += 1;
    }
    let day = rem_days + 1;
    let day_secs = secs % 86_400;
    let (hour, minute) = (day_secs / 3600, (day_secs % 3600) / 60);
    format!("{month:02}/{day:02} {hour:02}:{minute:02}")
}

/// Prompt for a manual/cron run without a bound issue — mirrors the
/// instructions-first fallback chain of `buildPersonalDigitalEmployeeCronPrompt`.
fn build_run_prompt(agent: &PersonalAgentRow) -> String {
    let config: serde_json::Value = serde_json::from_str(&agent.automation_config).unwrap_or_default();
    let instructions = config
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(instructions) = instructions {
        return instructions.to_owned();
    }
    if let Some(description) = agent.description.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return format!("你是「{}」。你的职责：{}\n\n请立即执行你的日常职责，完成后输出可交付摘要。", agent.name, description);
    }
    format!("你是「{}」。请立即执行你的日常职责，完成后输出可交付摘要。", agent.name)
}

impl EmployeeService {
    pub fn new(
        pool: SqlitePool,
        conversation_service: Arc<ConversationService>,
        conversation_repo: Arc<dyn IConversationRepository>,
        agent_registry: Arc<AgentRegistry>,
        work_dir: PathBuf,
    ) -> Self {
        Self {
            pool,
            conversation_service,
            conversation_repo,
            agent_registry,
            work_dir,
        }
    }

    /// Same resolution chain as the cron executor's `parse_agent_type` +
    /// `inject_agent_identity`: native serde names (acp/aionrs/…) pass
    /// through; backend labels ("claude", "gemini", …) resolve through the
    /// agent registry to `Acp` plus an `agent_id`/`backend` identity in extra.
    async fn resolve_agent_identity(
        &self,
        agent_type_str: &str,
        extra: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<AgentType, EmployeeError> {
        if let Some(meta) = self.agent_registry.find_builtin_by_backend(agent_type_str).await {
            extra.insert("agent_id".into(), serde_json::Value::String(meta.id.clone()));
            if let Some(backend) = meta.backend {
                extra.insert("backend".into(), serde_json::Value::String(backend));
            }
            return Ok(AgentType::Acp);
        }
        serde_json::from_value::<AgentType>(serde_json::Value::String(agent_type_str.to_owned()))
            .map_err(|_| EmployeeError::BadRequest(format!("unknown agent type: {agent_type_str}")))
    }

    // --- CRUD ---

    pub async fn list(&self, owner_user_id: &str) -> Result<Vec<PersonalAgentDto>, EmployeeError> {
        let rows = sqlx::query_as::<_, PersonalAgentRow>(
            "SELECT * FROM one_personal_agents WHERE owner_user_id = ? ORDER BY updated_at DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get(&self, owner_user_id: &str, agent_id: &str) -> Result<PersonalAgentRow, EmployeeError> {
        sqlx::query_as::<_, PersonalAgentRow>(
            "SELECT * FROM one_personal_agents WHERE id = ? AND owner_user_id = ?",
        )
        .bind(agent_id)
        .bind(owner_user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(EmployeeError::NotFound)
    }

    pub async fn create(
        &self,
        owner_user_id: &str,
        tenant_id: &str,
        input: CreateEmployeeInput,
    ) -> Result<PersonalAgentDto, EmployeeError> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err(EmployeeError::BadRequest("name is required".into()));
        }
        if input.agent_type.trim().is_empty() {
            return Err(EmployeeError::BadRequest("agentType is required".into()));
        }
        let automation_config = input
            .automation_config
            .unwrap_or_else(|| serde_json::json!({}))
            .to_string();

        let id = short_id("pa");
        let now = now_ms() as i64;
        sqlx::query(
            "INSERT INTO one_personal_agents \
             (id, owner_user_id, tenant_id, name, description, agent_type, custom_agent_id, cli_path, \
              automation_config, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(owner_user_id)
        .bind(tenant_id)
        .bind(name)
        .bind(&input.description)
        .bind(input.agent_type.trim())
        .bind(&input.custom_agent_id)
        .bind(&input.cli_path)
        .bind(&automation_config)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(self.get(owner_user_id, &id).await?.into())
    }

    pub async fn update(
        &self,
        owner_user_id: &str,
        agent_id: &str,
        input: UpdateEmployeeInput,
    ) -> Result<PersonalAgentDto, EmployeeError> {
        let existing = self.get(owner_user_id, agent_id).await?;
        let name = match input.name.as_deref().map(str::trim) {
            Some("") => return Err(EmployeeError::BadRequest("name must not be empty".into())),
            Some(name) => name.to_owned(),
            None => existing.name,
        };
        let description = input.description.or(existing.description);
        let automation_config = input
            .automation_config
            .map(|v| v.to_string())
            .unwrap_or(existing.automation_config);

        sqlx::query(
            "UPDATE one_personal_agents SET name = ?, description = ?, automation_config = ?, updated_at = ? \
             WHERE id = ? AND owner_user_id = ?",
        )
        .bind(&name)
        .bind(&description)
        .bind(&automation_config)
        .bind(now_ms() as i64)
        .bind(agent_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;

        Ok(self.get(owner_user_id, agent_id).await?.into())
    }

    pub async fn delete(&self, owner_user_id: &str, agent_id: &str) -> Result<(), EmployeeError> {
        let result = sqlx::query("DELETE FROM one_personal_agents WHERE id = ? AND owner_user_id = ?")
            .bind(agent_id)
            .bind(owner_user_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(EmployeeError::NotFound);
        }
        Ok(())
    }

    // --- runs ---

    pub async fn list_runs(&self, owner_user_id: &str, agent_id: &str) -> Result<Vec<EmployeeRunRow>, EmployeeError> {
        // Ownership check first so foreign agents 404 instead of listing empty.
        self.get(owner_user_id, agent_id).await?;
        let rows = sqlx::query_as::<_, EmployeeRunRow>(
            "SELECT * FROM one_employee_runs WHERE agent_id = ? ORDER BY started_at DESC LIMIT 50",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_run(&self, owner_user_id: &str, run_id: &str) -> Result<EmployeeRunRow, EmployeeError> {
        sqlx::query_as::<_, EmployeeRunRow>(
            "SELECT * FROM one_employee_runs WHERE id = ? AND owner_user_id = ?",
        )
        .bind(run_id)
        .bind(owner_user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(EmployeeError::RunNotFound)
    }

    /// Manual "run now": create a fresh conversation, fire the run prompt as a
    /// hidden turn in the background, record the outcome in
    /// `one_employee_runs`. Returns immediately with `{run_id, conversation_id}`.
    pub async fn run_now(
        self: &Arc<Self>,
        owner_user_id: &str,
        agent_id: &str,
    ) -> Result<(String, String), EmployeeError> {
        let agent = self.get(owner_user_id, agent_id).await?;

        let mut extra = serde_json::Map::new();
        extra.insert("one_employee_id".into(), serde_json::Value::String(agent.id.clone()));
        extra.insert(
            "one_employee_owner".into(),
            serde_json::Value::String(agent.owner_user_id.clone()),
        );
        if let Some(cli_path) = agent.cli_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            extra.insert("cli_path".into(), serde_json::Value::String(cli_path.to_owned()));
        }
        let agent_type = self.resolve_agent_identity(&agent.agent_type, &mut extra).await?;

        let now = now_ms() as i64;
        let conversation_name = format!("{} - {}", agent.name, format_run_timestamp(now));
        let req = CreateConversationRequest {
            r#type: Some(agent_type),
            name: Some(conversation_name),
            model: None,
            assistant: None,
            source: None,
            channel_chat_id: None,
            extra: serde_json::Value::Object(extra),
        };
        let response = self
            .conversation_service
            .create(owner_user_id, req)
            .await
            .map_err(|e| EmployeeError::Internal(format!("create conversation: {e}")))?;
        let conversation_id = response.id.clone();

        self.ensure_workspace(&conversation_id, &response.extra).await?;

        let run_id = short_id("run");
        sqlx::query(
            "INSERT INTO one_employee_runs \
             (id, agent_id, owner_user_id, tenant_id, conversation_id, status, trigger_source, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run_id)
        .bind(&agent.id)
        .bind(owner_user_id)
        .bind(&agent.tenant_id)
        .bind(&conversation_id)
        .bind(RUN_RUNNING)
        .bind(TRIGGER_MANUAL)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let service = Arc::clone(self);
        let owner = owner_user_id.to_owned();
        let run_id_bg = run_id.clone();
        let conversation_id_bg = conversation_id.clone();
        tokio::spawn(async move {
            service
                .execute_run(&owner, &agent, &run_id_bg, &conversation_id_bg)
                .await;
        });

        Ok((run_id, conversation_id))
    }

    /// Mirror of the cron executor's fallback: some conversation types come
    /// back without a provisioned workspace; the agent turn needs one.
    async fn ensure_workspace(
        &self,
        conversation_id: &str,
        extra: &serde_json::Value,
    ) -> Result<(), EmployeeError> {
        let workspace = extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default();
        if !workspace.is_empty() {
            return Ok(());
        }

        let fallback = self
            .work_dir
            .join("conversations")
            .join(format!("one-employee-{conversation_id}"));
        std::fs::create_dir_all(&fallback)
            .map_err(|e| EmployeeError::Internal(format!("create workspace {}: {e}", fallback.display())))?;

        let Some(row) = self.conversation_repo.get(conversation_id).await? else {
            return Ok(());
        };
        let mut extra_value: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        if !extra_value.is_object() {
            extra_value = serde_json::json!({});
        }
        extra_value
            .as_object_mut()
            .expect("json object")
            .insert("workspace".into(), serde_json::Value::String(fallback.to_string_lossy().into_owned()));
        let update = ConversationRowUpdate {
            extra: Some(extra_value.to_string()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(conversation_id, &update).await?;
        Ok(())
    }

    async fn execute_run(&self, owner_user_id: &str, agent: &PersonalAgentRow, run_id: &str, conversation_id: &str) {
        let prompt = build_run_prompt(agent);
        let turn_req = ConversationAgentTurnRequest {
            user_id: owner_user_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            content: prompt,
            files: vec![],
            inject_skills: vec![],
            persist_user_message: true,
            user_message_hidden: true,
            on_started: None,
        };

        let (status, turn_id, summary, error) = match self.conversation_service.run_agent_turn(turn_req).await {
            Ok(outcome) if outcome.status == ConversationAgentTurnStatus::Completed => {
                let summary = self.extract_summary(conversation_id).await;
                (RUN_SUCCESS, Some(outcome.turn_id), summary, None)
            }
            Ok(outcome) => (
                RUN_FAILED,
                Some(outcome.turn_id),
                None,
                Some(outcome.error_message.unwrap_or_else(|| "agent turn failed".into())),
            ),
            Err(e) => (RUN_FAILED, None, None, Some(e.to_string())),
        };

        let result = sqlx::query(
            "UPDATE one_employee_runs SET status = ?, turn_id = ?, summary = ?, error = ?, finished_at = ? \
             WHERE id = ?",
        )
        .bind(status)
        .bind(&turn_id)
        .bind(&summary)
        .bind(&error)
        .bind(now_ms() as i64)
        .bind(run_id)
        .execute(&self.pool)
        .await;
        if let Err(e) = result {
            tracing::error!(run_id, error = %e, "one-employee failed to persist run outcome");
        }
    }

    /// Latest visible assistant text reply, truncated to 240 chars — same
    /// summary rule as the TS reference.
    async fn extract_summary(&self, conversation_id: &str) -> Option<String> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT content FROM messages \
             WHERE conversation_id = ? AND type = 'text' AND position = 'left' \
             ORDER BY created_at DESC LIMIT 12",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await
        .ok()?;

        let reply = rows.into_iter().find_map(|(content,)| {
            let text = serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .and_then(|v| {
                    v.get("content")
                        .and_then(|c| c.as_str())
                        .map(str::to_owned)
                        .or_else(|| v.as_str().map(str::to_owned))
                })
                .unwrap_or(content);
            let trimmed = text.trim().to_owned();
            (!trimmed.is_empty()).then_some(trimmed)
        })?;

        Some(if reply.chars().count() > 240 {
            let truncated: String = reply.chars().take(237).collect();
            format!("{truncated}…")
        } else {
            reply
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_prompt_prefers_instructions() {
        let agent = PersonalAgentRow {
            id: "pa_1".into(),
            owner_user_id: "u".into(),
            tenant_id: "default".into(),
            name: "调研员".into(),
            description: Some("每日调研".into()),
            agent_type: "claude".into(),
            custom_agent_id: None,
            cli_path: None,
            automation_config: r#"{"instructions":"  调研今日热点并输出简报  "}"#.into(),
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(build_run_prompt(&agent), "调研今日热点并输出简报");
    }

    #[test]
    fn run_prompt_falls_back_to_description_then_generic() {
        let mut agent = PersonalAgentRow {
            id: "pa_1".into(),
            owner_user_id: "u".into(),
            tenant_id: "default".into(),
            name: "调研员".into(),
            description: Some("每日调研".into()),
            agent_type: "claude".into(),
            custom_agent_id: None,
            cli_path: None,
            automation_config: "{}".into(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(build_run_prompt(&agent).contains("每日调研"));

        agent.description = None;
        assert!(build_run_prompt(&agent).contains("日常职责"));
    }

    #[test]
    fn run_timestamp_shape() {
        // 2026-07-05 01:30 UTC
        let s = format_run_timestamp(1_783_474_200_000);
        assert_eq!(s.len(), 11);
        assert!(s.contains('/') && s.contains(':'));
    }
}
