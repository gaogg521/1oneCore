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
use crate::models::{CollaborationConfigDto, ContainerConfigDto, IpAllowlistConfigDto, SiemConfigDto};
use crate::rbac::RequirePlatformAdmin;
use crate::siem::SiemStatus;
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
        .route("/api/one/admin/platform/collaboration/probe", post(probe_collaboration))
        .route(
            "/api/one/admin/platform/ip-allowlist",
            get(get_ip_allowlist).put(set_ip_allowlist),
        )
        .route("/api/one/admin/platform/ip-allowlist/check", post(check_ip_allowlist))
        .route("/api/one/admin/platform/siem", get(get_siem).put(set_siem))
        .route("/api/one/admin/platform/siem/probe", post(probe_siem))
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

// --- P1-4 IP allowlist ---

async fn get_ip_allowlist(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<IpAllowlistConfigDto>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_ip_allowlist(&actor.tenant_id).await?,
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetIpAllowlistBody {
    #[serde(default)]
    cidrs: Vec<String>,
    #[serde(default)]
    enabled: bool,
}

async fn set_ip_allowlist(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
    Json(body): Json<SetIpAllowlistBody>,
) -> Result<Json<ApiResponse<IpAllowlistConfigDto>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_ip_allowlist(&actor.tenant_id, &body.cidrs, body.enabled)
            .await?,
    )))
}

#[derive(Deserialize)]
struct CheckIpBody {
    ip: String,
}

#[derive(serde::Serialize)]
struct CheckIpResult {
    allowed: bool,
}

/// Test whether an IP would be allowed under the current allowlist — lets an
/// admin validate rules (and confirm they won't lock themselves out) before
/// enabling enforcement.
async fn check_ip_allowlist(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
    Json(body): Json<CheckIpBody>,
) -> Result<Json<ApiResponse<CheckIpResult>>, PlatformError> {
    let allowed = state.service.is_ip_allowed(&actor.tenant_id, &body.ip).await?;
    Ok(Json(ApiResponse::ok(CheckIpResult { allowed })))
}

// --- P1-4 SIEM export ---

async fn get_siem(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<SiemConfigDto>>, PlatformError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_siem_config(&actor.tenant_id).await?,
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSiemBody {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    /// Absent/empty = keep the stored token.
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    enabled: bool,
}

async fn set_siem(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
    Json(body): Json<SetSiemBody>,
) -> Result<Json<ApiResponse<SiemConfigDto>>, PlatformError> {
    let dto = state
        .service
        .set_siem_config(
            &actor.tenant_id,
            body.kind.as_deref(),
            body.endpoint.as_deref(),
            body.secret.as_deref(),
            body.enabled,
        )
        .await?;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn probe_siem(
    State(state): State<OnePlatformRouterState>,
    RequirePlatformAdmin(actor): RequirePlatformAdmin,
) -> Result<Json<ApiResponse<SiemStatus>>, PlatformError> {
    Ok(Json(ApiResponse::ok(state.service.probe_siem(&actor.tenant_id).await?)))
}
