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
    pub seat_used: i64,
    /// `null` = unlimited.
    pub seat_limit: Option<i64>,
    pub expires_at: Option<i64>,
    pub entitlements: Vec<EntitlementDto>,
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
