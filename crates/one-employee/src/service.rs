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
use aionui_api_types::{
    AssistantConversationOverridesRequest, AssistantConversationRequest, CreateConversationRequest, CronScheduleDto,
};
use aionui_common::{AgentType, ProviderWithModel, now_ms};
use aionui_conversation::{ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationService};
use aionui_cron::scheduler::compute_next_run;
use aionui_cron::types::schedule_from_dto;
use aionui_db::{ConversationRowUpdate, IConversationRepository, IProviderRepository};
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
    /// Optional so personal-only deployments and unit tests can build the
    /// service without it. When wired, save-time validation additionally
    /// checks that an aionrs employee's model is offered by an *enabled*
    /// provider; when absent only the shape check runs.
    provider_repo: Option<Arc<dyn IProviderRepository>>,
    work_dir: PathBuf,
}

pub struct CreateEmployeeInput {
    pub name: String,
    pub description: Option<String>,
    pub agent_type: String,
    pub custom_agent_id: Option<String>,
    pub cli_path: Option<String>,
    pub assistant_id: Option<String>,
    pub agent_id_override: Option<String>,
    pub model_id: Option<String>,
    pub model: Option<ProviderWithModel>,
    pub automation_config: Option<serde_json::Value>,
}

/// `None` on any field means "leave unchanged". For the nullable persona/model
/// fields an explicitly-supplied empty string clears the column back to NULL,
/// so a client can detach a persona or a model without deleting the employee.
pub struct UpdateEmployeeInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub agent_type: Option<String>,
    pub assistant_id: Option<String>,
    pub agent_id_override: Option<String>,
    pub model_id: Option<String>,
    pub model: Option<ProviderWithModel>,
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

/// Whether a stored `agent_type` label denotes the aionrs backend — the only
/// one that takes a top-level conversation model. Compares against the enum's
/// own serde name rather than a literal so a rename can't silently drift.
fn is_aionrs(agent_type: &str) -> bool {
    agent_type.trim() == AgentType::Aionrs.serde_name()
}

/// Trim an incoming optional string, treating blank as absent. Used so a
/// client sending `""` never writes a whitespace-only id into the DB.
fn normalize_optional(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|s| !s.is_empty()).map(ToOwned::to_owned)
}

/// Update-time merge for a nullable column: field absent → keep `existing`;
/// field present and blank → clear to `None`; otherwise take the new value.
fn merge_optional(incoming: Option<&str>, existing: Option<String>) -> Option<String> {
    match incoming {
        None => existing,
        Some(value) => normalize_optional(Some(value)),
    }
}

fn serialize_model(model: Option<&ProviderWithModel>) -> Result<Option<String>, EmployeeError> {
    model
        .map(|model| {
            serde_json::to_string(model).map_err(|e| EmployeeError::Internal(format!("serialize employee model: {e}")))
        })
        .transpose()
}

/// Only aionrs conversations carry a meaningful top-level model. Verbatim port
/// of `aionui_cron::executor::resolve_model` so the employee and cron paths
/// derive the same `(provider_id, model)` — see the divergence warning in
/// `aionui_conversation::task_options`.
fn resolve_model(agent: &PersonalAgentRow) -> Option<ProviderWithModel> {
    if !is_aionrs(&agent.agent_type) {
        return None;
    }
    agent
        .model
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ProviderWithModel>(raw).ok())
}

/// Build the persona binding for a run. Mirrors
/// `aionui_cron::executor::build_assistant_request`: `assistant_id` wins, the
/// legacy `custom_agent_id` column is the fallback (which is what finally makes
/// that previously write-only column mean something).
///
/// `conversation_overrides.agent_id` is the *only* channel that can move a
/// persona onto a different backend — once `assistant` is set,
/// `CreateConversationRequest.type` is ignored and the effective agent type
/// comes from the assistant snapshot.
fn build_assistant_request(agent: &PersonalAgentRow) -> Option<AssistantConversationRequest> {
    let assistant_id = normalize_optional(agent.assistant_id.as_deref())
        .or_else(|| normalize_optional(agent.custom_agent_id.as_deref()))?;

    let agent_id = normalize_optional(agent.agent_id_override.as_deref());
    // ACP backends carry the model through the assistant overrides; aionrs uses
    // the top-level `model` instead (see `resolve_model`).
    let model = if is_aionrs(&agent.agent_type) {
        None
    } else {
        normalize_optional(agent.model_id.as_deref())
    };

    let overrides = (agent_id.is_some() || model.is_some()).then(|| AssistantConversationOverridesRequest {
        agent_id,
        model,
        ..Default::default()
    });

    Some(AssistantConversationRequest {
        id: assistant_id,
        locale: None,
        conversation_overrides: overrides,
    })
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
            provider_repo: None,
            work_dir,
        }
    }

    /// Wire the provider repository so `validate_model_binding` can reject an
    /// aionrs model that no enabled provider offers at save time, instead of
    /// letting the run fail later. Optional, same rationale as
    /// [`Self::with_team_session`].
    pub fn with_provider_repo(mut self, provider_repo: Arc<dyn IProviderRepository>) -> Self {
        self.provider_repo = Some(provider_repo);
        self
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

    /// Reject `(agent_type, model)` combinations the conversation layer would
    /// refuse, at save time rather than at run time.
    ///
    /// Three rules:
    /// 1. An aionrs employee *needs* a model — without one `provision_run`
    ///    resolves the empty provider sentinel and the run dies with
    ///    `Provider '' not found`, which is the whole bug this binding exists to
    ///    fix. Only enforced when the caller is actually setting the binding
    ///    (`require_model`), so renaming a legacy backend-only employee still
    ///    works.
    /// 2. A top-level model is aionrs-only — `ConversationService::create`
    ///    returns a hard 400 for any other agent type that carries one.
    /// 3. For aionrs the model must be offered by an *enabled* provider.
    ///    Same check as `aionui_team::provisioning::resolve_provider_for_model`;
    ///    skipped when no provider repo is wired.
    async fn validate_model_binding(
        &self,
        owner_user_id: &str,
        agent_type: &str,
        model: Option<&ProviderWithModel>,
        require_model: bool,
    ) -> Result<(), EmployeeError> {
        let Some(model) = model else {
            if require_model && is_aionrs(agent_type) {
                return Err(EmployeeError::BadRequest(
                    "an aionrs employee requires a model; pick one from an enabled provider".into(),
                ));
            }
            return Ok(());
        };

        if !is_aionrs(agent_type) {
            return Err(EmployeeError::BadRequest(format!(
                "a model may only be bound to an aionrs employee; '{agent_type}' resolves its model through its own backend"
            )));
        }
        if model.provider_id.trim().is_empty() || model.model.trim().is_empty() {
            return Err(EmployeeError::BadRequest(
                "model requires both providerId and model".into(),
            ));
        }

        let Some(provider_repo) = self.provider_repo.as_ref() else {
            return Ok(());
        };
        let provider = provider_repo
            .find_by_id(owner_user_id, &model.provider_id)
            .await
            .map_err(|e| EmployeeError::Internal(format!("load provider: {e}")))?
            .ok_or_else(|| EmployeeError::BadRequest(format!("provider '{}' no longer exists", model.provider_id)))?;
        if !provider.enabled {
            return Err(EmployeeError::BadRequest(format!(
                "provider '{}' is disabled; pick a model from an enabled provider",
                model.provider_id
            )));
        }

        Ok(())
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

        let agent_type = input.agent_type.trim();
        self.validate_model_binding(owner_user_id, agent_type, input.model.as_ref(), true)
            .await?;
        let model = serialize_model(input.model.as_ref())?;

        let id = short_id("pa");
        let now = now_ms() as i64;
        sqlx::query(
            "INSERT INTO one_personal_agents \
             (id, owner_user_id, tenant_id, name, description, agent_type, custom_agent_id, cli_path, \
              assistant_id, agent_id_override, model_id, model, \
              automation_config, schedule_enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(owner_user_id)
        .bind(tenant_id)
        .bind(name)
        .bind(&input.description)
        .bind(agent_type)
        .bind(&input.custom_agent_id)
        .bind(&input.cli_path)
        .bind(normalize_optional(input.assistant_id.as_deref()))
        .bind(normalize_optional(input.agent_id_override.as_deref()))
        .bind(normalize_optional(input.model_id.as_deref()))
        .bind(&model)
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

        let agent_type = match input.agent_type.as_deref().map(str::trim) {
            Some("") => return Err(EmployeeError::BadRequest("agentType must not be empty".into())),
            Some(value) => value.to_owned(),
            None => existing.agent_type,
        };
        // Absent field → keep stored value; explicit empty string → clear to NULL.
        let assistant_id = merge_optional(input.assistant_id.as_deref(), existing.assistant_id);
        let agent_id_override = merge_optional(input.agent_id_override.as_deref(), existing.agent_id_override);
        let model_id = merge_optional(input.model_id.as_deref(), existing.model_id);
        let model = match input.model.as_ref() {
            Some(model) => serialize_model(Some(model))?,
            None => existing.model,
        };

        // Re-validate against the *resulting* pair, not the incoming one: changing
        // only the backend on an employee that already stores a model must not
        // leave behind a combination the conversation layer would reject.
        // "aionrs needs a model" is only enforced when this request actually
        // touches the binding, so renaming a legacy backend-only employee (which
        // predates migration 004 and has no model) is still allowed.
        let effective_model = model
            .as_deref()
            .and_then(|raw| serde_json::from_str::<ProviderWithModel>(raw).ok());
        let touches_binding = input.agent_type.is_some() || input.model.is_some();
        self.validate_model_binding(owner_user_id, &agent_type, effective_model.as_ref(), touches_binding)
            .await?;

        sqlx::query(
            "UPDATE one_personal_agents SET name = ?, description = ?, agent_type = ?, assistant_id = ?, \
             agent_id_override = ?, model_id = ?, model = ?, automation_config = ?, updated_at = ? \
             WHERE id = ? AND owner_user_id = ?",
        )
        .bind(&name)
        .bind(&description)
        .bind(&agent_type)
        .bind(&assistant_id)
        .bind(&agent_id_override)
        .bind(&model_id)
        .bind(&model)
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
        let assistant = build_assistant_request(agent);
        let req = CreateConversationRequest {
            // With a persona attached the effective agent type is derived from
            // the assistant snapshot (and can be redirected by
            // `conversation_overrides.agent_id`), so an explicit type here would
            // be ignored at best and misleading at worst. Same rule as the cron
            // executor.
            r#type: if assistant.is_some() { None } else { Some(agent_type) },
            name: Some(conversation_name),
            // Was hardcoded `None`, which made every aionrs employee resolve to
            // the empty provider sentinel and fail with `Provider '' not found`.
            model: resolve_model(agent),
            assistant,
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

        self.ensure_workspace(owner_user_id, &conversation_id, &response.extra).await?;

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
            // Employee/breakdown runs don't force a runtime mode; keep the
            // agent's own resolved mode (matches pre-v0.1.45 behavior).
            required_runtime_mode: None,
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
    async fn ensure_workspace(
        &self,
        owner_user_id: &str,
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

        let Some(row) = self.conversation_repo.get(owner_user_id, conversation_id).await? else {
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
        self.conversation_repo.update(owner_user_id, conversation_id, &update).await?;
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
            // Autopilot employee runs don't force a runtime mode; keep the
            // agent's own resolved mode (matches pre-v0.1.45 behavior).
            required_runtime_mode: None,
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

    fn agent_row(agent_type: &str) -> PersonalAgentRow {
        PersonalAgentRow {
            id: "pa_1".into(),
            owner_user_id: "u".into(),
            tenant_id: "default".into(),
            name: "调研员".into(),
            description: Some("每日调研".into()),
            agent_type: agent_type.into(),
            custom_agent_id: None,
            cli_path: None,
            assistant_id: None,
            agent_id_override: None,
            model_id: None,
            model: None,
            automation_config: "{}".into(),
            schedule: None,
            schedule_enabled: 0,
            next_run_at: None,
            visibility: "private".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn run_prompt_prefers_instructions() {
        let mut agent = agent_row("claude");
        agent.automation_config = r#"{"instructions":"  调研今日热点并输出简报  "}"#.into();
        assert_eq!(build_run_prompt(&agent), "调研今日热点并输出简报");
    }

    #[test]
    fn run_prompt_falls_back_to_description_then_generic() {
        let mut agent = agent_row("claude");
        assert!(build_run_prompt(&agent).contains("每日调研"));

        agent.description = None;
        assert!(build_run_prompt(&agent).contains("日常职责"));
    }

    // ── persona + model binding (migration 004) ────────────────────────

    /// The reported bug: an aionrs employee used to reach the factory with an
    /// empty provider id and fail with `Provider '' not found`. A bound model
    /// must now survive the round-trip through the stored column.
    #[test]
    fn aionrs_agent_resolves_stored_model() {
        let mut agent = agent_row("aionrs");
        agent.model = Some(
            serde_json::to_string(&ProviderWithModel {
                provider_id: "prov_1".into(),
                model: "glm-5-2".into(),
                use_model: None,
            })
            .unwrap(),
        );

        let resolved = resolve_model(&agent).expect("aionrs must carry a top-level model");
        assert_eq!(resolved.provider_id, "prov_1");
        assert_eq!(resolved.model, "glm-5-2");
    }

    /// Top-level model is aionrs-only — `ConversationService::create` returns a
    /// hard 400 otherwise, so ACP employees must never send one even if the
    /// column somehow holds a value.
    #[test]
    fn non_aionrs_agent_never_sends_a_top_level_model() {
        let mut agent = agent_row("claude");
        agent.model = Some(r#"{"provider_id":"prov_1","model":"x","use_model":null}"#.into());
        assert!(resolve_model(&agent).is_none());
    }

    #[test]
    fn unparseable_model_column_degrades_to_none() {
        let mut agent = agent_row("aionrs");
        agent.model = Some("not-json".into());
        assert!(resolve_model(&agent).is_none());
    }

    #[test]
    fn assistant_request_is_absent_without_a_persona() {
        assert!(build_assistant_request(&agent_row("claude")).is_none());
    }

    /// The legacy `custom_agent_id` column was write-only dead data; it now
    /// serves as the fallback persona source, matching the cron executor.
    #[test]
    fn assistant_request_prefers_assistant_id_then_custom_agent_id() {
        let mut agent = agent_row("claude");
        agent.custom_agent_id = Some("legacy_persona".into());
        assert_eq!(build_assistant_request(&agent).unwrap().id, "legacy_persona");

        agent.assistant_id = Some("persona_1".into());
        assert_eq!(build_assistant_request(&agent).unwrap().id, "persona_1");
    }

    /// A manual backend override only takes effect through
    /// `conversation_overrides.agent_id` — once `assistant` is set the
    /// conversation type is derived from the assistant snapshot.
    #[test]
    fn backend_override_travels_as_conversation_override() {
        let mut agent = agent_row("aionrs");
        agent.assistant_id = Some("persona_1".into());
        agent.agent_id_override = Some("agent_meta_9".into());

        let overrides = build_assistant_request(&agent)
            .unwrap()
            .conversation_overrides
            .expect("override must be forwarded");
        assert_eq!(overrides.agent_id.as_deref(), Some("agent_meta_9"));
        // aionrs takes its model from the top level, not the assistant override.
        assert!(overrides.model.is_none());
    }

    #[test]
    fn acp_persona_carries_model_id_through_the_override() {
        let mut agent = agent_row("claude");
        agent.assistant_id = Some("persona_1".into());
        agent.model_id = Some("claude-sonnet-4-6".into());

        let overrides = build_assistant_request(&agent).unwrap().conversation_overrides.unwrap();
        assert_eq!(overrides.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn blank_persona_fields_are_treated_as_absent() {
        let mut agent = agent_row("claude");
        agent.assistant_id = Some("   ".into());
        assert!(build_assistant_request(&agent).is_none());

        agent.assistant_id = Some("persona_1".into());
        agent.agent_id_override = Some("  ".into());
        assert!(
            build_assistant_request(&agent)
                .unwrap()
                .conversation_overrides
                .is_none()
        );
    }

    #[test]
    fn merge_optional_keeps_clears_and_replaces() {
        assert_eq!(merge_optional(None, Some("kept".into())), Some("kept".into()));
        assert_eq!(merge_optional(Some(""), Some("kept".into())), None);
        assert_eq!(merge_optional(Some("new"), Some("kept".into())), Some("new".into()));
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
