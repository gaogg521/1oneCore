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
    McpRegistryDto, MilestoneDto, RagDocumentDto, RequirementCommentDto, RequirementDto, SkillRegistryDto,
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
        .route("/api/one/devops/requirements/{id}/dispatch", axum::routing::post(dispatch_requirement))
        .route("/api/one/devops/skills", get(list_skills).post(upsert_skill))
        .route("/api/one/devops/skills/{id}", axum::routing::delete(delete_skill))
        .route("/api/one/devops/mcp-registry", get(list_mcp).post(upsert_mcp))
        .route("/api/one/devops/mcp-registry/{id}", axum::routing::delete(delete_mcp))
        .route("/api/one/devops/rag/documents", get(list_rag).post(register_rag))
        .route("/api/one/devops/rag/documents/{id}", axum::routing::delete(delete_rag))
        .route("/api/one/devops/milestones", get(list_milestones).post(create_milestone))
        .route(
            "/api/one/devops/milestones/{id}",
            patch(update_milestone).delete(delete_milestone),
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
}

async fn create_requirement(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CreateRequirementBody>,
) -> Result<Json<ApiResponse<RequirementDto>>, DevopsError> {
    let created = state
        .service
        .create_requirement(&user.id, Some(user.username.as_str()), CreateRequirementInput {
            parent_id: body.parent_id,
            kind: body.kind,
            subject: body.subject,
            description: body.description,
            priority: body.priority,
            milestone_id: body.milestone_id,
        })
        .await?;
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
    Path(id): Path<String>,
    Json(body): Json<UpdateRequirementBody>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state
        .service
        .update_requirement(&id, UpdateRequirementInput {
            subject: body.subject,
            description: body.description,
            status: body.status,
            priority: body.priority,
            assigned_to: body.assigned_to,
            parent_id: body.parent_id,
            milestone_id: body.milestone_id,
        })
        .await?;
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

    let task_context = build_task_context(&req);

    let (run_id, conversation_id) = employee
        .run_now_with_context(&user.id, assigned_to, task_context)
        .await
        .map_err(|e| match e {
            one_employee::EmployeeError::NotFound => DevopsError::BadRequest(
                "assigned digital employee not found among your employees (team-shared employees are not supported yet)".into(),
            ),
            other => DevopsError::Internal(format!("dispatch run: {other}")),
        })?;

    let metadata = serde_json::json!({ "conversationId": conversation_id, "runId": run_id }).to_string();
    let body = format!("已派发给数字员工，运行中（会话 {conversation_id}）");
    state
        .service
        .insert_agent_comment(&id, "agent", Some(assigned_to), "数字员工", &body, Some(metadata))
        .await?;

    if req.status == "backlog" || req.status == "planning" {
        state
            .service
            .update_requirement(&id, UpdateRequirementInput {
                status: Some("developing".into()),
                ..Default::default()
            })
            .await?;
    }

    Ok(Json(ApiResponse::ok(DispatchResult { conversation_id, run_id })))
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
}

fn default_true() -> bool {
    true
}

async fn upsert_skill(
    State(state): State<OneDevopsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<UpsertSkillBody>,
) -> Result<Json<ApiResponse<SkillRegistryDto>>, DevopsError> {
    let dto = state
        .service
        .upsert_skill(
            body.id.as_deref(),
            &body.name,
            &body.description,
            &body.content,
            body.enabled,
            &user.id,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn delete_skill(
    State(state): State<OneDevopsRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
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
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
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
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, DevopsError> {
    state.service.delete_rag_document(&id).await?;
    Ok(Json(ApiResponse::ok(())))
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
        .create_milestone(&user.id, Some(user.username.as_str()), &body.title, body.description.as_deref(), body.due_at)
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
