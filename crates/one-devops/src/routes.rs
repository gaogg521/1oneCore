//! `/api/one/devops/*` routes. Mount behind the upstream auth middleware —
//! the whole board is collaborative, so every authenticated org member can
//! read and write (matching the 1one superAssistant behavior).

use axum::extract::{Path, State};
use axum::routing::{get, patch};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::error::DevopsError;
use crate::models::{
    McpRegistryDto, MilestoneDto, PipelineDto, PipelineRunDto, RagConfigDto, RagDocumentDto, RagSearchHit,
    RequirementCommentDto, RequirementDto, SkillRegistryDto, TestCaseDto, TestPlanDto,
};
use crate::service::{CreateRequirementInput, UpdateRequirementInput};
use crate::state::OneDevopsRouterState;

pub fn one_devops_routes(state: OneDevopsRouterState) -> Router {
    Router::new()
        .route("/api/one/devops/requirements/tree", get(requirements_tree))
        .route("/api/one/devops/requirements", axum::routing::post(create_requirement))
        .route(
            "/api/one/devops/requirements/{id}",
            patch(update_requirement).delete(delete_requirement),
        )
        .route(
            "/api/one/devops/requirements/{id}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/one/devops/requirements/{id}/dispatch",
            axum::routing::post(dispatch_requirement),
        )
        .route(
            "/api/one/devops/requirements/{id}/breakdown",
            axum::routing::post(breakdown_requirement),
        )
        .route("/api/one/devops/skills", get(list_skills).post(upsert_skill))
        .route("/api/one/devops/skills/{id}", axum::routing::delete(delete_skill))
        .route("/api/one/devops/mcp-registry", get(list_mcp).post(upsert_mcp))
        .route("/api/one/devops/mcp-registry/{id}", axum::routing::delete(delete_mcp))
        .route("/api/one/devops/rag/documents", get(list_rag).post(register_rag))
        .route("/api/one/devops/rag/documents/{id}", axum::routing::delete(delete_rag))
        .route(
            "/api/one/devops/rag/documents/{id}/content",
            axum::routing::put(set_rag_content),
        )
        .route(
            "/api/one/devops/rag/documents/{id}/process",
            axum::routing::post(process_rag),
        )
        .route("/api/one/devops/rag/config", get(get_rag_config).put(set_rag_config))
        .route("/api/one/devops/rag/search", axum::routing::post(search_rag))
        .route(
            "/api/one/devops/milestones",
            get(list_milestones).post(create_milestone),
        )
        .route(
            "/api/one/devops/milestones/{id}",
            patch(update_milestone).delete(delete_milestone),
        )
        // test plans (A4)
        .route(
            "/api/one/devops/test-plans",
            get(list_test_plans).post(create_test_plan),
        )
        .route(
            "/api/one/devops/test-plans/{id}",
            patch(update_test_plan).delete(delete_test_plan),
        )
        .route(
            "/api/one/devops/test-plans/{id}/cases",
            get(list_test_cases).post(create_test_case),
        )
        .route(
            "/api/one/devops/test-plans/{plan_id}/cases/{id}",
            patch(update_test_case).delete(delete_test_case),
        )
        // pipelines (A4)
        .route("/api/one/devops/pipelines", get(list_pipelines).post(create_pipeline))
        .route(
            "/api/one/devops/pipelines/{id}",
            patch(update_pipeline).delete(delete_pipeline),
        )
        .route(
            "/api/one/devops/pipelines/{id}/runs",
            get(list_pipeline_runs).post(create_pipeline_run),
        )
        .route(
            "/api/one/devops/pipelines/{pipeline_id}/runs/{id}",
            patch(update_pipeline_run),
        )
        .with_state(state)
}

// -- requirements ---------------------------------------------------------

async fn requirements_tree(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<RequirementDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.requirements_tree().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequirementBody {
    subject: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    milestone_id: Option<String>,
    #[serde(default)]
    autopilot: Option<bool>,
}

async fn create_requirement(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CreateRequirementBody>,
) -> Result<Json<ApiResponse<RequirementDto>>, DevopsError> {
    let created = state
        .service
        .create_requirement(
            &user.id,
            Some(user.username.as_str()),
            CreateRequirementInput {
                parent_id: body.parent_id,
                kind: body.kind,
                subject: body.subject,
                description: body.description,
                priority: body.priority,
                milestone_id: body.milestone_id,
                autopilot: body.autopilot,
            },
        )
        .await?;
    maybe_autopilot(&state, &user.id, &created.id).await;
    Ok(Json(ApiResponse::ok(created)))
}

/// PATCH body: absent field = keep, `null` = clear (for nullable columns).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequirementBody {
    #[serde(default)]
    subject: Option<String>,
    #[serde(default, with = "double_option")]
    description: Option<Option<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default, with = "double_option")]
    assigned_to: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    parent_id: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    milestone_id: Option<Option<String>>,
    #[serde(default)]
    autopilot: Option<bool>,
}

/// serde helper distinguishing "absent" from "null".
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

async fn update_requirement(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRequirementBody>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state
        .service
        .update_requirement(
            &id,
            UpdateRequirementInput {
                subject: body.subject,
                description: body.description,
                status: body.status,
                priority: body.priority,
                assigned_to: body.assigned_to,
                parent_id: body.parent_id,
                milestone_id: body.milestone_id,
                autopilot: body.autopilot,
            },
        )
        .await?;
    maybe_autopilot(&state, &user.id, &id).await;
    Ok(Json(ApiResponse::ok(())))
}

async fn delete_requirement(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state.service.delete_requirement(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn list_comments(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<RequirementCommentDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_comments(&id).await?)))
}

#[derive(Deserialize)]
struct CreateCommentBody {
    body: String,
}

async fn create_comment(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<CreateCommentBody>,
) -> Result<Json<ApiResponse<RequirementCommentDto>>, DevopsError> {
    let created = state
        .service
        .create_comment(&id, &user.id, &user.username, &body.body)
        .await?;
    Ok(Json(ApiResponse::ok(created)))
}

// -- orchestration (A1 dispatch) ------------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DispatchResult {
    conversation_id: String,
    run_id: String,
}

/// Dispatch a requirement to its assigned digital employee: run the employee
/// with the requirement as task context, record the run linkage as an
/// agent-authored comment, and advance the status to `developing`.
///
/// L1 constraint: `assigned_to` must be one of the caller's own personal
/// digital employees (one-employee enforces owner isolation inside
/// `run_now_with_context`). Team-shared employees are a later layer.
async fn dispatch_requirement(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DispatchResult>>, DevopsError> {
    let result = dispatch_core(&state, &user.id, &id).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Core dispatch: run the requirement's assigned digital employee with the
/// requirement as task context, record the run linkage as an agent comment,
/// and advance a pre-dev status to `developing`. Shared by the manual
/// dispatch endpoint and autopilot. Errors with `BadRequest` when the
/// requirement has no assigned employee.
async fn dispatch_core(state: &OneDevopsRouterState, user_id: &str, id: &str) -> Result<DispatchResult, DevopsError> {
    let employee = state
        .employee
        .as_ref()
        .ok_or_else(|| DevopsError::Internal("employee runtime not wired".into()))?;

    let req = state.service.get_requirement_row(id).await?;
    let assigned_to = req
        .assigned_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DevopsError::BadRequest("requirement has no assigned digital employee".into()))?;

    let mut task_context = build_task_context(&req);

    // M2-RAG: enrich the employee's task with team knowledge. Strictly
    // best-effort — RAG unconfigured, embedding endpoint down, or an empty
    // index must never block a dispatch (and standalone mode has no RAG).
    let rag_query = format!("{} {}", req.subject, req.description.as_deref().unwrap_or(""));
    if let Ok(hits) = state.service.search_rag(&rag_query, 3).await {
        let relevant: Vec<_> = hits.into_iter().filter(|h| h.score >= 0.35).collect();
        if !relevant.is_empty() {
            task_context.push_str("\n\n——团队知识库参考（自动检索，按相关度）——\n");
            for hit in &relevant {
                task_context.push_str(&format!("\n【{}】\n{}\n", hit.document_title, hit.content));
            }
        }
    }

    let tenant = state.tenant_of(user_id).await;
    let (run_id, conversation_id) = employee
        .run_now_with_context(user_id, &tenant, assigned_to, task_context)
        .await
        .map_err(|e| match e {
            one_employee::EmployeeError::NotFound => DevopsError::BadRequest(
                "assigned digital employee is not available to you (not your employee, and not shared within your team)".into(),
            ),
            other => DevopsError::Internal(format!("dispatch run: {other}")),
        })?;

    let metadata = serde_json::json!({ "conversationId": conversation_id, "runId": run_id }).to_string();
    let body = format!("已派发给数字员工，运行中（会话 {conversation_id}）");
    state
        .service
        .insert_agent_comment(id, "agent", Some(assigned_to), "数字员工", &body, Some(metadata))
        .await?;

    if req.status == "backlog" || req.status == "planning" {
        state
            .service
            .update_requirement(
                id,
                UpdateRequirementInput {
                    status: Some("developing".into()),
                    ..Default::default()
                },
            )
            .await?;
    }

    Ok(DispatchResult {
        conversation_id,
        run_id,
    })
}

/// Best-effort autopilot (A1 L3): after a create/update, if the requirement
/// has autopilot on, an assigned employee, and is still in a pre-dev status,
/// auto-dispatch it. Silent no-op when conditions aren't met; failures are
/// logged, never surfaced — autopilot must not fail the originating request.
///
/// Re-entrancy is self-guarding: a successful dispatch advances the status to
/// `developing`, so the `backlog`/`planning` gate stops it from firing again
/// until the user deliberately moves the requirement back.
async fn maybe_autopilot(state: &OneDevopsRouterState, user_id: &str, id: &str) {
    if state.employee.is_none() {
        return;
    }
    let Ok(req) = state.service.get_requirement_row(id).await else {
        return;
    };
    if !req.autopilot {
        return;
    }
    if req
        .assigned_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return;
    }
    if req.status != "backlog" && req.status != "planning" {
        return;
    }
    if let Err(e) = dispatch_core(state, user_id, id).await {
        tracing::warn!(requirement = id, error = %e, "autopilot dispatch failed");
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BreakdownResult {
    conversation_id: String,
    run_id: String,
    created: Vec<RequirementDto>,
}

/// Break a requirement down into child requirements (A1 L2): run the assigned
/// digital employee with a structured breakdown prompt, parse its reply into
/// child items, batch-create them under this requirement, and record the run
/// linkage as an agent-authored comment.
///
/// Same L1 ownership constraint as dispatch: `assigned_to` must be one of the
/// caller's own personal digital employees.
async fn breakdown_requirement(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<BreakdownResult>>, DevopsError> {
    let employee = state
        .employee
        .as_ref()
        .ok_or_else(|| DevopsError::Internal("employee runtime not wired".into()))?;

    let req = state.service.get_requirement_row(&id).await?;
    let assigned_to = req
        .assigned_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DevopsError::BadRequest("requirement has no assigned digital employee".into()))?;

    let prompt = crate::breakdown::build_breakdown_prompt(&req);
    let tenant = state.tenant_of(&user.id).await;
    let run = employee
        .run_prompt_blocking(&user.id, &tenant, assigned_to, prompt)
        .await
        .map_err(|e| match e {
            one_employee::EmployeeError::NotFound => DevopsError::BadRequest(
                "assigned digital employee is not available to you (not your employee, and not shared within your team)".into(),
            ),
            other => DevopsError::Internal(format!("breakdown run: {other}")),
        })?;

    let items = crate::breakdown::parse_breakdown_items(&run.reply);
    if items.is_empty() {
        // Record the failure so the run linkage is not lost, then surface it.
        let metadata = serde_json::json!({ "conversationId": run.conversation_id, "runId": run.run_id }).to_string();
        state
            .service
            .insert_agent_comment(
                &id,
                "agent",
                Some(assigned_to),
                "数字员工",
                "自动拆解未能从回复中解析出子需求，请重试或手动拆解。",
                Some(metadata),
            )
            .await?;
        return Err(DevopsError::BadRequest("未能从数字员工回复中解析出子需求".into()));
    }

    let created = state
        .service
        .create_breakdown_children(&id, &user.id, Some(user.username.as_str()), &items)
        .await?;

    let child_ids: Vec<&str> = created.iter().map(|c| c.id.as_str()).collect();
    let metadata = serde_json::json!({
        "conversationId": run.conversation_id,
        "runId": run.run_id,
        "childIds": child_ids,
    })
    .to_string();
    let body = format!(
        "已自动拆解为 {} 条子需求（会话 {}）",
        created.len(),
        run.conversation_id
    );
    state
        .service
        .insert_agent_comment(&id, "agent", Some(assigned_to), "数字员工", &body, Some(metadata))
        .await?;

    Ok(Json(ApiResponse::ok(BreakdownResult {
        conversation_id: run.conversation_id,
        run_id: run.run_id,
        created,
    })))
}

/// Compose the requirement into a task prompt appended to the employee's own
/// run prompt. Kept plain-text so any agent backend can consume it.
fn build_task_context(req: &crate::models::RequirementRow) -> String {
    let mut out = String::new();
    out.push_str("你收到一条协作看板需求，请完成它并输出可交付摘要。\n\n");
    out.push_str(&format!("标题：{}\n", req.subject));
    out.push_str(&format!("类型：{} · 优先级：{}\n", req.r#type, req.priority));
    if let Some(desc) = req.description.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("\n描述：\n{desc}\n"));
    }
    out
}

// -- registries -----------------------------------------------------------

/// Distribution-policy gate: registry WRITES (skills / MCP / RAG) define what
/// gets distributed to every member's machine, so inside an enterprise they
/// are admin-only. A user with no org row (standalone / personal mode, or a
/// member's own local backend) is the machine owner and passes.
///
/// Reads (list/search) and collaboration surfaces (requirements / comments /
/// dispatch / milestones / test plans / pipelines) stay member-open.
async fn require_registry_admin(state: &OneDevopsRouterState, user_id: &str) -> Result<(), DevopsError> {
    match state.service.user_org_role(user_id).await? {
        None => Ok(()),
        Some(role) if role == "org_admin" || role == "system_admin" || role == "admin" => Ok(()),
        Some(_) => Err(DevopsError::Forbidden(
            "registry writes are admin-only: distributed skills/MCP/knowledge affect every member".into(),
        )),
    }
}

async fn list_skills(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<SkillRegistryDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_skills().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpsertSkillBody {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    content: String,
    #[serde(default = "default_true")]
    enabled: bool,
    /// Mixed distribution model: admin marks the skill as auto-active for
    /// member agents. Defaults to opt-in (false).
    #[serde(default)]
    auto_active: bool,
}

fn default_true() -> bool {
    true
}

async fn upsert_skill(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<UpsertSkillBody>,
) -> Result<Json<ApiResponse<SkillRegistryDto>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    let dto = state
        .service
        .upsert_skill(
            body.id.as_deref(),
            &body.name,
            &body.description,
            &body.content,
            body.enabled,
            body.auto_active,
            &user.id,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_skill(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    state.service.delete_skill(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn list_mcp(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<McpRegistryDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_mcp_registry().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpsertMcpBody {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default = "default_stdio", rename = "type")]
    kind: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    has_keys: bool,
}

fn default_stdio() -> String {
    "stdio".into()
}

async fn upsert_mcp(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<UpsertMcpBody>,
) -> Result<Json<ApiResponse<McpRegistryDto>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    let dto = state
        .service
        .upsert_mcp_registry(
            body.id.as_deref(),
            &body.name,
            &body.kind,
            &body.endpoint,
            body.enabled,
            body.has_keys,
            &user.id,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_mcp(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    state.service.delete_mcp_registry(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn list_rag(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<RagDocumentDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_rag_documents().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterRagBody {
    title: String,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
    #[serde(default)]
    mime_type: Option<String>,
}

async fn register_rag(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<RegisterRagBody>,
) -> Result<Json<ApiResponse<RagDocumentDto>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    let dto = state
        .service
        .register_rag_document(
            &body.title,
            body.file_path.as_deref(),
            body.file_size,
            body.mime_type.as_deref(),
            &user.id,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_rag(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    state.service.delete_rag_document(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct SetRagContentBody {
    content: String,
}

async fn set_rag_content(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<SetRagContentBody>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    state.service.set_document_content(&id, &body.content).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessResult {
    chunk_count: i64,
}

async fn process_rag(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ProcessResult>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    let chunk_count = state.service.process_rag_document(&id).await?;
    Ok(Json(ApiResponse::ok(ProcessResult { chunk_count })))
}

async fn get_rag_config(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<RagConfigDto>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.get_rag_config().await?)))
}

/// `apiKey` absent = keep stored key; present = replace.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetRagConfigBody {
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
}

async fn set_rag_config(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<SetRagConfigBody>,
) -> Result<Json<ApiResponse<RagConfigDto>>, DevopsError> {
    require_registry_admin(&state, &user.id).await?;
    let dto = state
        .service
        .set_rag_config(&body.base_url, &body.model, body.api_key.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRagBody {
    query: String,
    #[serde(default)]
    top_k: Option<usize>,
}

async fn search_rag(
    State(state): State<OneDevopsRouterState>,
    Json(body): Json<SearchRagBody>,
) -> Result<Json<ApiResponse<Vec<RagSearchHit>>>, DevopsError> {
    let hits = state.service.search_rag(&body.query, body.top_k.unwrap_or(5)).await?;
    Ok(Json(ApiResponse::ok(hits)))
}

// -- milestones -----------------------------------------------------------

async fn list_milestones(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<MilestoneDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_milestones().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateMilestoneBody {
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    due_at: Option<i64>,
}

async fn create_milestone(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CreateMilestoneBody>,
) -> Result<Json<ApiResponse<MilestoneDto>>, DevopsError> {
    let dto = state
        .service
        .create_milestone(
            &user.id,
            Some(user.username.as_str()),
            &body.title,
            body.description.as_deref(),
            body.due_at,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

/// PATCH body: absent field = keep, `null` = clear (for nullable columns).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateMilestoneBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, with = "double_option")]
    description: Option<Option<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, with = "double_option")]
    due_at: Option<Option<i64>>,
}

async fn update_milestone(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMilestoneBody>,
) -> Result<Json<ApiResponse<MilestoneDto>>, DevopsError> {
    let dto = state
        .service
        .update_milestone(
            &id,
            body.title.as_deref(),
            body.description.as_ref().map(|d| d.as_deref()),
            body.status.as_deref(),
            body.due_at,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_milestone(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state.service.delete_milestone(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

// -- test plans -----------------------------------------------------------

async fn list_test_plans(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<TestPlanDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_test_plans().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTestPlanBody {
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    requirement_id: Option<String>,
}

async fn create_test_plan(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CreateTestPlanBody>,
) -> Result<Json<ApiResponse<TestPlanDto>>, DevopsError> {
    let dto = state
        .service
        .create_test_plan(
            &user.id,
            Some(user.username.as_str()),
            &body.title,
            body.description.as_deref(),
            body.requirement_id.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateTestPlanBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, with = "double_option")]
    description: Option<Option<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, with = "double_option")]
    requirement_id: Option<Option<String>>,
}

async fn update_test_plan(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTestPlanBody>,
) -> Result<Json<ApiResponse<TestPlanDto>>, DevopsError> {
    let dto = state
        .service
        .update_test_plan(
            &id,
            body.title.as_deref(),
            body.description.as_ref().map(|d| d.as_deref()),
            body.status.as_deref(),
            body.requirement_id.as_ref().map(|r| r.as_deref()),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_test_plan(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state.service.delete_test_plan(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

// -- test cases -----------------------------------------------------------

async fn list_test_cases(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<TestCaseDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_test_cases(&id).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTestCaseBody {
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    steps: Option<String>,
    #[serde(default)]
    expected: Option<String>,
}

async fn create_test_case(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(plan_id): Path<String>,
    Json(body): Json<CreateTestCaseBody>,
) -> Result<Json<ApiResponse<TestCaseDto>>, DevopsError> {
    let dto = state
        .service
        .create_test_case(
            &plan_id,
            &user.id,
            Some(user.username.as_str()),
            &body.title,
            body.description.as_deref(),
            body.steps.as_deref(),
            body.expected.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateTestCaseBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, with = "double_option")]
    description: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    steps: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    expected: Option<Option<String>>,
}

async fn update_test_case(
    State(state): State<OneDevopsRouterState>,
    Path((_, id)): Path<(String, String)>,
    Json(body): Json<UpdateTestCaseBody>,
) -> Result<Json<ApiResponse<TestCaseDto>>, DevopsError> {
    let dto = state
        .service
        .update_test_case(
            &id,
            body.title.as_deref(),
            body.status.as_deref(),
            body.description.as_ref().map(|d| d.as_deref()),
            body.steps.as_ref().map(|s| s.as_deref()),
            body.expected.as_ref().map(|e| e.as_deref()),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_test_case(
    State(state): State<OneDevopsRouterState>,
    Path((_, id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state.service.delete_test_case(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

// -- pipelines ------------------------------------------------------------

async fn list_pipelines(
    State(state): State<OneDevopsRouterState>,
) -> Result<Json<ApiResponse<Vec<PipelineDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_pipelines().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatePipelineBody {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    trigger: Option<String>,
}

async fn create_pipeline(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CreatePipelineBody>,
) -> Result<Json<ApiResponse<PipelineDto>>, DevopsError> {
    let dto = state
        .service
        .create_pipeline(
            &user.id,
            Some(user.username.as_str()),
            &body.name,
            body.description.as_deref(),
            body.trigger.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePipelineBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, with = "double_option")]
    description: Option<Option<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    trigger: Option<String>,
}

async fn update_pipeline(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
    Json(body): Json<UpdatePipelineBody>,
) -> Result<Json<ApiResponse<PipelineDto>>, DevopsError> {
    let dto = state
        .service
        .update_pipeline(
            &id,
            body.name.as_deref(),
            body.description.as_ref().map(|d| d.as_deref()),
            body.status.as_deref(),
            body.trigger.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_pipeline(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state.service.delete_pipeline(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

// -- pipeline runs --------------------------------------------------------

async fn list_pipeline_runs(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<PipelineRunDto>>>, DevopsError> {
    Ok(Json(ApiResponse::ok(state.service.list_pipeline_runs(&id).await?)))
}

async fn create_pipeline_run(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(pipeline_id): Path<String>,
) -> Result<Json<ApiResponse<PipelineRunDto>>, DevopsError> {
    let dto = state
        .service
        .create_pipeline_run(&pipeline_id, Some(user.username.as_str()))
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePipelineRunBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default, with = "double_option")]
    started_at: Option<Option<i64>>,
    #[serde(default, with = "double_option")]
    finished_at: Option<Option<i64>>,
    #[serde(default, with = "double_option")]
    log: Option<Option<String>>,
}

async fn update_pipeline_run(
    State(state): State<OneDevopsRouterState>,
    Path((_, id)): Path<(String, String)>,
    Json(body): Json<UpdatePipelineRunBody>,
) -> Result<Json<ApiResponse<PipelineRunDto>>, DevopsError> {
    let dto = state
        .service
        .update_pipeline_run(
            &id,
            body.status.as_deref(),
            body.started_at,
            body.finished_at,
            body.log.as_ref().map(|l| l.as_deref()),
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}
