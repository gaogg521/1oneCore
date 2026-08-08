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
        .route("/api/one/billing/media-precheck", post(billing_media_precheck))
        .route("/api/one/billing/media-usage", post(billing_media_usage))
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaPrecheckBody {
    /// "image" | "video" — carried for future kind-specific policy and for the
    /// denial message; the allowlist itself keys on the model.
    kind: Option<String>,
    model: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaPrecheckDto {
    pub allow: bool,
    /// Present when `allow` is false: why, in words a user can act on.
    pub reason: Option<String>,
}

/// Policy gate for image/video generation.
///
/// Media runs through the built-in MCP tool, which never passed through
/// `SendGate` — so the priciest calls in the product used to bypass both the
/// spend cap and the model allowlist. Answers `allow: false` with a reason
/// rather than an HTTP error, so the caller can surface the policy decision as
/// a normal job failure instead of having to tell a denial apart from an outage.
///
/// Personal / no-company callers always get `allow: true`.
async fn billing_media_precheck(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<MediaPrecheckBody>,
) -> Result<Json<ApiResponse<MediaPrecheckDto>>, BillingError> {
    match state.service.check_media_allowed(&user.id, &body.model).await {
        Ok(()) => Ok(Json(ApiResponse::ok(MediaPrecheckDto {
            allow: true,
            reason: None,
        }))),
        Err(err) => {
            let kind = body.kind.as_deref().unwrap_or("media");
            // Name the lever, not just the verdict. The allowlist is one list
            // for chat and media alike, so the overwhelmingly common cause of
            // this refusal is an admin who filled in chat models and did not
            // realise image/video models had to be listed too — and "blocked by
            // company policy" alone gives them nowhere to go.
            let reason = match &err {
                BillingError::ModelNotAllowed(model) => format!(
                    "{kind} generation blocked: the model '{model}' is not on your company's model allowlist. \
                     An administrator can add it under 企业管理后台 → 订阅与用量 → 模型 allowlist \
                     (that one list covers chat and image/video models alike)."
                ),
                other => format!("{kind} generation blocked by company policy: {other}"),
            };
            Ok(Json(ApiResponse::ok(MediaPrecheckDto {
                allow: false,
                reason: Some(reason),
            })))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaUsageBody {
    kind: String,
    model: String,
    /// Number of assets produced.
    count: Option<i64>,
    /// Video only; ignored for images.
    duration_seconds: Option<i64>,
    /// The user's own price per asset (image) or per second (video), in
    /// USD-micros. Present only when they entered one; overrides the built-in
    /// rate table so the rollup reflects real money rather than an estimate.
    unit_price_micros: Option<i64>,
    /// Which conversation the generation belongs to, when the caller knows.
    ///
    /// Attribution, not content. A generation started from the compose box
    /// writes no message, so without this the ledger row is the only trace it
    /// leaves and it points at nothing an admin can follow.
    conversation_id: Option<String>,
}

/// Report a completed media generation so it lands in the company's spend
/// rollup and usage dashboard. Reported after the fact — the precheck is what
/// blocks, this is what makes the next precheck accurate.
async fn billing_media_usage(
    State(state): State<OneBillingRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<MediaUsageBody>,
) -> Result<Json<ApiResponse<()>>, BillingError> {
    state
        .service
        .record_media_usage(
            &user.id,
            &body.kind,
            &body.model,
            body.count.unwrap_or(1),
            body.duration_seconds.unwrap_or(0),
            body.unit_price_micros,
            body.conversation_id.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(())))
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
