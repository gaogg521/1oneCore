//! `/api/one/admin/platform/*` routes — deployment infrastructure config
//! (P1-3 container runtime + P2-2 realtime collaboration).
//!
//! Mounted behind the upstream `auth_middleware` (relies on `CurrentUser` in
//! request extensions). All routes are gated by `RequirePlatformAdmin`.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use aionui_api_types::ApiResponse;

use crate::collaboration::CollaborationStatus;
use crate::container::ContainerStatus;
use crate::error::PlatformError;
use crate::models::{CollaborationConfigDto, ContainerConfigDto};
use crate::rbac::RequirePlatformAdmin;
use crate::state::OnePlatformRouterState;

pub fn one_platform_routes(state: OnePlatformRouterState) -> Router {
    Router::new()
        .route(
            "/api/one/admin/platform/container",
            get(get_container).put(set_container),
        )
        .route("/api/one/admin/platform/container/probe", post(probe_container))
        .route(
            "/api/one/admin/platform/collaboration",
            get(get_collaboration).put(set_collaboration),
        )
        .route(
            "/api/one/admin/platform/collaboration/probe",
            post(probe_collaboration),
        )
        .with_state(state)
}

// --- P1-3 container runtime ---

async fn get_container(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<ContainerConfigDto>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_container_config(&actor.tenant_id).await?,
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetContainerBody {
    #[serde(default)]
    runtime_kind: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    default_image: Option<String>,
    #[serde(default)]
    registry: Option<String>,
    /// Absent/empty = keep the stored registry secret.
    #[serde(default)]
    registry_secret: Option<String>,
    #[serde(default)]
    enabled: bool,
}

async fn set_container(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
    Json(body): Json<SetContainerBody>,
) -> Result<Json<ApiResponse<ContainerConfigDto>>, PlatformError> {
    let dto = state
        .service
        .set_container_config(
            &actor.tenant_id,
            body.runtime_kind.as_deref(),
            body.endpoint.as_deref(),
            body.default_image.as_deref(),
            body.registry.as_deref(),
            body.registry_secret.as_deref(),
            body.enabled,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn probe_container(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<ContainerStatus>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state.service.probe_container(&actor.tenant_id).await?,
    )))
}

// --- P2-2 realtime collaboration ---

async fn get_collaboration(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<CollaborationConfigDto>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_collaboration_config(&actor.tenant_id).await?,
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetCollaborationBody {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    /// Absent/empty = keep the stored secret.
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    presence: bool,
    #[serde(default)]
    enabled: bool,
}

async fn set_collaboration(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
    Json(body): Json<SetCollaborationBody>,
) -> Result<Json<ApiResponse<CollaborationConfigDto>>, PlatformError> {
    let dto = state
        .service
        .set_collaboration_config(
            &actor.tenant_id,
            body.provider.as_deref(),
            body.endpoint.as_deref(),
            body.secret.as_deref(),
            body.presence,
            body.enabled,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn probe_collaboration(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<CollaborationStatus>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state.service.probe_collaboration(&actor.tenant_id).await?,
    )))
}
