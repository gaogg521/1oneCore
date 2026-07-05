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
use crate::models::{McpRegistryDto, RagDocumentDto, RequirementCommentDto, RequirementDto, SkillRegistryDto};
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
        .route("/api/one/devops/skills", get(list_skills).post(upsert_skill))
        .route("/api/one/devops/skills/{id}", axum::routing::delete(delete_skill))
        .route("/api/one/devops/mcp-registry", get(list_mcp).post(upsert_mcp))
        .route("/api/one/devops/mcp-registry/{id}", axum::routing::delete(delete_mcp))
        .route("/api/one/devops/rag/documents", get(list_rag).post(register_rag))
        .route("/api/one/devops/rag/documents/{id}", axum::routing::delete(delete_rag))
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
