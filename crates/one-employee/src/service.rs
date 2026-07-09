//! Digital employee orchestration service.
//!
//! Translation of the 1ONE ClaudeCode reference
//! (`src/process/digitalEmployee/DigitalEmployeeRunService.ts` +
//! `TeamDigitalEmployeeRunService.ts`), rebuilt on upstream in-process
//! primitives instead of HTTP-to-self:
//!
//! - personal run → `ConversationService::create` + `run_agent_turn`
//!   (same path the aionui-cron `JobExecutor` takes for
//!   `ExecutionMode::NewConversation`)
//! - team run → `TeamSessionService::send_message_to_agent` against the
//!   team's existing slot conversation; completion observed by polling
//!   `get_run_state` until `active_run` becomes `None`
//! - cron → own 30s scanner; scheduling semantics reuses upstream
//!   `aionui_cron::scheduler::compute_next_run` so At/Every/Cron + tz
//!   behave identically to upstream cron jobs (zero upstream diff)
//! - runHistory JSON blob → `one_employee_runs` table

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

use aionui_ai_agent::AgentRegistry;
use aionui_api_types::{CreateConversationRequest, CronScheduleDto};
use aionui_common::{AgentType, now_ms};
use aionui_conversation::{ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationService};
use aionui_cron::scheduler::compute_next_run;
use aionui_cron::types::schedule_from_dto;
use aionui_db::{ConversationRowUpdate, IConversationRepository};
use aionui_team::TeamSessionService;

use crate::error::EmployeeError;
use crate::models::{
    EmployeeRunRow, PersonalAgentDto, PersonalAgentRow, RUN_FAILED, RUN_RUNNING, RUN_SUCCESS, TRIGGER_BREAKDOWN,
    TRIGGER_CRON, TRIGGER_MANUAL,
};

/// Outcome of a blocking run: the run/conversation linkage plus the agent's
/// full (untruncated) text reply, so callers can parse structured output.
#[derive(Debug, Clone)]
pub struct RunReply {
    pub run_id: String,
    pub conversation_id: String,
    pub reply: String,
}

/// 30s scanner tick — same cadence the TS cron driver used.
const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Poll `get_run_state` every 3s when waiting for a team run to settle.
const TEAM_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
/// Hard ceiling on team-run wait. Mirrors the 15min cap noted in the
/// design doc; prevents a stuck slot from blocking the scanner forever.
const TEAM_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);

pub struct EmployeeService {
    pool: SqlitePool,
    conversation_service: Arc<ConversationService>,
    conversation_repo: Arc<dyn IConversationRepository>,
    agent_registry: Arc<AgentRegistry>,
    team_session_service: Option<Arc<TeamSessionService>>,
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

pub struct ScheduleInput {
    pub schedule: Option<CronScheduleDto>,
    pub enabled: Option<bool>,
}

fn short_id(prefix: &str) -> String {
    let uuid = uuid::Uuid::now_v7().simple().to_string();
    format!("{prefix}_{uuid}")
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
    let month_lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
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
        return format!(
            "你是「{}」。你的职责：{}\n\n请立即执行你的日常职责，完成后输出可交付摘要。",
            agent.name, description
        );
    }
    format!("你是「{}」。请立即执行你的日常职责，完成后输出可交付摘要。", agent.name)
}

/// Employees a user may *use* (own or shared within their tenant). Free
/// function so the sharing predicate can be unit-tested against a bare pool
/// without constructing the full `EmployeeService`.
async fn select_agent_for_use(
    pool: &SqlitePool,
    user_id: &str,
    tenant_id: &str,
    agent_id: &str,
) -> Result<Option<PersonalAgentRow>, sqlx::Error> {
    sqlx::query_as::<_, PersonalAgentRow>(
        "SELECT * FROM one_personal_agents \
         WHERE id = ? AND (owner_user_id = ? OR (visibility = 'shared' AND tenant_id = ?))",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
}

/// Own employees plus tenant-shared ones. Free function, mirrors
/// `select_agent_for_use` for testability.
async fn select_available_agents(
    pool: &SqlitePool,
    user_id: &str,
    tenant_id: &str,
) -> Result<Vec<PersonalAgentRow>, sqlx::Error> {
    sqlx::query_as::<_, PersonalAgentRow>(
        "SELECT * FROM one_personal_agents \
         WHERE owner_user_id = ? OR (visibility = 'shared' AND tenant_id = ?) \
         ORDER BY updated_at DESC",
    )
    .bind(user_id)
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

/// Truncate a reply to a 240-char run summary (matches the TS reference).
fn truncate_summary(reply: &str) -> String {
    if reply.chars().count() > 240 {
        let truncated: String = reply.chars().take(237).collect();
        format!("{truncated}…")
    } else {
        reply.to_owned()
    }
}

/// Append an optional task context (e.g. a dispatched requirement) under the
/// employee's base run prompt. Empty/whitespace context is a no-op.
fn append_task_context(mut prompt: String, task_context: Option<&str>) -> String {
    if let Some(context) = task_context.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\n## 本次任务\n");
        prompt.push_str(context);
    }
    prompt
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
            team_session_service: None,
            work_dir,
        }
    }

    /// Wire the team session service. Optional so personal-only deployments
    /// (and unit tests that don't exercise team paths) can construct an
    /// `EmployeeService` without it. Called by the router builder after the
    /// `TeamRouterState` is built.
    pub fn with_team_session(mut self, team_session_service: Arc<TeamSessionService>) -> Self {
        self.team_session_service = Some(team_session_service);
        self
    }

    fn require_team_session(&self) -> Result<&Arc<TeamSessionService>, EmployeeError> {
        self.team_session_service
            .as_ref()
            .ok_or_else(|| EmployeeError::Internal("team session service not configured".into()))
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

    pub async fn get(&self, owner_user_id: &str, agent_id: &str) -> Result<PersonalAgentRow, EmployeeError> {
        sqlx::query_as::<_, PersonalAgentRow>("SELECT * FROM one_personal_agents WHERE id = ? AND owner_user_id = ?")
            .bind(agent_id)
            .bind(owner_user_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(EmployeeError::NotFound)
    }

    /// Employees the user can pick from: their own, plus employees shared
    /// within their tenant (A1 L3). Personal-tenant users only ever see their
    /// own (they are the sole member of the 'default' tenant).
    pub async fn list_available(&self, user_id: &str, tenant_id: &str) -> Result<Vec<PersonalAgentDto>, EmployeeError> {
        let rows = select_available_agents(&self.pool, user_id, tenant_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Resolve an employee the user is allowed to *use* (dispatch/breakdown):
    /// their own employee, or one shared within their tenant. Ownership for
    /// mutation still goes through `get`. Returns `NotFound` when neither
    /// applies.
    pub async fn resolve_agent_for_use(
        &self,
        user_id: &str,
        tenant_id: &str,
        agent_id: &str,
    ) -> Result<PersonalAgentRow, EmployeeError> {
        select_agent_for_use(&self.pool, user_id, tenant_id, agent_id)
            .await?
            .ok_or(EmployeeError::NotFound)
    }

    /// Set an employee's visibility ('private' | 'shared'). Owner-only: only
    /// the creator can share or unshare their employee.
    pub async fn set_visibility(
        &self,
        owner_user_id: &str,
        agent_id: &str,
        visibility: &str,
    ) -> Result<PersonalAgentDto, EmployeeError> {
        if visibility != "private" && visibility != "shared" {
            return Err(EmployeeError::BadRequest(format!(
                "invalid visibility: {visibility} (allowed: private/shared)"
            )));
        }
        let result = sqlx::query(
            "UPDATE one_personal_agents SET visibility = ?, updated_at = ? WHERE id = ? AND owner_user_id = ?",
        )
        .bind(visibility)
        .bind(now_ms() as i64)
        .bind(agent_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(EmployeeError::NotFound);
        }
        Ok(self.get(owner_user_id, agent_id).await?.into())
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
              automation_config, schedule_enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
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

    // --- schedule ---

    /// Replace the schedule on a personal agent. Recomputes `next_run_at`
    /// against the supplied schedule (or clears it when disabled/removed).
    pub async fn set_schedule(
        &self,
        owner_user_id: &str,
        agent_id: &str,
        input: ScheduleInput,
    ) -> Result<PersonalAgentDto, EmployeeError> {
        // Verify ownership before writing; the row is needed for nothing
        // else here (schedule is overwritten wholesale).
        self.get(owner_user_id, agent_id).await?;
        let schedule_json = input
            .schedule
            .as_ref()
            .map(|dto| serde_json::to_value(dto))
            .transpose()
            .map_err(|e| EmployeeError::BadRequest(format!("invalid schedule: {e}")))?
            .map(|v| v.to_string());

        let enabled = input.enabled.unwrap_or_else(|| schedule_json.is_some());
        let next_run_at: Option<i64> = match &input.schedule {
            Some(dto) if enabled => {
                let schedule = schedule_from_dto(dto);
                compute_next_run(&schedule, now_ms()).map(|ts| ts as i64)
            }
            _ => None,
        };

        sqlx::query(
            "UPDATE one_personal_agents \
             SET schedule = ?, schedule_enabled = ?, next_run_at = ?, updated_at = ? \
             WHERE id = ? AND owner_user_id = ?",
        )
        .bind(&schedule_json)
        .bind(if enabled { 1 } else { 0 })
        .bind(next_run_at)
        .bind(now_ms() as i64)
        .bind(agent_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;

        Ok(self.get(owner_user_id, agent_id).await?.into())
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
        sqlx::query_as::<_, EmployeeRunRow>("SELECT * FROM one_employee_runs WHERE id = ? AND owner_user_id = ?")
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
        self.start_personal_run(owner_user_id, &agent, TRIGGER_MANUAL, None)
            .await
    }

    /// Manual run carrying an extra task context (e.g. a devops requirement
    /// dispatched to this employee). The context is appended to the agent's
    /// own run prompt so the turn works the requirement, not just the daily
    /// routine.
    ///
    /// Accepts the caller's tenant so a *shared* employee (A1 L3) can be
    /// driven by any same-tenant member — `resolve_agent_for_use` allows the
    /// owner or a tenant-shared employee. The run itself is owned by the
    /// caller (conversation + workspace + run row), regardless of who owns the
    /// agent definition.
    pub async fn run_now_with_context(
        self: &Arc<Self>,
        user_id: &str,
        tenant_id: &str,
        agent_id: &str,
        task_context: String,
    ) -> Result<(String, String), EmployeeError> {
        let agent = self.resolve_agent_for_use(user_id, tenant_id, agent_id).await?;
        self.start_personal_run(user_id, &agent, TRIGGER_MANUAL, Some(task_context))
            .await
    }

    /// Provision a fresh personal run: create the conversation (with the
    /// agent identity injected into `extra`), ensure a workspace, and insert
    /// the `one_employee_runs` row in `running` state. Returns
    /// `(run_id, conversation_id)`. Shared by the fire-and-forget
    /// (`start_personal_run`) and blocking (`run_prompt_blocking`) paths.
    async fn provision_run(
        &self,
        owner_user_id: &str,
        agent: &PersonalAgentRow,
        trigger_source: &str,
    ) -> Result<(String, String), EmployeeError> {
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
        .bind(trigger_source)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok((run_id, conversation_id))
    }

    /// Shared personal-run path for manual and cron triggers. Provisions the
    /// run, then spawns `execute_run` to await the agent turn and persist the
    /// outcome. Returns immediately.
    async fn start_personal_run(
        self: &Arc<Self>,
        owner_user_id: &str,
        agent: &PersonalAgentRow,
        trigger_source: &str,
        task_context: Option<String>,
    ) -> Result<(String, String), EmployeeError> {
        let (run_id, conversation_id) = self.provision_run(owner_user_id, agent, trigger_source).await?;

        let service = Arc::clone(self);
        let owner = owner_user_id.to_owned();
        let run_id_bg = run_id.clone();
        let conversation_id_bg = conversation_id.clone();
        let trigger = trigger_source.to_owned();
        let agent_clone = agent.clone();
        tokio::spawn(async move {
            service
                .execute_run(
                    &owner,
                    &agent_clone,
                    &run_id_bg,
                    &conversation_id_bg,
                    &trigger,
                    task_context,
                )
                .await;
        });

        Ok((run_id, conversation_id))
    }

    /// Blocking run with a fully-supplied prompt: provision the run, await the
    /// agent turn inline (no `build_run_prompt` prepend, no background spawn),
    /// persist the outcome, and return the agent's full text reply so callers
    /// can parse structured output (e.g. devops breakdown → child requirements).
    ///
    /// Accepts the caller's tenant so a shared employee can be used by any
    /// same-tenant member (A1 L3); the run is owned by the caller.
    pub async fn run_prompt_blocking(
        self: &Arc<Self>,
        user_id: &str,
        tenant_id: &str,
        agent_id: &str,
        prompt: String,
    ) -> Result<RunReply, EmployeeError> {
        let agent = self.resolve_agent_for_use(user_id, tenant_id, agent_id).await?;
        let (run_id, conversation_id) = self.provision_run(user_id, &agent, TRIGGER_BREAKDOWN).await?;

        let turn_req = ConversationAgentTurnRequest {
            user_id: user_id.to_owned(),
            conversation_id: conversation_id.clone(),
            content: prompt,
            files: vec![],
            inject_skills: vec![],
            persist_user_message: true,
            user_message_hidden: true,
            on_started: None,
        };

        match self.conversation_service.run_agent_turn(turn_req).await {
            Ok(outcome) if outcome.status == ConversationAgentTurnStatus::Completed => {
                let reply = self.extract_latest_reply(&conversation_id).await.unwrap_or_default();
                let summary = truncate_summary(&reply);
                self.persist_run_outcome(&run_id, RUN_SUCCESS, Some(&outcome.turn_id), Some(&summary), None)
                    .await;
                Ok(RunReply {
                    run_id,
                    conversation_id,
                    reply,
                })
            }
            Ok(outcome) => {
                let error = outcome.error_message.unwrap_or_else(|| "agent turn failed".into());
                self.persist_run_outcome(&run_id, RUN_FAILED, Some(&outcome.turn_id), None, Some(&error))
                    .await;
                Err(EmployeeError::Internal(error))
            }
            Err(e) => {
                self.persist_run_outcome(&run_id, RUN_FAILED, None, None, Some(&e.to_string()))
                    .await;
                Err(EmployeeError::Internal(e.to_string()))
            }
        }
    }

    /// Manual "run now" against an existing team slot. Reads the slot's
    /// conversation_id via `TeamSessionService::get_team`, then fires the
    /// run prompt via `send_message_to_agent` (fire-and-ack), then polls
    /// `get_run_state` until the slot settles.
    pub async fn run_now_team(
        self: &Arc<Self>,
        owner_user_id: &str,
        agent_id: &str,
        team_id: &str,
        slot_id: &str,
    ) -> Result<(String, String), EmployeeError> {
        let team_session = self.require_team_session()?.clone();
        let agent = self.get(owner_user_id, agent_id).await?;

        // Resolve the slot's existing conversation_id before we fire — we
        // need it for summary extraction after the run settles.
        let team = team_session
            .get_team(owner_user_id, team_id)
            .await
            .map_err(|e| EmployeeError::Internal(format!("team get_team: {e}")))?;
        let slot = team
            .assistants
            .iter()
            .find(|a| a.slot_id == slot_id)
            .ok_or_else(|| EmployeeError::BadRequest(format!("slot {slot_id} not found in team {team_id}")))?;
        let conversation_id = slot.conversation_id.clone();

        let prompt = build_run_prompt(&agent);
        team_session
            .send_message_to_agent(owner_user_id, team_id, slot_id, &prompt, None)
            .await
            .map_err(|e| EmployeeError::Internal(format!("team send_message_to_agent: {e}")))?;

        let now = now_ms() as i64;
        let run_id = short_id("run");
        sqlx::query(
            "INSERT INTO one_employee_runs \
             (id, agent_id, owner_user_id, tenant_id, team_id, slot_id, conversation_id, status, trigger_source, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run_id)
        .bind(&agent.id)
        .bind(owner_user_id)
        .bind(&agent.tenant_id)
        .bind(team_id)
        .bind(slot_id)
        .bind(&conversation_id)
        .bind(RUN_RUNNING)
        .bind(TRIGGER_MANUAL)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let service = Arc::clone(self);
        let owner = owner_user_id.to_owned();
        let run_id_bg = run_id.clone();
        let team_id_bg = team_id.to_owned();
        let conversation_id_bg = conversation_id.clone();
        tokio::spawn(async move {
            service
                .execute_team_run(&owner, &run_id_bg, &team_id_bg, &conversation_id_bg)
                .await;
        });

        Ok((run_id, conversation_id))
    }

    /// Mirror of the cron executor's fallback: some conversation types come
    /// back without a provisioned workspace; the agent turn needs one.
    async fn ensure_workspace(&self, conversation_id: &str, extra: &serde_json::Value) -> Result<(), EmployeeError> {
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
        let mut extra_value: serde_json::Value =
            serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        if !extra_value.is_object() {
            extra_value = serde_json::json!({});
        }
        extra_value.as_object_mut().expect("json object").insert(
            "workspace".into(),
            serde_json::Value::String(fallback.to_string_lossy().into_owned()),
        );
        let update = ConversationRowUpdate {
            extra: Some(extra_value.to_string()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(conversation_id, &update).await?;
        Ok(())
    }

    async fn execute_run(
        &self,
        owner_user_id: &str,
        agent: &PersonalAgentRow,
        run_id: &str,
        conversation_id: &str,
        trigger_source: &str,
        task_context: Option<String>,
    ) {
        let prompt = append_task_context(build_run_prompt(agent), task_context.as_deref());
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

        self.persist_run_outcome(run_id, status, turn_id.as_deref(), summary.as_deref(), error.as_deref())
            .await;
        if trigger_source == TRIGGER_CRON {
            self.recompute_next_run(&agent.id).await;
        }
    }

    /// Wait for the team slot to settle (`active_run` flips to `None`),
    /// then extract summary from the slot's conversation_id (resolved
    /// up-front in `run_now_team`).
    async fn execute_team_run(&self, owner_user_id: &str, run_id: &str, team_id: &str, conversation_id: &str) {
        let team_session = match self.require_team_session() {
            Ok(svc) => Arc::clone(svc),
            Err(e) => {
                self.persist_run_outcome(run_id, RUN_FAILED, None, None, Some(&e.to_string()))
                    .await;
                return;
            }
        };

        let deadline = std::time::Instant::now() + TEAM_POLL_MAX;
        loop {
            if std::time::Instant::now() >= deadline {
                let msg = "team run poll timed out";
                self.persist_run_outcome(run_id, RUN_FAILED, None, None, Some(msg))
                    .await;
                return;
            }
            let state = match team_session.get_run_state(owner_user_id, team_id).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("get_run_state: {e}");
                    self.persist_run_outcome(run_id, RUN_FAILED, None, None, Some(&msg))
                        .await;
                    return;
                }
            };
            if state.active_run.is_none() {
                break;
            }
            tokio::time::sleep(TEAM_POLL_INTERVAL).await;
        }

        let summary = self.extract_summary(conversation_id).await;
        self.persist_run_outcome(run_id, RUN_SUCCESS, None, summary.as_deref(), None)
            .await;
    }

    async fn persist_run_outcome(
        &self,
        run_id: &str,
        status: &str,
        turn_id: Option<&str>,
        summary: Option<&str>,
        error: Option<&str>,
    ) {
        let result = sqlx::query(
            "UPDATE one_employee_runs SET status = ?, turn_id = ?, summary = ?, error = ?, finished_at = ? \
             WHERE id = ?",
        )
        .bind(status)
        .bind(turn_id)
        .bind(summary)
        .bind(error)
        .bind(now_ms() as i64)
        .bind(run_id)
        .execute(&self.pool)
        .await;
        if let Err(e) = result {
            tracing::error!(run_id, error = %e, "one-employee failed to persist run outcome");
        }
    }

    /// Recompute `next_run_at` for the given agent after a cron-triggered
    /// run lands. Uses upstream `compute_next_run` so semantics match the
    /// cron driver exactly.
    async fn recompute_next_run(&self, agent_id: &str) {
        let row: Result<(Option<String>,), sqlx::Error> =
            sqlx::query_as("SELECT schedule FROM one_personal_agents WHERE id = ?")
                .bind(agent_id)
                .fetch_one(&self.pool)
                .await;
        let Ok((schedule_json,)) = row else { return };
        let Some(schedule_json) = schedule_json else { return };
        let Ok(dto) = serde_json::from_str::<CronScheduleDto>(&schedule_json) else {
            return;
        };
        let schedule = schedule_from_dto(&dto);
        let next = compute_next_run(&schedule, now_ms()).map(|ts| ts as i64);
        let _ = sqlx::query("UPDATE one_personal_agents SET next_run_at = ? WHERE id = ?")
            .bind(next)
            .bind(agent_id)
            .execute(&self.pool)
            .await;
    }

    /// Latest visible assistant text reply, truncated to 240 chars — same
    /// summary rule as the TS reference. Used to fill the run row `summary`.
    async fn extract_summary(&self, conversation_id: &str) -> Option<String> {
        self.extract_latest_reply(conversation_id)
            .await
            .map(|r| truncate_summary(&r))
    }

    /// Latest visible assistant text reply, untruncated. Callers that parse
    /// structured output (breakdown) need the whole thing.
    async fn extract_latest_reply(&self, conversation_id: &str) -> Option<String> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT content FROM messages \
             WHERE conversation_id = ? AND type = 'text' AND position = 'left' \
             ORDER BY created_at DESC LIMIT 12",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await
        .ok()?;

        rows.into_iter().find_map(|(content,)| {
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
        })
    }

    // --- cron scanner ---

    /// Spawn the 30s schedule scanner. Runs for the lifetime of the service.
    /// On each tick: select agents with `schedule_enabled=1 AND
    /// next_run_at <= now`, fire `run_now` with `trigger_source='cron'`,
    /// let `recompute_next_run` reschedule on completion.
    pub fn spawn_scheduler(self: &Arc<Self>) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SCAN_INTERVAL);
            // First tick fires immediately on `tokio::time::Interval` — skip
            // it so we don't fire all due schedules the instant the service
            // starts (prevents a thundering herd during boot).
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = service.scan_once().await {
                    tracing::error!(error = %e, "one-employee cron scanner tick failed");
                }
            }
        });
    }

    /// One scanner pass. Returns the number of agents fired.
    async fn scan_once(self: &Arc<Self>) -> Result<usize, EmployeeError> {
        let now = now_ms() as i64;
        let due: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, owner_user_id FROM one_personal_agents \
             WHERE schedule_enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;

        let mut fired = 0;
        for (agent_id, owner_user_id) in due {
            fired += 1;
            let agent = match self.get(&owner_user_id, &agent_id).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(agent_id, error = %e, "one-employee scanner: agent disappeared");
                    continue;
                }
            };

            // Mark as fired immediately by clearing next_run_at — prevents
            // the next tick from re-firing while this run is still in
            // flight. `recompute_next_run` (called at the end of
            // `execute_run`) will set the next fire time.
            let _ = sqlx::query("UPDATE one_personal_agents SET next_run_at = NULL WHERE id = ?")
                .bind(&agent_id)
                .execute(&self.pool)
                .await;

            if let Err(e) = self
                .start_personal_run(&owner_user_id, &agent, TRIGGER_CRON, None)
                .await
            {
                tracing::error!(agent_id, error = %e, "one-employee scanner: start_personal_run failed");
                // Restore next_run_at so we retry on the next tick.
                let _ = self.recompute_next_run(&agent_id).await;
            }
        }
        Ok(fired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::run_one_employee_migrations;

    #[test]
    fn append_task_context_appends_and_noops() {
        assert_eq!(append_task_context("base".into(), None), "base");
        assert_eq!(append_task_context("base".into(), Some("   ")), "base");
        assert_eq!(
            append_task_context("base".into(), Some("做需求 X")),
            "base\n\n## 本次任务\n做需求 X"
        );
    }

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
            schedule: None,
            schedule_enabled: 0,
            next_run_at: None,
            visibility: "private".into(),
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
            schedule: None,
            schedule_enabled: 0,
            next_run_at: None,
            visibility: "private".into(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(build_run_prompt(&agent).contains("每日调研"));

        agent.description = None;
        assert!(build_run_prompt(&agent).contains("日常职责"));
    }

    async fn insert_agent(pool: &SqlitePool, id: &str, owner: &str, tenant: &str, visibility: &str) {
        sqlx::query(
            "INSERT INTO one_personal_agents \
             (id, owner_user_id, tenant_id, name, agent_type, automation_config, schedule_enabled, visibility, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'claude', '{}', 0, ?, 0, 0)",
        )
        .bind(id)
        .bind(owner)
        .bind(tenant)
        .bind(id)
        .bind(visibility)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sharing_resolves_own_and_tenant_shared() {
        let db = aionui_db::init_database_memory().await.unwrap();
        run_one_employee_migrations(db.pool()).await.unwrap();
        let pool = db.pool();
        // A: a1 private/t1, a2 shared/t1. B: b1 shared/t1. C: c1 shared/t2.
        insert_agent(pool, "a1", "A", "t1", "private").await;
        insert_agent(pool, "a2", "A", "t1", "shared").await;
        insert_agent(pool, "b1", "B", "t1", "shared").await;
        insert_agent(pool, "c1", "C", "t2", "shared").await;

        // A@t1: own a1/a2; same-tenant shared b1; NOT cross-tenant c1.
        assert!(select_agent_for_use(pool, "A", "t1", "a1").await.unwrap().is_some());
        assert!(select_agent_for_use(pool, "A", "t1", "b1").await.unwrap().is_some());
        assert!(select_agent_for_use(pool, "A", "t1", "c1").await.unwrap().is_none());
        // B@t1: NOT A's private a1; A's shared a2 yes; own b1 yes.
        assert!(select_agent_for_use(pool, "B", "t1", "a1").await.unwrap().is_none());
        assert!(select_agent_for_use(pool, "B", "t1", "a2").await.unwrap().is_some());
        // Cross-tenant: A@t2 cannot use t1-shared b1, but always sees own a1.
        assert!(select_agent_for_use(pool, "A", "t2", "b1").await.unwrap().is_none());
        assert!(select_agent_for_use(pool, "A", "t2", "a1").await.unwrap().is_some());

        // list_available for B@t1 = own b1 + shared-in-t1 a2 (not private a1, not t2 c1).
        let ids: std::collections::HashSet<String> = select_available_agents(pool, "B", "t1")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["a2".to_owned(), "b1".to_owned()].into_iter().collect());
    }

    #[test]
    fn run_timestamp_shape() {
        // 2026-07-05 01:30 UTC
        let s = format_run_timestamp(1_783_474_200_000);
        assert_eq!(s.len(), 11);
        assert!(s.contains('/') && s.contains(':'));
    }

    #[test]
    fn compute_next_run_every() {
        let dto = CronScheduleDto::Every {
            every_ms: 60_000,
            description: None,
        };
        let schedule = schedule_from_dto(&dto);
        // 1000ms + 60000ms = 61000ms
        assert_eq!(compute_next_run(&schedule, 1000), Some(61_000));
    }

    #[test]
    fn compute_next_run_at_is_absolute() {
        let dto = CronScheduleDto::At {
            at_ms: 5_000,
            description: None,
        };
        let schedule = schedule_from_dto(&dto);
        // At always returns the absolute timestamp regardless of `now`.
        assert_eq!(compute_next_run(&schedule, 1000), Some(5_000));
        assert_eq!(compute_next_run(&schedule, 100_000), Some(5_000));
    }
}
