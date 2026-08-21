//! DTOs for the billing plane. camelCase on the wire.

use serde::Serialize;

/// Whether a feature is included in the current plan.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntitlementDto {
    pub feature: String,
    pub allowed: bool,
}

/// The caller's company plan: tier, seat usage, and per-feature entitlements.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanDto {
    pub enterprise_id: String,
    pub tier: String,
    /// ACTIVE (governed, billable) seats only — what `seat_limit` actually
    /// caps. Does not include `seat_pending`.
    pub seat_used: i64,
    /// `null` = unlimited.
    pub seat_limit: Option<i64>,
    /// Members who logged in while the plan was full: they exist (so they can
    /// be denied rather than mistaken for personal users) but hold no seat and
    /// are blocked from every send. Not counted in `seat_used`; a plan upgrade
    /// or freeing a seat promotes them on their next login (T6-4).
    pub seat_pending: i64,
    pub expires_at: Option<i64>,
    pub entitlements: Vec<EntitlementDto>,
    /// P1-2 model control: rolling-30-day spend cap (USD-micros); `null` = no cap.
    pub cost_cap_micros: Option<i64>,
    /// Estimated spend this budget window (USD-micros).
    pub cost_used_micros: i64,
    /// Allowed model names; empty = all allowed.
    pub allowed_models: Vec<String>,
}

/// The vendor-signed license currently backing this company's plan.
///
/// Shown in the admin UI so an operator can confirm what was purchased, for
/// whom, and when it lapses. `expired` is computed server-side so the client
/// never has to reason about clock skew.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseInfoDto {
    pub license_id: String,
    pub customer: String,
    pub tier: String,
    /// `null` = the tier's default seat cap.
    pub seats: Option<i64>,
    /// `null` = perpetual.
    pub expires_at: Option<i64>,
    pub activated_at: i64,
    pub expired: bool,
}

/// One aggregation bucket (by user, by model, or by day).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucketDto {
    /// The bucket key: a user id, a model name, or a `YYYY-MM-DD` day.
    pub key: String,
    pub turns: i64,
    pub total_tokens: i64,
    pub estimated_cost_micros: i64,
}

/// Usage dashboard payload for a company over a time range.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummaryDto {
    pub since: i64,
    pub total_turns: i64,
    pub total_tokens: i64,
    pub estimated_cost_micros: i64,
    pub by_user: Vec<UsageBucketDto>,
    pub by_model: Vec<UsageBucketDto>,
    pub by_day: Vec<UsageBucketDto>,
    /// Bucket key `"unassigned"` covers members with no department (T7).
    pub by_department: Vec<UsageBucketDto>,
    /// How many media generations in this window were metered at zero because
    /// nothing priced them.
    ///
    /// Surfaced rather than papered over with an invented rate: a zero-cost call
    /// consumes none of the spend cap, so the cap quietly stops binding for that
    /// model. The built-in rate table matches on model name and a gateway with
    /// its own naming — the common case — misses every entry. The fix is for an
    /// admin to enter a unit price, and they can only do that if they know.
    pub unpriced_media_calls: i64,
    /// The models behind `unpriced_media_calls`, so the admin knows exactly
    /// which ones need a price rather than having to hunt.
    pub unpriced_media_models: Vec<String>,
}

/// Cumulative estimated spend for one conversation, across every backend
/// (ACP and aionrs both write `one_usage_events` rows via `record_turn`).
/// Self-scoped: any authenticated member can query their own conversation's
/// cost, not just billing admins.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationCostDto {
    pub conversation_id: String,
    pub estimated_cost_micros: i64,
}

/// One department's spend cap and usage this window (T7). `department_id` is
/// opaque here — one-billing does not depend on one-org, so it never resolves
/// a name; the caller (an admin UI that already fetched the department list
/// for its own tree view) joins by id.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepartmentBudgetDto {
    pub department_id: String,
    /// `null` = no department-level cap (only the company-wide one, if any,
    /// applies to this department's members).
    pub cost_cap_micros: Option<i64>,
    /// Estimated spend this budget window (USD-micros), same rolling window
    /// as the company-level figure.
    pub cost_used_micros: i64,
}

/// One generated media asset, for the T8 consolidated ledger.
///
/// One row per FILE, not per generation job — a job that produces 4 images
/// is 4 rows here, each individually findable.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaAssetDto {
    pub id: String,
    pub user_id: String,
    /// Opaque, same reasoning as `DepartmentBudgetDto.department_id` — this
    /// crate does not depend on one-org, so it never resolves a name.
    pub department_id: Option<String>,
    pub conversation_id: Option<String>,
    /// "image" | "video"
    pub kind: String,
    pub model: Option<String>,
    pub file_path: String,
    /// `null` unless the company has opted into prompt retention. Enforced
    /// server-side at write time — see `billing_005_media_ledger.sql`.
    pub prompt: Option<String>,
    pub created_at: i64,
}

/// Whether a company has opted into storing generation prompts in the
/// media ledger. Off by default: recording what people typed is a
/// content-retention decision, not something this product assumes for them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaLedgerSettingsDto {
    pub retain_prompts: bool,
}

/// Result of a checkout attempt. Real payment is not wired: the manual provider
/// returns `manual`, telling the client to contact an admin for provisioning.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutResultDto {
    /// `manual` (no payment provider configured) or `redirect` (a real provider
    /// returned a URL).
    pub status: String,
    pub message: String,
    pub checkout_url: Option<String>,
}
