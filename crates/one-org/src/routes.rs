//! `/api/one/org/*` and `/api/one/admin/*` routes.
//!
//! The whole router must be mounted behind the upstream `auth_middleware`
//! (it relies on `CurrentUser` in request extensions). The `/api/one/*`
//! prefix keeps our namespace disjoint from upstream `/api/*` routes.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::ApiResponse;

use crate::error::OrgError;
use crate::models::{InviteDto, OrgContextDto};
use crate::rbac::{OrgActor, RequireOrgAdmin};
use crate::state::OneOrgRouterState;

pub fn one_org_routes(state: OneOrgRouterState) -> Router {
    Router::new()
        .route("/api/one/org/context", get(org_context))
        .route("/api/one/org/public-info", get(org_public_info))
        .route("/api/one/org/invites/preview", post(org_preview_invite))
        .route("/api/one/org/join", post(org_join))
        .route("/api/one/org/exit", post(org_exit))
        .route("/api/one/org/create", post(org_create))
        .route("/api/one/admin/invites", get(admin_list_invites).post(admin_create_invite))
        .route("/api/one/admin/invites/{invite_id}/revoke", post(admin_revoke_invite))
        .route(
            "/api/one/admin/exit-password",
            get(admin_exit_password_status)
                .put(admin_set_exit_password)
                .delete(admin_clear_exit_password),
        )
        .with_state(state)
}

// --- org (member-facing) ---

async fn org_context(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
) -> Result<Json<ApiResponse<OrgContextDto>>, OrgError> {
    let ctx = state.service.context(&actor.user_id).await?;
    Ok(Json(ApiResponse::ok(ctx)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicInfoDto {
    tenant_name: Option<String>,
}

async fn org_public_info(
    State(state): State<OneOrgRouterState>,
    _actor: OrgActor,
) -> Result<Json<ApiResponse<PublicInfoDto>>, OrgError> {
    let tenant_name = state.service.public_info().await?;
    Ok(Json(ApiResponse::ok(PublicInfoDto { tenant_name })))
}

#[derive(Deserialize)]
struct InviteCodeBody {
    code: String,
}

#[derive(Serialize)]
struct PreviewDto {
    valid: bool,
}

async fn org_preview_invite(
    State(state): State<OneOrgRouterState>,
    _actor: OrgActor,
    Json(body): Json<InviteCodeBody>,
) -> Result<Json<ApiResponse<PreviewDto>>, OrgError> {
    state.service.preview_invite(&body.code).await?;
    Ok(Json(ApiResponse::ok(PreviewDto { valid: true })))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TenantDto {
    tenant_id: String,
    tenant_name: String,
}

async fn org_join(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Json(body): Json<InviteCodeBody>,
) -> Result<Json<ApiResponse<TenantDto>>, OrgError> {
    let (tenant_id, tenant_name) = state.service.join_with_invite(&actor.user_id, &body.code).await?;
    Ok(Json(ApiResponse::ok(TenantDto { tenant_id, tenant_name })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExitBody {
    exit_code: String,
}

async fn org_exit(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Json(body): Json<ExitBody>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state.service.leave(&actor.user_id, &body.exit_code).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct CreateTenantBody {
    name: String,
}

async fn org_create(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Json(body): Json<CreateTenantBody>,
) -> Result<Json<ApiResponse<TenantDto>>, OrgError> {
    let (tenant_id, tenant_name) = state.service.create_tenant(&actor.user_id, &body.name).await?;
    Ok(Json(ApiResponse::ok(TenantDto { tenant_id, tenant_name })))
}

// --- admin ---

async fn admin_list_invites(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<InviteDto>>>, OrgError> {
    let invites = state.service.list_invites(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(invites)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateInviteBody {
    max_uses: Option<i64>,
    expires_in_days: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreatedInviteDto {
    invite: InviteDto,
    display_code: String,
}

async fn admin_create_invite(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<CreateInviteBody>,
) -> Result<Json<ApiResponse<CreatedInviteDto>>, OrgError> {
    let (invite, display_code) = state
        .service
        .create_invite(&actor.tenant_id, &actor.user_id, body.max_uses, body.expires_in_days)
        .await?;
    state
        .service
        .audit(&actor.tenant_id, Some(&actor.user_id), "org.invite.create", Some(&invite.id))
        .await;
    Ok(Json(ApiResponse::ok(CreatedInviteDto { invite, display_code })))
}

async fn admin_revoke_invite(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(invite_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state.service.revoke_invite(&actor.tenant_id, &invite_id).await?;
    state
        .service
        .audit(&actor.tenant_id, Some(&actor.user_id), "org.invite.revoke", Some(&invite_id))
        .await;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExitPasswordStatusDto {
    is_set: bool,
}

async fn admin_exit_password_status(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<ExitPasswordStatusDto>>, OrgError> {
    let is_set = state.service.exit_password_status(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(ExitPasswordStatusDto { is_set })))
}

#[derive(Deserialize)]
struct SetExitPasswordBody {
    password: String,
}

async fn admin_set_exit_password(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<SetExitPasswordBody>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state.service.set_exit_password(&actor.tenant_id, &body.password).await?;
    state
        .service
        .audit(&actor.tenant_id, Some(&actor.user_id), "org.exit_password.set", None)
        .await;
    Ok(Json(ApiResponse::ok(())))
}

async fn admin_clear_exit_password(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state.service.clear_exit_password(&actor.tenant_id).await?;
    state
        .service
        .audit(&actor.tenant_id, Some(&actor.user_id), "org.exit_password.clear", None)
        .await;
    Ok(Json(ApiResponse::ok(())))
}
