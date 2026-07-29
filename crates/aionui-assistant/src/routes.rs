#![allow(clippy::disallowed_types)]

//! HTTP route handlers for `/api/assistants/*`.

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, patch, post};

use aionui_api_types::{
    ApiResponse, AssistantDetailResponse, AssistantResponse, CreateAssistantRequest, ImportAssistantsRequest,
    ImportAssistantsResult, MarketplacePersonaResponse, SetAssistantStateRequest, UpdateAssistantRequest,
};
use aionui_common::ApiError;

use crate::error::AssistantError;
pub use crate::state::AssistantRouterState;

/// Build the router for `/api/assistants/*`.
pub fn assistant_routes(state: AssistantRouterState) -> Router {
    Router::new()
        .route("/api/assistants", get(list).post(create))
        .route("/api/assistants/{id}", get(get_one).put(update).delete(delete_one))
        .route("/api/assistants/{id}/state", patch(set_state))
        .route("/api/assistants/{id}/avatar", get(get_avatar))
        .route("/api/assistants/import", post(import))
        .route("/api/assistants/import-personas", post(import_personas))
        .route("/api/assistants/marketplace", get(marketplace_list))
        .route("/api/assistants/marketplace/{id}/install", post(marketplace_install))
        .route("/api/assistants/marketplace/{id}/avatar", get(marketplace_avatar))
        .with_state(state)
}

#[derive(Debug, serde::Deserialize, Default)]
struct GetAssistantDetailQuery {
    locale: Option<String>,
}

impl From<AssistantError> for ApiError {
    fn from(error: AssistantError) -> Self {
        match error {
            AssistantError::NotFound(message) => Self::NotFound(message),
            AssistantError::BadRequest(message) => Self::BadRequest(message),
            AssistantError::Forbidden(message) => Self::Forbidden(message),
            AssistantError::Conflict(message) => Self::Conflict(message),
            AssistantError::Internal(message) => Self::Internal(message),
            // Only produced by startup assistant-storage bootstrap (never on an
            // HTTP path); treated as a transient internal condition if it ever
            // surfaces through the API boundary.
            AssistantError::ConcurrentBootstrapContention(message) => Self::Internal(message),
        }
    }
}

async fn list(
    State(state): State<AssistantRouterState>,
) -> Result<Json<ApiResponse<Vec<AssistantResponse>>>, ApiError> {
    let items = state.service.list().await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create(
    State(state): State<AssistantRouterState>,
    body: Result<Json<CreateAssistantRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AssistantResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let created = state.service.create(req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn get_one(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
    Query(query): Query<GetAssistantDetailQuery>,
) -> Result<Json<ApiResponse<AssistantDetailResponse>>, ApiError> {
    let detail = state.service.get_detail(&id, query.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(detail)))
}

async fn update(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAssistantRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let updated = state.service.update(&id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn delete_one(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete(&id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn set_state(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
    body: Result<Json<SetAssistantStateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let resp = state.service.set_state(&id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn import(
    State(state): State<AssistantRouterState>,
    body: Result<Json<ImportAssistantsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportAssistantsResult>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.service.import(req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Bulk upsert-by-`id` import of persona assistants (e.g. Claude Code
/// sub-agent `.md` files). Unlike `import`, re-importing the same `id`
/// overwrites the existing row instead of skipping it.
async fn import_personas(
    State(state): State<AssistantRouterState>,
    body: Result<Json<ImportAssistantsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportAssistantsResult>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.service.import_personas(req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Browse the expert marketplace catalog. Read-only — never touches the
/// caller's own assistant list.
async fn marketplace_list(
    State(state): State<AssistantRouterState>,
) -> Result<Json<ApiResponse<Vec<MarketplacePersonaResponse>>>, ApiError> {
    let entries = state
        .marketplace_repo
        .list()
        .await
        .map_err(|e| ApiError::Internal(format!("list marketplace personas: {e}")))?;

    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let installed = state.service.exists(&entry.id).await?;
        let avatar = entry
            .has_avatar
            .then(|| format!("/api/assistants/marketplace/{}/avatar", entry.id));
        result.push(MarketplacePersonaResponse {
            id: entry.id,
            name: entry.name,
            description: entry.description,
            installed,
            display_name: entry.display_name,
            role_name: entry.role_name,
            category: entry.category,
            avatar,
        });
    }
    Ok(Json(ApiResponse::ok(result)))
}

/// Materialize one marketplace catalog entry into a real, owned assistant.
/// Reuses the same upsert-by-id semantics as `import_personas` — installing
/// twice just re-syncs the row, it never duplicates.
async fn marketplace_install(
    State(state): State<AssistantRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AssistantResponse>>, ApiError> {
    let entry = state
        .marketplace_repo
        .get(&id)
        .await
        .map_err(|e| ApiError::Internal(format!("get marketplace persona: {e}")))?
        .ok_or_else(|| ApiError::NotFound(format!("marketplace persona '{id}' not found")))?;

    // Prefer the catalog's Chinese display name for the installed assistant's
    // actual `name` — other surfaces (quick-select chips, conversation
    // header) render `name` directly, not the marketplace card's display
    // override, so without this the assistant reverts to the raw
    // PascalCase id once installed.
    let installed_name = entry.display_name.clone().unwrap_or_else(|| entry.name.clone());

    state
        .service
        .import_personas(ImportAssistantsRequest {
            assistants: vec![CreateAssistantRequest {
                id: Some(entry.id.clone()),
                name: installed_name,
                description: entry.description,
                avatar: None,
                agent_id: None,
                enabled_skills: None,
                custom_skill_names: None,
                disabled_builtin_skills: None,
                prompts: None,
                models: None,
                name_i18n: None,
                description_i18n: None,
                prompts_i18n: None,
                recommended_prompts: None,
                recommended_prompts_i18n: None,
                defaults: None,
                rule_content: Some(entry.rule_content),
            }],
        })
        .await?;

    // `import_personas` intentionally never sets an avatar (see above) — the
    // catalog's own avatar bytes are wired in as a separate step so the
    // marketplace module stays the sole owner of its embedded assets.
    if entry.has_avatar
        && let Some(bytes) = crate::marketplace::marketplace_avatar_bytes(&entry.id)
    {
        state.service.set_avatar_from_bytes(&entry.id, &bytes, "webp").await?;
    }

    let installed = state.service.get(&entry.id).await?;
    Ok(Json(ApiResponse::ok(installed)))
}

/// Serve the raw avatar bytes for a marketplace catalog entry (not yet an
/// owned assistant — see [`marketplace_install`] for the "install" path).
async fn marketplace_avatar(Path(id): Path<String>) -> Result<Response, ApiError> {
    let bytes = crate::marketplace::marketplace_avatar_bytes(&id)
        .ok_or_else(|| ApiError::NotFound(format!("marketplace avatar '{id}' not found")))?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type_for_extension(Some("webp")))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Serve the raw avatar bytes for an assistant. Content-Type inferred from the
/// file extension (png/jpg/svg default). Extensions return 404 — the frontend
/// serves those via `aion-asset://`.
async fn get_avatar(State(state): State<AssistantRouterState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let asset = state
        .service
        .avatar_asset(&id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("avatar '{id}' not found")))?;

    let content_type = content_type_for_extension(asset.extension.as_deref());

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(asset.bytes))
        .map_err(|e| ApiError::Internal(e.to_string()))
}

fn content_type_for_extension(ext: Option<&str>) -> HeaderValue {
    let mime = match ext {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(mime)
}
