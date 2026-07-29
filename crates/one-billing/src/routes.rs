//! `/api/one/billing/*` routes. Mount behind the upstream auth middleware
//! (relies on `CurrentUser`). `plan` / `checkout` are readable by any
//! authenticated member (they return `null` / a manual message for personal
//! users); `usage` and `tier` require a billing admin.

use axum::extract::{Query, State};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::license::Tier;
use aionui_common::now_ms;

use crate::error::BillingError;
use crate::models::{CheckoutResultDto, LicenseInfoDto, PlanDto, UsageSummaryDto};
use crate::state::OneBillingRouterState;

pub fn one_billing_routes(state: OneBillingRouterState) -> Router {
    Router::new()
        .route("/api/one/billing/plan", get(billing_plan))
        .route("/api/one/billing/usage", get(billing_usage))
        .route("/api/one/billing/tier", put(billing_set_tier))
        .route("/api/one/billing/model-control", put(billing_set_model_control))
        .route(
            "/api/one/billing/license",
            get(billing_get_license).post(billing_activate_license),
        )
        .route("/api/one/billing/checkout", post(billing_checkout))
        .route("/api/one/billing/webhook", post(billing_webhook))
        .with_state(state)
}

/// The license currently backing the plan, or `null` if none was ever
/// activated (or the caller is a personal user).
async fn billing_get_license(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Option<LicenseInfoDto>>>, BillingError> {
    let Some(eid) = state.service.resolve_enterprise_id(&user.id).await? else {
        return Ok(Json(ApiResponse::ok(None)));
    };
    Ok(Json(ApiResponse::ok(state.service.active_license(&eid).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivateLicenseBody {
    license_key: String,
}

/// Activate a vendor-signed license key — the only way to *raise* a tier.
/// Admin-gated: it changes what the whole company is entitled to.
async fn billing_activate_license(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<ActivateLicenseBody>,
) -> Result<Json<ApiResponse<PlanDto>>, BillingError> {
    if !state.service.is_billing_admin(&user.id).await? {
        return Err(BillingError::Forbidden("only an admin can activate a license".into()));
    }
    let eid = state
        .service
        .resolve_enterprise_id(&user.id)
        .await?
        .ok_or(BillingError::EnterpriseNotFound)?;
    state
        .service
        .activate_license(&eid, &body.license_key, &user.id)
        .await?;
    Ok(Json(ApiResponse::ok(state.service.plan(&eid).await?)))
}

/// The caller's company plan, or `null` for personal / standalone users.
async fn billing_plan(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Option<PlanDto>>>, BillingError> {
    let Some(eid) = state.service.resolve_enterprise_id(&user.id).await? else {
        return Ok(Json(ApiResponse::ok(None)));
    };
    Ok(Json(ApiResponse::ok(Some(state.service.plan(&eid).await?))))
}

#[derive(Deserialize)]
struct UsageQuery {
    /// Inclusive lower bound (ms). Defaults to 30 days ago.
    #[serde(default)]
    since: Option<i64>,
}

async fn billing_usage(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<ApiResponse<UsageSummaryDto>>, BillingError> {
    if !state.service.is_billing_admin(&user.id).await? {
        return Err(BillingError::Forbidden("usage dashboard is admin-only".into()));
    }
    let eid = state
        .service
        .resolve_enterprise_id(&user.id)
        .await?
        .ok_or(BillingError::EnterpriseNotFound)?;
    const THIRTY_DAYS_MS: i64 = 30 * 24 * 3600 * 1000;
    let since = q.since.unwrap_or_else(|| now_ms() - THIRTY_DAYS_MS);
    Ok(Json(ApiResponse::ok(state.service.usage_summary(&eid, since).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetTierBody {
    tier: String,
    /// Explicit seat override; omit to use the tier default.
    #[serde(default)]
    seat_limit: Option<i64>,
}

/// Change the tier **downward** (billing-admin only). Upgrades are refused with
/// `UPGRADE_REQUIRES_LICENSE` — raising a tier requires activating a
/// vendor-signed key via `POST /api/one/billing/license`.
async fn billing_set_tier(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<SetTierBody>,
) -> Result<Json<ApiResponse<PlanDto>>, BillingError> {
    if !state.service.is_billing_admin(&user.id).await? {
        return Err(BillingError::Forbidden("only an admin can change the plan".into()));
    }
    // Reject unknown tiers explicitly (Tier::parse would silently downgrade).
    if !matches!(body.tier.as_str(), "free" | "team" | "enterprise") {
        return Err(BillingError::BadRequest(format!("unknown tier: {}", body.tier)));
    }
    let eid = state
        .service
        .resolve_enterprise_id(&user.id)
        .await?
        .ok_or(BillingError::EnterpriseNotFound)?;
    state
        .service
        .set_tier(&eid, Tier::parse(&body.tier), body.seat_limit)
        .await?;
    Ok(Json(ApiResponse::ok(state.service.plan(&eid).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelControlBody {
    /// Rolling-30-day spend cap in USD-micros; `null` = remove the cap.
    #[serde(default)]
    cost_cap_micros: Option<i64>,
    /// Allowed model names; empty = allow all.
    #[serde(default)]
    allowed_models: Vec<String>,
}

/// Set the model-control policy (spend cap + model allowlist). Billing-admin.
async fn billing_set_model_control(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<ModelControlBody>,
) -> Result<Json<ApiResponse<PlanDto>>, BillingError> {
    if !state.service.is_billing_admin(&user.id).await? {
        return Err(BillingError::Forbidden("only an admin can change model control".into()));
    }
    let eid = state
        .service
        .resolve_enterprise_id(&user.id)
        .await?
        .ok_or(BillingError::EnterpriseNotFound)?;
    state
        .service
        .set_model_control(&eid, body.cost_cap_micros, &body.allowed_models)
        .await?;
    Ok(Json(ApiResponse::ok(state.service.plan(&eid).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutBody {
    target_tier: String,
}

/// Begin an upgrade. With no payment provider configured this returns a
/// `manual` result telling the client to contact an admin.
async fn billing_checkout(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CheckoutBody>,
) -> Result<Json<ApiResponse<CheckoutResultDto>>, BillingError> {
    let eid = state
        .service
        .resolve_enterprise_id(&user.id)
        .await?
        .ok_or(BillingError::EnterpriseNotFound)?;
    Ok(Json(ApiResponse::ok(
        state.service.create_checkout(&eid, &body.target_tier),
    )))
}

/// Payment webhook seam. No provider configured → accept-and-ignore so a future
/// real provider can post here without a 404.
async fn billing_webhook(State(_state): State<OneBillingRouterState>) -> Json<ApiResponse<()>> {
    Json(ApiResponse::ok(()))
}
