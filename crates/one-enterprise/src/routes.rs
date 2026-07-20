//! `/api/one/enterprise/*` routes. Mount behind the upstream auth middleware
//! (relies on `CurrentUser`). Not role-gated: any authenticated user reads
//! their own enterprise-org identity.

use axum::extract::State;
use axum::routing::get;
use axum::{Extension, Json, Router};

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::error::EnterpriseError;
use crate::models::EnterpriseIdentityDto;
use crate::state::OneEnterpriseRouterState;

pub fn one_enterprise_routes(state: OneEnterpriseRouterState) -> Router {
    Router::new()
        .route("/api/one/enterprise/me", get(enterprise_me))
        .with_state(state)
}

/// The caller's own enterprise-org identity (SSO company + department), or
/// `null` for a user with no SSO-company membership. Independent of any
/// project-group membership.
async fn enterprise_me(
    State(state): State<OneEnterpriseRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Option<EnterpriseIdentityDto>>>, EnterpriseError> {
    let identity = state.service.identity_of(&user.id).await?;
    Ok(Json(ApiResponse::ok(identity)))
}
