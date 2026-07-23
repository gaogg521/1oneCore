//! `/api/one/enterprise/*` routes. Mount behind the upstream auth middleware
//! (relies on `CurrentUser`). `/me` and `/company` are readable by any
//! authenticated user (they return `null` when nothing applies); the company
//! member-admin routes require the company `admin` role.

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::error::EnterpriseError;
use crate::models::{CompanyMemberDto, CompanyOverviewDto, EnterpriseIdentityDto};
use crate::rbac::RequireCompanyAdmin;
use crate::state::OneEnterpriseRouterState;

pub fn one_enterprise_routes(state: OneEnterpriseRouterState) -> Router {
    Router::new()
        .route("/api/one/enterprise/me", get(enterprise_me))
        .route("/api/one/enterprise/company", get(company_overview))
        .route("/api/one/enterprise/company/setup", post(company_setup))
        .route("/api/one/enterprise/company/members", get(company_members))
        .route(
            "/api/one/enterprise/company/members/{user_id}/role",
            put(company_set_member_role),
        )
        .with_state(state)
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
