//! `/api/one/org/*` and `/api/one/admin/*` routes.
//!
//! The whole router must be mounted behind the upstream `auth_middleware`
//! (it relies on `CurrentUser` in request extensions). The `/api/one/*`
//! prefix keeps our namespace disjoint from upstream `/api/*` routes.

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::ApiResponse;

use crate::email::SendEmailResult;
use crate::error::OrgError;
use crate::integration::IntegrationTestResult;
use crate::models::{
    AdminUserDto, AgentAuditEntry, AuditLogRow, DepartmentDto, DirectoryMapReport, EnterpriseTenantDto, IntegrationDto,
    InviteDto, MyTenantDto, OrgContextDto, ResetLocalResult, RuntimeNodeDto, SmtpConfigDto, TenantSummaryDto,
    is_enterprise_tenant_id, is_system_admin_role,
};
use crate::rbac::{OrgActor, RequireOrgAdmin, RequireSystemAdmin};
use crate::state::OneOrgRouterState;

pub fn one_org_routes(state: OneOrgRouterState) -> Router {
    Router::new()
        .route("/api/one/org/context", get(org_context))
        .route("/api/one/org/public-info", get(org_public_info))
        .route("/api/one/org/invites/preview", post(org_preview_invite))
        .route("/api/one/org/join", post(org_join))
        .route("/api/one/org/my-tenants", get(org_my_tenants))
        .route("/api/one/org/switch", post(org_switch))
        .route("/api/one/org/members", get(org_members))
        .route("/api/one/org/tenants", get(org_list_tenants))
        .route("/api/one/org/invites", get(org_invites))
        .route("/api/one/org/exit", post(org_exit))
        .route("/api/one/org/create", post(org_create))
        .route("/api/one/org/reset-local", post(org_reset_local))
        .route(
            "/api/one/admin/invites",
            get(admin_list_invites).post(admin_create_invite),
        )
        .route("/api/one/admin/invites/bulk", post(admin_create_invites_bulk))
        .route(
            "/api/one/admin/invites/{invite_id}/send-email",
            post(admin_send_invite_email),
        )
        .route("/api/one/admin/invites/{invite_id}/revoke", post(admin_revoke_invite))
        .route(
            "/api/one/admin/onboarding/domains",
            get(admin_get_allowed_domains).put(admin_set_allowed_domains),
        )
        .route(
            "/api/one/admin/onboarding/smtp",
            get(admin_get_smtp_config).put(admin_set_smtp_config),
        )
        .route(
            "/api/one/admin/exit-password",
            get(admin_exit_password_status)
                .put(admin_set_exit_password)
                .delete(admin_clear_exit_password),
        )
        // M2e: user management + audit + runtime nodes
        .route("/api/one/admin/users", get(admin_list_users))
        .route("/api/one/admin/users/{user_id}", delete(admin_remove_user))
        .route("/api/one/admin/users/{user_id}/role", put(admin_set_user_role))
        .route(
            "/api/one/admin/users/{user_id}/department",
            put(admin_assign_member_department),
        )
        // P1-1 backup / restore of enterprise configuration.
        .route("/api/one/admin/backup/export", get(admin_export_backup))
        .route("/api/one/admin/backup/import", post(admin_import_backup))
        .route(
            "/api/one/admin/departments",
            get(admin_list_departments).post(admin_create_department),
        )
        .route(
            "/api/one/admin/departments/{department_id}",
            put(admin_rename_department).delete(admin_delete_department),
        )
        .route(
            "/api/one/admin/departments/{department_id}/parent",
            put(admin_set_department_parent),
        )
        .route(
            "/api/one/admin/departments/map-from-directory",
            post(admin_map_directory_department),
        )
        .route(
            "/api/one/admin/departments/directory-candidates",
            get(admin_list_directory_candidates),
        )
        // P2-1 integration connectors (reserved framework)
        .route("/api/one/admin/integrations", get(admin_list_integrations))
        .route("/api/one/admin/integrations/{provider}", put(admin_set_integration))
        .route(
            "/api/one/admin/integrations/{provider}/test",
            post(admin_test_integration),
        )
        .route("/api/one/admin/audit", get(admin_list_audit))
        .route("/api/one/admin/agent-audit", get(admin_list_agent_audit))
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

/// Every project group the caller belongs to (Phase 2 multi-membership), for
/// the "my project groups" switcher — with role, member count, and which one
/// is currently active.
async fn org_my_tenants(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
) -> Result<Json<ApiResponse<Vec<MyTenantDto>>>, OrgError> {
    let tenants = state.service.list_memberships(&actor.user_id).await?;
    Ok(Json(ApiResponse::ok(tenants)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchBody {
    tenant_id: String,
}

/// Switch which project group the caller is acting in. Returns the refreshed
/// context so the client can update in one round-trip. No token rotation — a
/// switch takes effect on the next request (see `set_active_tenant`).
async fn org_switch(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Json(body): Json<SwitchBody>,
) -> Result<Json<ApiResponse<OrgContextDto>>, OrgError> {
    state.service.set_active_tenant(&actor.user_id, &body.tenant_id).await?;
    let ctx = state.service.context(&actor.user_id).await?;
    Ok(Json(ApiResponse::ok(ctx)))
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

/// Every project group on this server (id + name), admin-gated — populates the
/// devops resource scope picker so an admin can bind a distributed skill / MCP
/// / knowledge doc to a specific project group (P0-4). Standalone/personal mode
/// has no groups; `RequireOrgAdmin` rejects there and the UI shows no options.
async fn org_list_tenants(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(_actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<TenantSummaryDto>>>, OrgError> {
    let tenants = state.service.list_all_tenants().await?;
    Ok(Json(ApiResponse::ok(tenants)))
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
    /// Which project group to leave; omitted = the currently-active group
    /// (Phase 2 multi-membership).
    #[serde(default)]
    tenant_id: Option<String>,
}

async fn org_exit(
    State(state): State<OneOrgRouterState>,
    actor: OrgActor,
    Json(body): Json<ExitBody>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state
        .service
        .leave(&actor.user_id, body.tenant_id.as_deref(), &body.exit_code)
        .await?;
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkInviteBody {
    count: usize,
    #[serde(default)]
    max_uses: Option<i64>,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

/// Bulk-generate invite codes (P2-4 onboarding): one click yields many codes to
/// hand out to a batch of new members.
async fn admin_create_invites_bulk(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<BulkInviteBody>,
) -> Result<Json<ApiResponse<Vec<CreatedInviteDto>>>, OrgError> {
    let created = state
        .service
        .create_invites_bulk(
            &actor.tenant_id,
            &actor.user_id,
            body.count,
            body.max_uses,
            body.expires_in_days,
        )
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.invite.bulk_create",
            None,
        )
        .await;
    let out = created
        .into_iter()
        .map(|(invite, display_code)| CreatedInviteDto { invite, display_code })
        .collect();
    Ok(Json(ApiResponse::ok(out)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendInviteEmailBody {
    to: String,
}

/// Send an invite by email (P2-4 onboarding). Reserved seam: with no SMTP
/// wired this returns `status: "not_configured"` — a clear signal for the UI
/// rather than a silent no-op.
async fn admin_send_invite_email(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(invite_id): Path<String>,
    Json(body): Json<SendInviteEmailBody>,
) -> Result<Json<ApiResponse<SendEmailResult>>, OrgError> {
    let result = state
        .service
        .send_invite_email(&actor.tenant_id, &invite_id, &body.to)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetAllowedDomainsBody {
    domains: Vec<String>,
}

/// Get/set the email domains that may auto-join this project group without an
/// invite code (P2-4 onboarding). Empty list = disabled (the default).
async fn admin_get_allowed_domains(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<String>>>, OrgError> {
    let domains = state.service.tenant_allowed_domains(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(domains)))
}

async fn admin_set_allowed_domains(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<SetAllowedDomainsBody>,
) -> Result<Json<ApiResponse<Vec<String>>>, OrgError> {
    state
        .service
        .set_tenant_allowed_domains(&actor.tenant_id, &body.domains)
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.onboarding.set_domains",
            None,
        )
        .await;
    let domains = state.service.tenant_allowed_domains(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(domains)))
}

async fn admin_get_smtp_config(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(_actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<SmtpConfigDto>>, OrgError> {
    Ok(Json(ApiResponse::ok(state.service.get_smtp_config().await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSmtpConfigBody {
    host: String,
    port: i64,
    #[serde(default)]
    username: Option<String>,
    /// Absent/empty = keep the stored password (if any).
    #[serde(default)]
    password: Option<String>,
    from_address: String,
    #[serde(default)]
    enabled: bool,
}

/// Reserved SMTP configuration (P2-4 onboarding). Saving this does not, by
/// itself, make email sending work — no SMTP client is wired into this crate.
/// It stores the operator's credentials so a real `EmailSender` can be dropped
/// in at the app layer (`OrgService::with_email_sender`) when ready.
async fn admin_set_smtp_config(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<SetSmtpConfigBody>,
) -> Result<Json<ApiResponse<SmtpConfigDto>>, OrgError> {
    let dto = state
        .service
        .set_smtp_config(
            &body.host,
            body.port,
            body.username.as_deref(),
            body.password.as_deref(),
            &body.from_address,
            body.enabled,
        )
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.onboarding.set_smtp",
            None,
        )
        .await;
    Ok(Json(ApiResponse::ok(dto)))
}

// --- P2-1 integration connectors (reserved framework) ---

async fn admin_list_integrations(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<IntegrationDto>>>, OrgError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_integrations(&actor.tenant_id).await?,
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetIntegrationBody {
    #[serde(default)]
    base_url: Option<String>,
    /// Non-secret provider-specific fields (org / project / repo / board ...).
    #[serde(default)]
    config: serde_json::Value,
    /// Absent/empty = keep the stored secret (if any).
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    enabled: bool,
}

/// Reserved connector configuration (P2-1). Saving this does not, by itself,
/// sync anything — no connector client is wired into this crate. It stores the
/// admin's credentials so a real `IntegrationProvider` can be dropped in at the
/// app layer (`OrgService::with_integration_provider`) when ready.
async fn admin_set_integration(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(provider): Path<String>,
    Json(body): Json<SetIntegrationBody>,
) -> Result<Json<ApiResponse<IntegrationDto>>, OrgError> {
    let dto = state
        .service
        .set_integration(
            &actor.tenant_id,
            &provider,
            body.base_url.as_deref(),
            &body.config,
            body.secret.as_deref(),
            body.enabled,
        )
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.integration.set",
            Some(&provider),
        )
        .await;
    Ok(Json(ApiResponse::ok(dto)))
}

async fn admin_test_integration(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(provider): Path<String>,
) -> Result<Json<ApiResponse<IntegrationTestResult>>, OrgError> {
    Ok(Json(ApiResponse::ok(
        state.service.test_integration(&actor.tenant_id, &provider).await?,
    )))
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

/// Remove a member from the caller's project group (P0-2), freeing their seat
/// and killing their sessions. Admin-gated by `RequireOrgAdmin`; the remaining
/// guards (self-removal, last admin, system_admin) live in the service so they
/// hold for every caller, not just this route.
async fn admin_remove_user(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(user_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state
        .service
        .remove_member(&actor.tenant_id, &actor.user_id, &user_id)
        .await?;
    Ok(Json(ApiResponse::ok(())))
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

// --- backup / restore (P1-1) ---

/// Export the deployment's enterprise configuration as a versioned bundle.
///
/// Secrets are stripped inside `backup::export_bundle` — this endpoint hands the
/// operator a file, so it must never be a credential-exfiltration path even for
/// a legitimate admin.
///
/// # Why system_admin and not org_admin
///
/// The bundle spans the **whole deployment**: every project group's members,
/// invites and departments, plus the company licence and SSO wiring. An
/// `org_admin` only administers their own project group, so gating this on
/// `RequireOrgAdmin` would let the admin of group A walk away with group B's
/// roster — a privilege escalation dressed up as a backup. The guard has to
/// match the data's scope, and deployment-wide data means the deployment's
/// system administrator.
async fn admin_export_backup(
    State(state): State<OneOrgRouterState>,
    RequireSystemAdmin(actor): RequireSystemAdmin,
) -> Result<Json<ApiResponse<crate::backup::BackupBundle>>, OrgError> {
    let bundle = state.service.export_backup(&actor.tenant_id, &actor.user_id).await?;
    Ok(Json(ApiResponse::ok(bundle)))
}

/// Restore a bundle produced by the export endpoint.
///
/// Same reasoning as the export above, and then some: a restore *overwrites*
/// deployment-wide rows, so a group admin could otherwise rewrite another
/// group's membership.
async fn admin_import_backup(
    State(state): State<OneOrgRouterState>,
    RequireSystemAdmin(actor): RequireSystemAdmin,
    Json(bundle): Json<crate::backup::BackupBundle>,
) -> Result<Json<ApiResponse<crate::backup::ImportReport>>, OrgError> {
    let report = state
        .service
        .import_backup(&actor.tenant_id, &actor.user_id, &bundle)
        .await?;
    Ok(Json(ApiResponse::ok(report)))
}

// --- departments / organizational hierarchy (P2-3) ---

async fn admin_list_departments(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<DepartmentDto>>>, OrgError> {
    let departments = state.service.list_departments(&actor.tenant_id).await?;
    Ok(Json(ApiResponse::ok(departments)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDepartmentBody {
    name: String,
    #[serde(default)]
    parent_id: Option<String>,
}

async fn admin_create_department(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<CreateDepartmentBody>,
) -> Result<Json<ApiResponse<DepartmentDto>>, OrgError> {
    let dept = state
        .service
        .create_department(&actor.tenant_id, &body.name, body.parent_id.as_deref())
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.department.create",
            Some(&dept.id),
        )
        .await;
    Ok(Json(ApiResponse::ok(dept)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameDepartmentBody {
    name: String,
}

async fn admin_rename_department(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(department_id): Path<String>,
    Json(body): Json<RenameDepartmentBody>,
) -> Result<Json<ApiResponse<DepartmentDto>>, OrgError> {
    let dept = state
        .service
        .rename_department(&actor.tenant_id, &department_id, &body.name)
        .await?;
    Ok(Json(ApiResponse::ok(dept)))
}

async fn admin_delete_department(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(department_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state
        .service
        .delete_department(&actor.tenant_id, &department_id)
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.department.delete",
            Some(&department_id),
        )
        .await;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDepartmentParentBody {
    /// `null` moves the department to top-level.
    parent_id: Option<String>,
}

async fn admin_set_department_parent(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(department_id): Path<String>,
    Json(body): Json<SetDepartmentParentBody>,
) -> Result<Json<ApiResponse<DepartmentDto>>, OrgError> {
    let dept = state
        .service
        .set_department_parent(&actor.tenant_id, &department_id, body.parent_id.as_deref())
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.department.move",
            Some(&department_id),
        )
        .await;
    Ok(Json(ApiResponse::ok(dept)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MapDirectoryDepartmentBody {
    /// The company directory department (T6 stage 1 mirror) to use as the
    /// mapping's root; it and everything under it become local departments.
    root_external_id: String,
}

/// T6 stage 3: the company directory mirror, for the "pick a subtree to map"
/// picker. Empty on personal/standalone installs or before any directory sync
/// has run — the frontend renders that as "nothing to map yet", not an error.
async fn admin_list_directory_candidates(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(_actor): RequireOrgAdmin,
) -> Result<Json<ApiResponse<Vec<crate::directory_bridge::DirectoryDepartmentRef>>>, OrgError> {
    let all = match &state.directory_source {
        Some(source) => source.directory_departments().await,
        None => Vec::new(),
    };
    Ok(Json(ApiResponse::ok(all)))
}

/// T6 stage 3: map a subtree of the company directory mirror into this
/// project group's department tree. Re-runnable — see
/// `OrgService::map_directory_subtree`'s doc comment for the reconciliation
/// rules.
async fn admin_map_directory_department(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Json(body): Json<MapDirectoryDepartmentBody>,
) -> Result<Json<ApiResponse<DirectoryMapReport>>, OrgError> {
    let all = match &state.directory_source {
        Some(source) => source.directory_departments().await,
        None => Vec::new(),
    };
    let report = state
        .service
        .map_directory_subtree(&actor.tenant_id, &body.root_external_id, &all)
        .await?;
    state
        .service
        .audit(
            &actor.tenant_id,
            Some(&actor.user_id),
            Some(&actor.username),
            "org.department.mapFromDirectory",
            Some(&body.root_external_id),
        )
        .await;
    Ok(Json(ApiResponse::ok(report)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignDepartmentBody {
    /// `null` clears the member's department assignment.
    department_id: Option<String>,
}

async fn admin_assign_member_department(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Path(user_id): Path<String>,
    Json(body): Json<AssignDepartmentBody>,
) -> Result<Json<ApiResponse<()>>, OrgError> {
    state
        .service
        .assign_member_department(&actor.tenant_id, &user_id, body.department_id.as_deref())
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
    // P0-3 license gate: the audit log is an enterprise-tier feature. Personal /
    // no-enterprise callers pass (the gate resolves to allowed).
    if !state
        .service
        .enterprise_feature_allowed(&actor.user_id, aionui_common::license::Feature::AuditLog)
        .await?
    {
        return Err(OrgError::Forbidden(
            "the audit log is not included in the current plan".into(),
        ));
    }
    let logs = state.service.list_audit_logs(&actor.tenant_id, query.limit).await?;
    Ok(Json(ApiResponse::ok(logs)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentAuditQuery {
    /// Filter to one member's runs.
    #[serde(default)]
    user_id: Option<String>,
    /// Filter to one tool name (Read / Write / Bash / …).
    #[serde(default)]
    tool: Option<String>,
    /// Inclusive lower bound (ms).
    #[serde(default)]
    since: Option<i64>,
    #[serde(default = "default_agent_audit_limit")]
    limit: i64,
}

fn default_agent_audit_limit() -> i64 {
    500
}

/// Agent-run audit (P1-1): which tools the agents invoked — files touched,
/// commands run — server-wide. Admin-only + AuditLog-tier-gated, matching the
/// governance audit log. Reconstructed from persisted tool-call messages.
async fn admin_list_agent_audit(
    State(state): State<OneOrgRouterState>,
    RequireOrgAdmin(actor): RequireOrgAdmin,
    Query(query): Query<AgentAuditQuery>,
) -> Result<Json<ApiResponse<Vec<AgentAuditEntry>>>, OrgError> {
    if !state
        .service
        .enterprise_feature_allowed(&actor.user_id, aionui_common::license::Feature::AuditLog)
        .await?
    {
        return Err(OrgError::Forbidden(
            "agent audit is not included in the current plan".into(),
        ));
    }
    let entries = state
        .service
        .list_agent_audit(
            query.user_id.as_deref(),
            query.tool.as_deref(),
            query.since,
            query.limit,
        )
        .await?;
    Ok(Json(ApiResponse::ok(entries)))
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
