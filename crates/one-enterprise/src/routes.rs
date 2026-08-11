//! `/api/one/enterprise/*` routes. Mount behind the upstream auth middleware
//! (relies on `CurrentUser`). `/me` and `/company` are readable by any
//! authenticated user (they return `null` when nothing applies); the company
//! member-admin routes require the company `admin` role.

use axum::extract::{Path, State};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::directory::{DepartedMemberDto, DirectoryPersonDto, DirectorySyncStateDto};
use crate::error::EnterpriseError;
use crate::models::{CompanyMemberDto, CompanyOverviewDto, DisbandCompanyResult, EnterpriseIdentityDto};
use crate::rbac::RequireCompanyAdmin;
use crate::state::OneEnterpriseRouterState;

pub fn one_enterprise_routes(state: OneEnterpriseRouterState) -> Router {
    Router::new()
        .route("/api/one/enterprise/me", get(enterprise_me))
        .route(
            "/api/one/enterprise/company",
            get(company_overview).delete(company_disband),
        )
        .route("/api/one/enterprise/company/setup", post(company_setup))
        .route("/api/one/enterprise/company/members", get(company_members))
        .route(
            "/api/one/enterprise/company/members/{user_id}",
            delete(company_remove_member),
        )
        .route(
            "/api/one/enterprise/company/members/{user_id}/role",
            put(company_set_member_role),
        )
        .route("/api/one/enterprise/directory/status", get(directory_status))
        .route("/api/one/enterprise/directory/departed", get(directory_departed))
        .route("/api/one/enterprise/directory/people", get(directory_people))
        .with_state(state)
}

/// Last directory sync's outcome, for the console's status line.
///
/// `null` before the first run ever. A `partial` status is not a cosmetic
/// detail: it means the mirror is stale and no departures were derived, so the
/// console has to say so rather than showing an empty "everyone is here".
async fn directory_status(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
) -> Result<Json<ApiResponse<Option<DirectorySyncStateDto>>>, EnterpriseError> {
    let status = state.service.directory_sync_state(&admin.enterprise_id).await?;
    Ok(Json(ApiResponse::ok(status)))
}

/// Company members the directory no longer vouches for.
///
/// A *suggestion list*, never an action: offboarding removes access, rotates
/// tokens and reassigns work, so it stays behind the admin's existing remove
/// dialog rather than happening because an API said somebody was missing.
async fn directory_departed(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
) -> Result<Json<ApiResponse<Vec<DepartedMemberDto>>>, EnterpriseError> {
    let departed = state.service.list_departed_members(&admin.enterprise_id).await?;
    Ok(Json(ApiResponse::ok(departed)))
}

/// The directory roster itself — everyone the last complete sync still
/// vouches for. `directory_status` says whether sync worked and
/// `directory_departed` says who left; neither shows who is actually in the
/// mirror, which previously had no reachable UI at all despite the sync
/// status line reporting a headcount.
async fn directory_people(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
) -> Result<Json<ApiResponse<Vec<DirectoryPersonDto>>>, EnterpriseError> {
    let people = state.service.list_directory_people(&admin.enterprise_id).await?;
    Ok(Json(ApiResponse::ok(people)))
}

/// The caller's own enterprise-org identity (SSO company + department), or
/// `null`. Independent of any project-group membership.
async fn enterprise_me(
    State(state): State<OneEnterpriseRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Option<EnterpriseIdentityDto>>>, EnterpriseError> {
    let identity = state.service.identity_of(&user.id).await?;
    Ok(Json(ApiResponse::ok(identity)))
}

/// The deployment's company as seen by the caller, or `null` when no company
/// exists on this server. Readable by any authenticated user (the frontend
/// gates the console on `viewerRole == 'admin'`).
async fn company_overview(
    State(state): State<OneEnterpriseRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Option<CompanyOverviewDto>>>, EnterpriseError> {
    let overview = state.service.company_overview(&user.id).await?;
    Ok(Json(ApiResponse::ok(overview)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupCompanyBody {
    name: String,
}

/// 显式设立: a system_admin establishes the deployment's company. The
/// system_admin gate lives in the service (it needs the cross-domain
/// `one_user_org` read).
async fn company_setup(
    State(state): State<OneEnterpriseRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<SetupCompanyBody>,
) -> Result<Json<ApiResponse<CompanyOverviewDto>>, EnterpriseError> {
    let overview = state.service.setup_company(&user.id, &body.name).await?;
    Ok(Json(ApiResponse::ok(overview)))
}

async fn company_members(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
) -> Result<Json<ApiResponse<Vec<CompanyMemberDto>>>, EnterpriseError> {
    let members = state.service.list_members(&admin.enterprise_id).await?;
    Ok(Json(ApiResponse::ok(members)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMemberRoleBody {
    role: String,
}

/// Permanently deletes the company: every project group it owns, every
/// enterprise-scoped billing/usage record, every membership, and the
/// company record itself. Irreversible.
async fn company_disband(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
) -> Result<Json<ApiResponse<DisbandCompanyResult>>, EnterpriseError> {
    let result = state
        .service
        .disband_company(&admin.user_id, &admin.enterprise_id)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Remove a member from the company, releasing their seat (P0-2).
async fn company_remove_member(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
    Path(user_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, EnterpriseError> {
    state
        .service
        .remove_member(&admin.enterprise_id, &admin.user_id, &user_id)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn company_set_member_role(
    State(state): State<OneEnterpriseRouterState>,
    admin: RequireCompanyAdmin,
    Path(user_id): Path<String>,
    Json(body): Json<SetMemberRoleBody>,
) -> Result<Json<ApiResponse<()>>, EnterpriseError> {
    state
        .service
        .set_member_role(&admin.enterprise_id, &user_id, &body.role)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}
