//! `/api/one/org/*` and `/api/one/admin/*` routes.
//!
//! The whole router must be mounted behind the upstream `auth_middleware`
//! (it relies on `CurrentUser` in request extensions). The `/api/one/*`
//! prefix keeps our namespace disjoint from upstream `/api/*` routes.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::ApiResponse;

use crate::error::OrgError;
use crate::models::{
    AdminUserDto, AuditLogRow, EnterpriseTenantDto, InviteDto, OrgContextDto, ResetLocalResult, RuntimeNodeDto,
    is_enterprise_tenant_id, is_system_admin_role,
};
use crate::rbac::{OrgActor, RequireOrgAdmin};
use crate::state::OneOrgRouterState;

pub fn one_org_routes(state: OneOrgRouterState) -> Router {
    Router::new()
        .route("/api/one/org/context", get(org_context))
        .route("/api/one/org/public-info", get(org_public_info))
        .route("/api/one/org/invites/preview", post(org_preview_invite))
        .route("/api/one/org/join", post(org_join))
        .route("/api/one/org/members", get(org_members))
        .route("/api/one/org/invites", get(org_invites))
        .route("/api/one/org/exit", post(org_exit))
        .route("/api/one/org/create", post(org_create))
        .route("/api/one/org/reset-local", post(org_reset_local))
        .route(
            "/api/one/admin/invites",
            get(admin_list_invites).post(admin_create_invite),
        )
        .route("/api/one/admin/invites/{invite_id}/revoke", post(admin_revoke_invite))
        .route(
            "/api/one/admin/exit-password",
            get(admin_exit_password_status)
                .put(admin_set_exit_password)
                .delete(admin_clear_exit_password),
        )
        // M2e: user management + audit + runtime nodes
        .route("/api/one/admin/users", get(admin_list_users))
        .route("/api/one/admin/users/{user_id}/role", put(admin_set_user_role))
        .route("/api/one/admin/audit", get(admin_list_audit))
        .route("/api/one/admin/runtime/nodes", get(admin_list_runtime_nodes))
        .route("/api/one/admin/runtime/heartbeat", post(admin_runtime_heartbeat))
        // Direction B: company-scoped project-group management. Gated
        // system_admin OR company-admin of the path `enterprise_id`.
        .route(
            "/api/one/org/enterprise/{enterprise_id}/tenants",
            get(enterprise_list_tenants).post(enterprise_create_tenant),
        )
        .with_state(state)
}

// --- company-scoped project groups (Direction B) ---

/// Authorize a company governor: instance system_admin, or an admin of the
/// target company (via the app-wired bridge). Personal edition has no bridge
/// and no company, so these routes are unreachable there.
async fn ensure_company_governor(
    state: &OneOrgRouterState,
    actor: &OrgActor,
    enterprise_id: &str,
) -> Result<(), OrgError> {
    if is_system_admin_role(&actor.role) {
        return Ok(());
    }
    if let Some(resolver) = state.company_resolver.as_ref()
        && resolver.is_company_admin(&actor.user_id, enterprise_id).await
    {
        return Ok(());
    }
    Err(OrgError::Forbidden("Company administrator role required".into()))
}

async fn enterprise_list_tenants(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Path(enterprise_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<EnterpriseTenantDto>>>, OrgError> {
    ensure_company_governor(&state, &actor, &enterprise_id).await?;
    let tenants = state.service.list_tenants_by_enterprise(&enterprise_id).await?;
    Ok(Json(ApiResponse::ok(tenants)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateEnterpriseTenantBody {
    name: String,
    #[serde(default)]
    initial_admin_user_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateEnterpriseTenantDto {
    tenant_id: String,
    name: String,
    invite_code: String,
}

async fn enterprise_create_tenant(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Path(enterprise_id): Path<String>,
    Json(body): Json<CreateEnterpriseTenantBody>,
) -> Result<Json<ApiResponse<CreateEnterpriseTenantDto>>, OrgError> {
    ensure_company_governor(&state, &actor, &enterprise_id).await?;
    let (tenant_id, name, invite_code) = state
        .service
        .create_tenant_for_enterprise(
            &enterprise_id,
            &body.name,
            &actor.user_id,
            body.initial_admin_user_id.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(CreateEnterpriseTenantDto {
        tenant_id,
        name,
        invite_code,
    })))
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

/// Read-only tenant roster for any enterprise member (client-mode terminals
/// see their team without admin rights). Mutations stay on `/api/one/admin/*`
/// behind `RequireOrgAdmin`.
async fn org_members(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
) -> Result<Json<ApiResponse<Vec<AdminUserDto>>>, OrgError> {
    if !is_enterprise_tenant_id(&actor.tenant_id) {
        return Err(OrgError::NotInEnterprise);
    }
    let users = state.service.list_users(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(users)))
}

/// Read-only invite list for any enterprise member. Members accepted this
/// visibility trade-off (codes are shown so a member can re-share them);
/// creation/revocation remain admin-only.
async fn org_invites(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
) -> Result<Json<ApiResponse<Vec<InviteDto>>>, OrgError> {
    if !is_enterprise_tenant_id(&actor.tenant_id) {
        return Err(OrgError::NotInEnterprise);
    }
    let invites = state.service.list_invites(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(invites)))
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

/// Archive and wipe stale local tenant/membership data left behind on this
/// machine, clearing the way for `org_create` to succeed again. See
/// `OrgService::reset_local_enterprise` for what gets archived and deleted.
async fn org_reset_local(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
) -> Result<Json<ApiResponse<ResetLocalResult>>, OrgError> {
    let result = state.service.reset_local_enterprise(&actor.user_id).await?;
    Ok(Json(ApiResponse::ok(result)))
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
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.invite.create",
            Some(&invite.id),
        )
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
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.invite.revoke",
            Some(&invite_id),
        )
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
    state
        .service
        .set_exit_password(&actor.tenant_id, &body.password)
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.exit_password.set",
            None,
        )
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
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.exit_password.clear",
            None,
        )
        .await;
    Ok(Json(ApiResponse::ok(())))
}

// --- M2e: admin users / audit / runtime nodes ---

async fn admin_list_users(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<AdminUserDto>>>, OrgError> {
    let users = state.service.list_users(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(users)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetRoleBody {
    role: String,
}

async fn admin_set_user_role(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(user_id): Path<String>,
    Json(body): Json<SetRoleBody>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    let role = body.role.trim();
    if !matches!(role, "member" | "org_admin" | "system_admin") {
        return Err(OrgError::BadRequest(format!("invalid role: {role}")));
    }
    // system_admin can only be set by an existing system_admin.
    if role == "system_admin" && !is_system_admin_role(&actor.role) {
        return Err(OrgError::Forbidden(
            "only system_admin can promote to system_admin".into(),
        ));
    }
    state
        .service
        .set_user_role(&actor.tenant_id, &actor.user_id, &user_id, role)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: i64,
}

fn default_audit_limit() -> i64 {
    100
}

async fn admin_list_audit(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Query(query): Query<ListAuditQuery>,
) -> Result<Json<ApiResponse<Vec<AuditLogRow>>>, OrgError> {
    let logs = state.service.list_audit_logs(&actor.tenant_id, query.limit).await?;
    Ok(Json(ApiResponse::ok(logs)))
}

async fn admin_list_runtime_nodes(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<RuntimeNodeDto>>>, OrgError> {
    let nodes = state.service.list_runtime_nodes(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(nodes)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatBody {
    machine_id: String,
    display_name: String,
    #[serde(default)]
    hostnames: serde_json::Value,
    #[serde(default)]
    ip_addresses: serde_json::Value,
    #[serde(default)]
    installed_agents: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatDto {
    node_id: String,
}

// Any enterprise member's machine reports in here, not just admins' — the
// whole point of the runtime-node roster is fleet-wide visibility (which
// machines have which agents installed). Gating this to `RequireOrgAdmin`
// meant a regular member's machine could never heartbeat at all (403), so
// the roster could only ever show admins' own machines.
async fn admin_runtime_heartbeat(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Json(body): Json<HeartbeatBody>,
) -> Result<Json<ApiResponse<HeartbeatDto>>, OrgError> {
    if !is_enterprise_tenant_id(&actor.tenant_id) {
        return Err(OrgError::NotInEnterprise);
    }
    let machine_id = body.machine_id.trim();
    if machine_id.is_empty() {
        return Err(OrgError::BadRequest("machineId is required".into()));
    }
    let node_id = state
        .service
        .heartbeat_runtime_node(
            &actor.tenant_id,
            &actor.user_id,
            machine_id,
            &body.display_name,
            &body.hostnames,
            &body.ip_addresses,
            &body.installed_agents,
        )
        .await?;
    Ok(Json(ApiResponse::ok(HeartbeatDto { node_id })))
}
