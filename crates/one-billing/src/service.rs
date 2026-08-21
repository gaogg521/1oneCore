//! Billing-plane business logic: license tier, seat enforcement, usage
//! metering, and the (stubbed) payment provider.
//!
//! License attaches to an SSO company (`one_enterprises`). A user's company is
//! resolved from `one_enterprise_members`. Personal / standalone users have no
//! company row → not in the billing system (every check is permissive, seats
//! uncounted, usage recorded with a NULL enterprise). This is the red line.

use std::sync::Arc;

use aionui_common::license::{
    Feature, Tier, estimate_cost_micros, estimate_media_cost_micros, tier_allows, tier_seat_limit,
};
use aionui_common::{generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::error::BillingError;
use crate::models::{
    CheckoutResultDto, DepartmentBudgetDto, EntitlementDto, LicenseInfoDto, MediaAssetDto, PlanDto, UsageBucketDto,
    UsageSummaryDto,
};

/// Pluggable payment backend. The default `ManualBillingProvider` is a stub
/// (no real payments); a real Stripe/… provider can drop in later without
/// touching callers.
pub trait BillingProvider: Send + Sync {
    /// Begin a checkout for `target_tier`. The stub returns a `manual` result.
    fn create_checkout(&self, enterprise_id: &str, target_tier: &str) -> CheckoutResultDto;
    fn name(&self) -> &'static str;
}

/// No payment provider configured: upgrades are provisioned manually by an
/// admin (`PUT /tier`). Structurally present so real payment is a drop-in.
pub struct ManualBillingProvider;

impl BillingProvider for ManualBillingProvider {
    fn create_checkout(&self, _enterprise_id: &str, _target_tier: &str) -> CheckoutResultDto {
        CheckoutResultDto {
            status: "manual".to_owned(),
            message: "No payment provider is configured. Contact your administrator to provision this plan.".to_owned(),
            checkout_url: None,
        }
    }

    fn name(&self) -> &'static str {
        "manual"
    }
}

#[derive(Clone)]
pub struct BillingService {
    pool: SqlitePool,
    provider: Arc<dyn BillingProvider>,
}

/// A stored license row (absent → free defaults).
struct License {
    tier: Tier,
    seat_limit: Option<i64>,
    expires_at: Option<i64>,
    /// Rolling-30-day estimated-cost budget in USD-micros; `None` = no cap (P1-2).
    cost_cap_micros: Option<i64>,
    /// Allowed model names; empty = all allowed (P1-2).
    allowed_models: Vec<String>,
}

/// Rolling budget window (P1-2): 30 days.
const BUDGET_WINDOW_MS: i64 = 30 * 24 * 3600 * 1000;

/// One completed media generation, as reported for metering.
///
/// A struct rather than a positional argument list: these are seven mostly-
/// primitive values, several of them `i64`, so a mis-ordered call site would
/// compile and silently meter the wrong thing.
#[derive(Debug, Clone, Copy)]
pub struct MediaUsage<'a> {
    pub user_id: &'a str,
    /// `"image"` or `"video"` — decides whether duration participates in cost.
    pub kind: &'a str,
    pub model: &'a str,
    /// Number of assets actually produced (not requested).
    pub count: i64,
    /// Video only; ignored for images.
    pub duration_seconds: i64,
    /// The user's own price for this model, when they entered one. Overrides the
    /// built-in rate table, which is a coarse illustration next to the contract
    /// they are actually billed under.
    pub unit_price_micros: Option<i64>,
    /// Where the generation happened. Attribution, not content — it is what lets
    /// an admin follow a charge back to somewhere. Optional because a caller may
    /// genuinely not have one.
    pub conversation_id: Option<&'a str>,
}

/// T8 ledger search filters. A named struct (not positional params) because
/// there are more than clippy's `too_many_arguments` threshold once
/// `enterprise_id` and `&self` are added, and because most of these are the
/// same primitive `Option<&str>` shape — position-mixups would compile.
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaAssetFilters<'a> {
    pub kind: Option<&'a str>,
    pub model: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub since: Option<i64>,
    /// Only meaningful when the company has opted into prompt retention —
    /// see `list_media_assets`'s doc comment for why no special-casing is
    /// needed when it hasn't.
    pub prompt_contains: Option<&'a str>,
    /// Defaults to 200, clamped to [1, 1000] — this is a search UI, not an
    /// unbounded export.
    pub limit: Option<i64>,
}

/// Raw row shape for `list_media_assets`, before the enterprise-scoped filter
/// context is folded away — `sqlx::FromRow` needs a concrete struct, and
/// `enterprise_id` itself is not projected back (it's already the filter).
#[derive(sqlx::FromRow)]
struct MediaAssetRow {
    id: String,
    user_id: String,
    department_id: Option<String>,
    conversation_id: Option<String>,
    kind: String,
    model: Option<String>,
    file_path: String,
    prompt: Option<String>,
    created_at: i64,
}

impl MediaAssetRow {
    fn into_dto(self) -> MediaAssetDto {
        MediaAssetDto {
            id: self.id,
            user_id: self.user_id,
            department_id: self.department_id,
            conversation_id: self.conversation_id,
            kind: self.kind,
            model: self.model,
            file_path: self.file_path,
            prompt: self.prompt,
            created_at: self.created_at,
        }
    }
}

/// Ordering for "is this an upgrade?". Kept local rather than deriving `Ord` on
/// `Tier` in aionui-common, because tier ordering is a *billing* policy, not an
/// intrinsic property of the enum.
fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Free => 0,
        Tier::Team => 1,
        Tier::Enterprise => 2,
    }
}

/// Parse the stored `allowed_models` JSON array; malformed / null → empty
/// (= all models allowed).
fn parse_allowed_models(json: Option<&str>) -> Vec<String> {
    json.and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default()
}

impl BillingService {
    pub fn new(pool: SqlitePool, provider: Arc<dyn BillingProvider>) -> Self {
        Self { pool, provider }
    }

    /// The caller's SSO company, or `None` for personal / standalone users
    /// (who are outside the billing system entirely).
    pub async fn resolve_enterprise_id(&self, user_id: &str) -> Result<Option<String>, BillingError> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT enterprise_id FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        Ok(row)
    }

    /// Whether the caller holds an ACTIVE (governed, billable) seat, as
    /// opposed to no company row at all, or a `pending` row created when they
    /// logged in while the plan's seat cap was already full (T6-4).
    ///
    /// This is deliberately a separate query from `resolve_enterprise_id`
    /// rather than folding seat_status into its return: that function's
    /// `None` means "personal user, skip every check" everywhere it is called,
    /// and a pending member is the opposite of that — they DO belong to a
    /// company, and must be denied, not waved through. Reusing `None` for both
    /// would silently restore the exact bug this column exists to close.
    async fn has_active_seat(&self, user_id: &str) -> Result<bool, BillingError> {
        let status: Option<String> =
            sqlx::query_scalar("SELECT seat_status FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        // No row → not a company member at all, handled by `resolve_enterprise_id`
        // returning `None` upstream; this function is only consulted once a
        // caller already has `Some(enterprise_id)`.
        Ok(status.as_deref() == Some("active"))
    }

    /// The caller's CURRENT department (T7), or `None` if unassigned. Reads
    /// `one_user_org.department_id` — owned by one-org, same cross-crate raw-
    /// SQL idiom this file already uses for `one_enterprise_members` — and is
    /// tolerant of the table being absent (standalone/personal builds, or a
    /// deployment where P2-3 departments were never migrated).
    async fn resolve_department_id(&self, user_id: &str) -> Result<Option<String>, BillingError> {
        // Double `Option`: the outer one is "does the membership row exist",
        // the inner one is the column's own nullability (unassigned). Only
        // the outer layer is truly "value absent" — collapsing both without
        // this would need the column decode to fail for a NULL department_id,
        // which is not an error, it is the overwhelmingly common case.
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT department_id FROM one_user_org WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        Ok(row.flatten())
    }

    async fn license_of(&self, enterprise_id: &str) -> Result<License, BillingError> {
        // tier, seat_limit, expires_at, cost_cap_micros, allowed_models(json)
        type LicenseRow = (String, Option<i64>, Option<i64>, Option<i64>, Option<String>);
        let row: Option<LicenseRow> = sqlx::query_as(
            "SELECT tier, seat_limit, expires_at, monthly_cost_cap_micros, allowed_models \
             FROM one_enterprise_license WHERE enterprise_id = ?",
        )
        .bind(enterprise_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((tier, seat_limit, expires_at, cost_cap_micros, allowed_models_json)) => {
                // Expiry is enforced here, at the single read point every gate
                // funnels through, so a lapsed license degrades everywhere at
                // once without a background job. The row is left untouched: the
                // admin UI still shows what was bought and when it ran out, and
                // renewing re-activates it without losing history.
                let expired = expires_at.is_some_and(|exp| exp <= aionui_common::now_ms());
                License {
                    tier: if expired { Tier::Free } else { Tier::parse(&tier) },
                    // A lapsed license also loses its seat override, otherwise
                    // an expired enterprise plan would keep an unlimited cap.
                    seat_limit: if expired { None } else { seat_limit },
                    expires_at,
                    cost_cap_micros,
                    allowed_models: parse_allowed_models(allowed_models_json.as_deref()),
                }
            }
            // No row → a company created before it was licensed, or an unknown
            // id: default to the entry tier (least privilege).
            None => License {
                tier: Tier::Free,
                seat_limit: None,
                expires_at: None,
                cost_cap_micros: None,
                allowed_models: Vec::new(),
            },
        })
    }

    /// Effective seat cap: explicit override, else the tier default. `None` =
    /// unlimited.
    fn effective_seat_limit(license: &License) -> Option<i64> {
        license
            .seat_limit
            .or_else(|| tier_seat_limit(license.tier).map(|n| n as i64))
    }

    /// ACTIVE seats only — what `seat_limit` caps. See `PlanDto::seat_used`.
    async fn seat_used(&self, enterprise_id: &str) -> Result<i64, BillingError> {
        let used: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ? AND seat_status = 'active'",
        )
        .bind(enterprise_id)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        Ok(used)
    }

    /// Members waiting on a seat. See `PlanDto::seat_pending`.
    async fn seat_pending(&self, enterprise_id: &str) -> Result<i64, BillingError> {
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ? AND seat_status = 'pending'",
        )
        .bind(enterprise_id)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        Ok(pending)
    }

    /// Whether the company can take one more member under its plan. Companies
    /// with an unlimited tier — and the personal case (`None` enterprise) —
    /// always can.
    pub async fn can_add_seat(&self, enterprise_id: Option<&str>) -> Result<bool, BillingError> {
        let Some(eid) = enterprise_id else {
            return Ok(true);
        };
        let license = self.license_of(eid).await?;
        match Self::effective_seat_limit(&license) {
            None => Ok(true),
            Some(limit) => Ok(self.seat_used(eid).await? < limit),
        }
    }

    /// Whether `feature` is included in the company's plan. Personal (`None`
    /// enterprise) is always allowed — the red line.
    pub async fn entitlement(&self, enterprise_id: Option<&str>, feature: Feature) -> Result<bool, BillingError> {
        let Some(eid) = enterprise_id else {
            return Ok(true);
        };
        let license = self.license_of(eid).await?;
        Ok(tier_allows(license.tier, feature))
    }

    /// Downgrade-only tier change (self-service).
    ///
    /// A customer admin may *drop* to a cheaper tier (e.g. to free up an
    /// entitlement they are not using) but may never raise one — an upgrade
    /// must come from a vendor-signed license via [`Self::activate_license`].
    /// Without this asymmetry the whole licensing scheme is decorative: the
    /// gates are enforced correctly, but anyone could grant themselves the top
    /// tier. Raising a tier here returns [`BillingError::UpgradeRequiresLicense`].
    pub async fn set_tier(&self, enterprise_id: &str, tier: Tier, seat_limit: Option<i64>) -> Result<(), BillingError> {
        let current = self.license_of(enterprise_id).await?;
        if tier_rank(tier) > tier_rank(current.tier) {
            return Err(BillingError::UpgradeRequiresLicense);
        }
        // A downgrade also clears any license expiry/seat override: the plan is
        // now whatever the admin chose, not what a (possibly still-valid) key
        // said. Re-activating the key restores it.
        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, seat_limit, expires_at, updated_at) \
             VALUES (?, ?, ?, NULL, ?) \
             ON CONFLICT(enterprise_id) DO UPDATE SET tier = excluded.tier, seat_limit = excluded.seat_limit, \
                 expires_at = NULL, updated_at = excluded.updated_at",
        )
        .bind(enterprise_id)
        .bind(tier.as_str())
        .bind(seat_limit)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Activate a vendor-signed license key: the only path that can *raise* a
    /// tier. Verification is offline (Ed25519 against the built-in public key)
    /// so an air-gapped deployment can be licensed.
    ///
    /// Idempotent by the key's `lid` claim — re-pasting the same key refreshes
    /// the entitlement without stacking activation rows.
    pub async fn activate_license(
        &self,
        enterprise_id: &str,
        license_key: &str,
        activated_by: &str,
    ) -> Result<crate::license_key::LicensePayload, BillingError> {
        let payload = crate::license_key::verify_license_key(license_key)?;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO one_license_activation \
                 (license_id, enterprise_id, customer, tier, seats, expires_at, issued_at, activated_at, activated_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(license_id) DO UPDATE SET enterprise_id = excluded.enterprise_id, \
                 activated_at = excluded.activated_at, activated_by = excluded.activated_by",
        )
        .bind(&payload.lid)
        .bind(enterprise_id)
        .bind(&payload.customer)
        .bind(&payload.tier)
        .bind(payload.seats)
        .bind(payload.exp)
        .bind(payload.iat)
        .bind(now_ms())
        .bind(activated_by)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, seat_limit, expires_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(enterprise_id) DO UPDATE SET tier = excluded.tier, seat_limit = excluded.seat_limit, \
                 expires_at = excluded.expires_at, updated_at = excluded.updated_at",
        )
        .bind(enterprise_id)
        .bind(&payload.tier)
        .bind(payload.seats)
        .bind(payload.exp)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        tracing::info!(
            enterprise_id,
            license_id = %payload.lid,
            tier = %payload.tier,
            "license activated"
        );
        Ok(payload)
    }

    /// The license currently backing this company's entitlements, if any was
    /// ever activated. Shown in the admin UI so an operator can see what was
    /// bought, for whom, and when it lapses.
    pub async fn active_license(&self, enterprise_id: &str) -> Result<Option<LicenseInfoDto>, BillingError> {
        type Row = (String, String, String, Option<i64>, Option<i64>, i64);
        let row: Option<Row> = sqlx::query_as(
            "SELECT license_id, customer, tier, seats, expires_at, activated_at \
             FROM one_license_activation WHERE enterprise_id = ? ORDER BY activated_at DESC LIMIT 1",
        )
        .bind(enterprise_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(license_id, customer, tier, seats, expires_at, activated_at)| LicenseInfoDto {
                license_id,
                customer,
                tier,
                seats,
                expires_at,
                activated_at,
                expired: expires_at.is_some_and(|e| e <= now_ms()),
            },
        ))
    }

    /// Set the model-control policy (P1-2): rolling-30-day spend cap
    /// (USD-micros; `None` = no cap) and allowed model list (`None`/empty = all
    /// allowed). Billing-admin path.
    pub async fn set_model_control(
        &self,
        enterprise_id: &str,
        cost_cap_micros: Option<i64>,
        allowed_models: &[String],
    ) -> Result<(), BillingError> {
        let allowed_json = if allowed_models.is_empty() {
            None
        } else {
            Some(serde_json::to_string(allowed_models).unwrap_or_else(|_| "[]".to_owned()))
        };
        // Upsert onto the (existing or default) license row.
        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, monthly_cost_cap_micros, allowed_models, updated_at) \
             VALUES (?, 'free', ?, ?, ?) \
             ON CONFLICT(enterprise_id) DO UPDATE SET monthly_cost_cap_micros = excluded.monthly_cost_cap_micros, \
                 allowed_models = excluded.allowed_models, updated_at = excluded.updated_at",
        )
        .bind(enterprise_id)
        .bind(cost_cap_micros)
        .bind(allowed_json)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Estimated spend (USD-micros) for the company over the rolling budget
    /// window.
    async fn budget_used_micros(&self, enterprise_id: &str) -> Result<i64, BillingError> {
        let since = now_ms() - BUDGET_WINDOW_MS;
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(estimated_cost_micros), 0) FROM one_usage_events \
             WHERE enterprise_id = ? AND created_at >= ?",
        )
        .bind(enterprise_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        Ok(used)
    }

    /// Cumulative estimated spend (USD-micros) for one conversation, summed
    /// across every `one_usage_events` row `record_turn`/`record_media_usage`
    /// wrote for it. Self-scoped by `user_id` — the caller can only ever sum
    /// rows they themselves generated, so no separate "is this conversation
    /// yours" ownership check is needed (unlike the enterprise-wide
    /// `budget_used_micros` above, this is meant to be called by any
    /// authenticated member for their own conversation, not just billing
    /// admins). Backends that never write a row here (a brand-new
    /// conversation with no turns yet) return 0, not an error.
    pub async fn conversation_cost(&self, user_id: &str, conversation_id: &str) -> Result<i64, BillingError> {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(estimated_cost_micros), 0) FROM one_usage_events \
             WHERE user_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        Ok(used)
    }

    /// Set (or, with `None`, clear) a department's spend cap (T7). Same
    /// rolling-30-day window as the company-level cap; a department cap is a
    /// tighter constraint layered UNDER the company one, never a replacement
    /// for it — a department under its own cap can still be blocked by the
    /// company running out of budget, and vice versa.
    pub async fn set_department_budget(
        &self,
        enterprise_id: &str,
        department_id: &str,
        cost_cap_micros: Option<i64>,
    ) -> Result<(), BillingError> {
        sqlx::query(
            "INSERT INTO one_department_budgets (department_id, enterprise_id, cost_cap_micros, updated_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(department_id) DO UPDATE SET \
                 cost_cap_micros = excluded.cost_cap_micros, updated_at = excluded.updated_at",
        )
        .bind(department_id)
        .bind(enterprise_id)
        .bind(cost_cap_micros)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn department_budget_cap(&self, department_id: &str) -> Result<Option<i64>, BillingError> {
        let cap: Option<Option<i64>> =
            sqlx::query_scalar("SELECT cost_cap_micros FROM one_department_budgets WHERE department_id = ?")
                .bind(department_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        Ok(cap.flatten())
    }

    /// Estimated spend (USD-micros) for one department over the rolling
    /// budget window. Reads `one_usage_events.department_id`, stamped at
    /// record time — see the migration's doc comment for why that is
    /// denormalized rather than resolved live from `one_user_org`.
    async fn department_budget_used_micros(&self, department_id: &str) -> Result<i64, BillingError> {
        let since = now_ms() - BUDGET_WINDOW_MS;
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(estimated_cost_micros), 0) FROM one_usage_events \
             WHERE department_id = ? AND created_at >= ?",
        )
        .bind(department_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        Ok(used)
    }

    /// Every department that has ever had a cap configured for this company,
    /// with current-window spend (T7 dashboard). Departments with no cap and
    /// no spend simply do not appear — nothing to show an admin about them.
    pub async fn list_department_budgets(&self, enterprise_id: &str) -> Result<Vec<DepartmentBudgetDto>, BillingError> {
        let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT department_id, cost_cap_micros FROM one_department_budgets \
             WHERE enterprise_id = ? ORDER BY updated_at DESC",
        )
        .bind(enterprise_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (department_id, cost_cap_micros) in rows {
            let cost_used_micros = self.department_budget_used_micros(&department_id).await?;
            out.push(DepartmentBudgetDto {
                department_id,
                cost_cap_micros,
                cost_used_micros,
            });
        }
        Ok(out)
    }

    /// T8: whether this company has opted into storing generation prompts.
    /// Absent row = default = never store them.
    pub async fn media_ledger_retain_prompts(&self, enterprise_id: &str) -> Result<bool, BillingError> {
        let retain: Option<bool> =
            sqlx::query_scalar("SELECT retain_prompts FROM one_media_ledger_settings WHERE enterprise_id = ?")
                .bind(enterprise_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(retain.unwrap_or(false))
    }

    /// Admin-only: opt the company in or out of prompt retention.
    pub async fn set_media_ledger_retain_prompts(
        &self,
        enterprise_id: &str,
        retain_prompts: bool,
    ) -> Result<(), BillingError> {
        sqlx::query(
            "INSERT INTO one_media_ledger_settings (enterprise_id, retain_prompts, updated_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT(enterprise_id) DO UPDATE SET \
                 retain_prompts = excluded.retain_prompts, updated_at = excluded.updated_at",
        )
        .bind(enterprise_id)
        .bind(retain_prompts)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// T8: record one generated FILE in the consolidated ledger (a job that
    /// produces N assets is N calls, not one). Enterprise-scoped only — a
    /// personal/no-company user is a no-op, same red line every other
    /// governance surface in this crate honors, and it keeps this table from
    /// silently becoming a per-user media index nobody asked for.
    ///
    /// `prompt` is always accepted from the caller but only persisted when
    /// the company has opted in — enforced HERE, not trusted from the client,
    /// same principle as every other policy check in this file.
    pub async fn record_media_asset(
        &self,
        user_id: &str,
        kind: &str,
        model: Option<&str>,
        file_path: &str,
        prompt: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Result<(), BillingError> {
        let Some(enterprise_id) = self.resolve_enterprise_id(user_id).await? else {
            return Ok(());
        };
        let department_id = self.resolve_department_id(user_id).await?;
        let retained_prompt = if self.media_ledger_retain_prompts(&enterprise_id).await? {
            prompt
        } else {
            None
        };
        sqlx::query(
            "INSERT INTO one_media_assets \
                (id, user_id, enterprise_id, department_id, conversation_id, kind, model, file_path, prompt, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(generate_prefixed_id("media"))
        .bind(user_id)
        .bind(&enterprise_id)
        .bind(department_id)
        .bind(conversation_id)
        .bind(kind)
        .bind(model)
        .bind(file_path)
        .bind(retained_prompt)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// T8: admin-only search over the ledger. `filters.prompt_contains` is
    /// harmless to pass even when the company never opted into retention —
    /// the column is NULL for every row in that case, so a `LIKE` against it
    /// simply matches nothing, no special-casing required.
    pub async fn list_media_assets(
        &self,
        enterprise_id: &str,
        filters: MediaAssetFilters<'_>,
    ) -> Result<Vec<MediaAssetDto>, BillingError> {
        let mut sql = String::from(
            "SELECT id, user_id, department_id, conversation_id, kind, model, file_path, prompt, created_at \
             FROM one_media_assets WHERE enterprise_id = ?",
        );
        if filters.kind.is_some() {
            sql.push_str(" AND kind = ?");
        }
        if filters.model.is_some() {
            sql.push_str(" AND model = ?");
        }
        if filters.user_id.is_some() {
            sql.push_str(" AND user_id = ?");
        }
        if filters.since.is_some() {
            sql.push_str(" AND created_at >= ?");
        }
        if filters.prompt_contains.is_some() {
            sql.push_str(" AND prompt LIKE ?");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");

        let mut query = sqlx::query_as::<_, MediaAssetRow>(&sql).bind(enterprise_id);
        if let Some(kind) = filters.kind {
            query = query.bind(kind);
        }
        if let Some(model) = filters.model {
            query = query.bind(model);
        }
        if let Some(user_id) = filters.user_id {
            query = query.bind(user_id);
        }
        if let Some(since) = filters.since {
            query = query.bind(since);
        }
        if let Some(needle) = filters.prompt_contains {
            query = query.bind(format!("%{}%", needle.replace('%', "\\%").replace('_', "\\_")));
        }
        query = query.bind(filters.limit.unwrap_or(200).clamp(1, 1000));

        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(MediaAssetRow::into_dto).collect())
    }

    /// Pre-send gate (P1-2): reject when the company is over its spend budget,
    /// or the requested `model` is not on its allowlist. Personal / no-company
    /// users, and companies with neither control set, always pass (red line).
    ///
    /// ⚠️ T6-4: a company member is not automatically a GOVERNED member. Someone
    /// who logged in after the plan's seat cap filled has a row (so they are
    /// found here, not mistaken for personal) but no assigned seat, and
    /// therefore no policy that was configured for them — the only correct
    /// answer is to deny outright, before even looking at the allowlist/budget
    /// (which, on a company with neither control set, would otherwise pass
    /// everyone through unconditionally).
    pub async fn check_send_allowed(&self, user_id: &str, model: Option<&str>) -> Result<(), BillingError> {
        let Some(enterprise_id) = self.resolve_enterprise_id(user_id).await? else {
            return Ok(());
        };
        if !self.has_active_seat(user_id).await? {
            return Err(BillingError::SeatLimitExceeded);
        }
        let license = self.license_of(&enterprise_id).await?;

        // Model allowlist.
        if !license.allowed_models.is_empty()
            && let Some(model) = model.map(str::trim).filter(|s| !s.is_empty())
            && !license.allowed_models.iter().any(|m| m == model)
        {
            return Err(BillingError::ModelNotAllowed(model.to_owned()));
        }

        // Spend cap — company-wide first: if the whole company is out of
        // budget that is the more useful thing to tell the caller, and it
        // blocks everyone regardless of department.
        if let Some(cap) = license.cost_cap_micros
            && self.budget_used_micros(&enterprise_id).await? >= cap
        {
            return Err(BillingError::BudgetExceeded);
        }

        // T7: a department cap is a tighter constraint layered under the
        // company one. Unassigned members have no department to check.
        if let Some(department_id) = self.resolve_department_id(user_id).await?
            && let Some(cap) = self.department_budget_cap(&department_id).await?
            && self.department_budget_used_micros(&department_id).await? >= cap
        {
            return Err(BillingError::DepartmentBudgetExceeded);
        }
        Ok(())
    }

    /// Allowlist-only check (P1-2): whether `model` may be selected under the
    /// company policy. Used at the model-switch point (budget is enforced
    /// separately at send). Personal / no-allowlist → allowed.
    ///
    /// Same T6-4 guard as `check_send_allowed`: a pending (unseated) member is
    /// denied outright rather than falling through to a possibly-empty
    /// allowlist that would otherwise let them pick any model.
    pub async fn check_model_allowed(&self, user_id: &str, model: &str) -> Result<(), BillingError> {
        let Some(enterprise_id) = self.resolve_enterprise_id(user_id).await? else {
            return Ok(());
        };
        if !self.has_active_seat(user_id).await? {
            return Err(BillingError::SeatLimitExceeded);
        }
        let license = self.license_of(&enterprise_id).await?;
        let model = model.trim();
        if !license.allowed_models.is_empty() && !model.is_empty() && !license.allowed_models.iter().any(|m| m == model)
        {
            return Err(BillingError::ModelNotAllowed(model.to_owned()));
        }
        Ok(())
    }

    /// Whether a media generation (image / video) may run under company policy.
    ///
    /// Media reaches the provider through the built-in MCP tool, which never
    /// passed through `SendGate` — so until this existed, the most expensive
    /// calls in the product bypassed both the spend cap and the model
    /// allowlist entirely. The policy is deliberately the same one the chat
    /// path uses (one allowlist, one budget) rather than a parallel set of
    /// rules an admin would have to discover and maintain separately.
    ///
    /// Personal / no-company users pass, same red line as everywhere else.
    pub async fn check_media_allowed(&self, user_id: &str, model: &str) -> Result<(), BillingError> {
        self.check_send_allowed(user_id, Some(model)).await
    }

    /// Record one completed media generation against the company's usage.
    ///
    /// Reuses `one_usage_events`: media has no token counts, so those columns
    /// stay NULL and only the estimated cost is carried. That keeps media
    /// inside the existing budget rollup (`budget_used_micros`) and the usage
    /// dashboard without a schema change.
    ///
    /// `conversation_id` is attribution, not content — it is what lets an admin
    /// follow a charge back to where it happened. Optional because a caller may
    /// genuinely not have one; the column has always been nullable.
    pub async fn record_media_usage(&self, usage: MediaUsage<'_>) -> Result<(), BillingError> {
        let MediaUsage {
            user_id,
            kind,
            model,
            count,
            duration_seconds,
            unit_price_micros,
            conversation_id,
        } = usage;
        let enterprise_id = self.resolve_enterprise_id(user_id).await?;
        let department_id = self.resolve_department_id(user_id).await?;
        // A price the user entered for their own provider beats our built-in
        // table: the table is a coarse illustration, theirs is the contract they
        // are actually billed under.
        let cost = match unit_price_micros.filter(|price| *price > 0) {
            Some(price) => {
                let units = if kind.eq_ignore_ascii_case("video") {
                    let seconds = if duration_seconds > 0 { duration_seconds } else { 5 };
                    count.max(0) * seconds
                } else {
                    count.max(0)
                };
                units * price
            }
            None => estimate_media_cost_micros(kind, model, count, duration_seconds),
        };
        // A media call that costs 0 does not just report oddly — it consumes
        // none of the company's spend cap, so the cap silently stops binding for
        // that model. The built-in rate table matches on model name, and a
        // gateway with its own naming (very common) misses it entirely. Nothing
        // here invents a number: the row is recorded as-is, but an operator can
        // now find out why their cap is not moving, and fix it by entering a
        // unit price for the model.
        if cost == 0 && count > 0 && enterprise_id.is_some() {
            tracing::warn!(
                model,
                kind,
                "media usage recorded at zero cost: no built-in rate matched this model and no unit \
                 price was configured, so it does not count against the company's spend cap"
            );
        }
        sqlx::query(
            "INSERT INTO one_usage_events \
                (id, user_id, enterprise_id, department_id, conversation_id, model, input_tokens, output_tokens, total_tokens, estimated_cost_micros, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?)",
        )
        .bind(generate_prefixed_id("usage"))
        .bind(user_id)
        .bind(enterprise_id)
        .bind(department_id)
        .bind(conversation_id)
        .bind(model)
        .bind(cost)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The company plan for the dashboard: tier, seat usage, entitlements.
    pub async fn plan(&self, enterprise_id: &str) -> Result<PlanDto, BillingError> {
        let license = self.license_of(enterprise_id).await?;
        let entitlements = aionui_common::license::ALL_FEATURES
            .iter()
            .map(|f| EntitlementDto {
                feature: f.as_str().to_owned(),
                allowed: tier_allows(license.tier, *f),
            })
            .collect();
        Ok(PlanDto {
            enterprise_id: enterprise_id.to_owned(),
            tier: license.tier.as_str().to_owned(),
            seat_used: self.seat_used(enterprise_id).await?,
            seat_limit: Self::effective_seat_limit(&license),
            seat_pending: self.seat_pending(enterprise_id).await?,
            expires_at: license.expires_at,
            entitlements,
            cost_cap_micros: license.cost_cap_micros,
            cost_used_micros: self.budget_used_micros(enterprise_id).await?,
            allowed_models: license.allowed_models,
        })
    }

    /// Record one metered turn. `enterprise_id` is resolved from the user;
    /// personal users record with a NULL enterprise. Tokens are best-effort
    /// (may be `None`); cost is an estimate from the model rate table.
    pub async fn record_turn(
        &self,
        user_id: &str,
        conversation_id: Option<&str>,
        model: Option<&str>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    ) -> Result<(), BillingError> {
        let enterprise_id = self.resolve_enterprise_id(user_id).await?;
        // T7: stamped from the user's department AT THIS MOMENT, not resolved
        // live when a report is later pulled — see the migration's doc
        // comment for why (a department is a cost center; reassigning someone
        // must not reshuffle where past spend counted).
        let department_id = self.resolve_department_id(user_id).await?;
        let total_tokens = match (input_tokens, output_tokens) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        let cost = model.map(|m| estimate_cost_micros(m, input_tokens.unwrap_or(0), output_tokens.unwrap_or(0)));
        // Same diagnosis hatch as `record_media_usage`: a turn costed at 0
        // consumes none of the company's spend cap, so the cap silently stops
        // binding for that model. The built-in rate table matches on model
        // name, and a gateway with its own naming misses it entirely — as does
        // a vision delegate whose model the table has never heard of. Nothing
        // here invents a rate (that would be a pricing decision); the row is
        // recorded as-is, but an operator can now find out why their cap is
        // not moving.
        if cost == Some(0) && total_tokens.unwrap_or(0) > 0 && enterprise_id.is_some() {
            tracing::warn!(
                model,
                "turn usage recorded at zero cost: no built-in rate matched this model, so it does not count                  against the company's spend cap"
            );
        }
        sqlx::query(
            "INSERT INTO one_usage_events \
                (id, user_id, enterprise_id, department_id, conversation_id, model, input_tokens, output_tokens, total_tokens, estimated_cost_micros, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(generate_prefixed_id("usage"))
        .bind(user_id)
        .bind(enterprise_id)
        .bind(department_id)
        .bind(conversation_id)
        .bind(model)
        .bind(input_tokens)
        .bind(output_tokens)
        .bind(total_tokens)
        .bind(cost)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes every billing/usage record attributed to `enterprise_id`:
    /// license/tier/spend-cap/allowlist, metered usage events, department
    /// budgets, the media ledger and its retention setting, and license
    /// activation history. Called by one-enterprise's `disband_company`
    /// through the `CompanyDisbandCascade` trait it wires up in
    /// `aionui-app` (same layer, no direct dependency). Authorization is
    /// enforced by that caller; this trusts the `enterprise_id` it is given.
    pub async fn delete_enterprise_billing_data(&self, enterprise_id: &str) -> Result<(), BillingError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM one_enterprise_license WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM one_usage_events WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM one_department_budgets WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM one_media_assets WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM one_media_ledger_settings WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM one_license_activation WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::warn!(enterprise_id, "enterprise billing history deleted (企业注销级联)");
        Ok(())
    }

    /// Aggregate usage for a company since `since_ms`, grouped by user, model,
    /// and day, plus grand totals.
    pub async fn usage_summary(&self, enterprise_id: &str, since_ms: i64) -> Result<UsageSummaryDto, BillingError> {
        let by_user = self.buckets(enterprise_id, since_ms, "user_id").await?;
        let by_model = self
            .buckets(enterprise_id, since_ms, "COALESCE(model, 'unknown')")
            .await?;
        let by_day = self
            .buckets(
                enterprise_id,
                since_ms,
                "strftime('%Y-%m-%d', created_at / 1000, 'unixepoch')",
            )
            .await?;
        let by_department = self
            .buckets(enterprise_id, since_ms, "COALESCE(department_id, 'unassigned')")
            .await?;

        let (total_turns, total_tokens, total_cost): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(SUM(estimated_cost_micros), 0) \
             FROM one_usage_events WHERE enterprise_id = ? AND created_at >= ?",
        )
        .bind(enterprise_id)
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await?;

        // Media rows are the ones with no token counts (media is metered per
        // asset, never per token), so `total_tokens IS NULL` identifies them
        // without a schema change. A zero cost among those means nothing priced
        // the call — neither the built-in table nor a unit price the admin
        // entered — and it therefore consumed none of the spend cap.
        let (unpriced_media_calls, unpriced_models): (i64, Option<String>) = sqlx::query_as(
            "SELECT COUNT(*), GROUP_CONCAT(DISTINCT model) FROM one_usage_events \
             WHERE enterprise_id = ? AND created_at >= ? \
               AND total_tokens IS NULL AND model IS NOT NULL AND estimated_cost_micros = 0",
        )
        .bind(enterprise_id)
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await?;

        Ok(UsageSummaryDto {
            since: since_ms,
            total_turns,
            total_tokens,
            estimated_cost_micros: total_cost,
            by_user,
            by_model,
            by_day,
            by_department,
            unpriced_media_calls,
            unpriced_media_models: unpriced_models
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// Grouped aggregation. `key_expr` is a trusted SQL expression (never user
    /// input) selecting the bucket key.
    async fn buckets(
        &self,
        enterprise_id: &str,
        since_ms: i64,
        key_expr: &str,
    ) -> Result<Vec<UsageBucketDto>, BillingError> {
        let sql = format!(
            "SELECT {key_expr} AS k, COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(SUM(estimated_cost_micros), 0) \
             FROM one_usage_events WHERE enterprise_id = ? AND created_at >= ? \
             GROUP BY k ORDER BY COUNT(*) DESC"
        );
        let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(&sql)
            .bind(enterprise_id)
            .bind(since_ms)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(key, turns, total_tokens, cost)| UsageBucketDto {
                key,
                turns,
                total_tokens,
                estimated_cost_micros: cost,
            })
            .collect())
    }

    /// Begin a checkout (stubbed by the manual provider).
    pub fn create_checkout(&self, enterprise_id: &str, target_tier: &str) -> CheckoutResultDto {
        self.provider.create_checkout(enterprise_id, target_tier)
    }

    /// Whether the caller may see the usage dashboard / provision a tier.
    ///
    /// Billing is **enterprise-scoped** (`one_enterprise_license` and
    /// `one_usage_events` are keyed by `enterprise_id`), so the guard has to be
    /// enterprise-scoped too: a company admin, or the deployment's
    /// `system_admin`.
    ///
    /// A plain `org_admin` is deliberately NOT enough. An org_admin administers
    /// one project group, but the tier, seat cap, spend cap and model allowlist
    /// they would be changing apply to the whole company — so in a company with
    /// several project groups, accepting org_admin would let group A's admin
    /// raise (or cut) the budget for everyone. The `system_admin` arm is what
    /// keeps personal / single-machine deployments working, where the machine
    /// owner holds that role and there is no company row at all.
    ///
    /// Tolerant of absent tables (personal mode → falls through to the role
    /// read, which itself tolerates a missing `one_user_org`).
    pub async fn is_billing_admin(&self, user_id: &str) -> Result<bool, BillingError> {
        let company_role: Option<String> =
            sqlx::query_scalar("SELECT role FROM one_enterprise_members WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
        if company_role.as_deref() == Some("admin") {
            return Ok(true);
        }
        // Server admin: active-tenant-aware role read, mirroring the cross-crate
        // role resolution in one-devops / one-sso.
        let org_role: Option<String> = sqlx::query_scalar(
            "SELECT uo.role FROM one_user_org uo WHERE uo.user_id = ? \
             ORDER BY (uo.tenant_id = (SELECT tenant_id FROM one_active_tenant WHERE user_id = uo.user_id)) DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);
        Ok(org_role.as_deref() == Some("system_admin"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service() -> BillingService {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::migrate::tests::one_enterprise_tables(&pool).await;
        crate::migrate::run_one_billing_migrations(&pool).await.unwrap();
        BillingService::new(pool, Arc::new(ManualBillingProvider))
    }

    async fn add_members(svc: &BillingService, enterprise_id: &str, n: usize) {
        for i in 0..n {
            sqlx::query("INSERT INTO one_enterprise_members (user_id, enterprise_id, role, joined_at, updated_at) VALUES (?, ?, 'member', 0, 0)")
                .bind(format!("u{enterprise_id}{i}"))
                .bind(enterprise_id)
                .execute(&svc.pool)
                .await
                .unwrap();
        }
    }

    /// ⚠️ The point of company disband cascading into one-billing: every
    /// billing/usage record attributed to the disbanded company must be
    /// gone from every table it can appear in — an unrelated company's
    /// history in the same tables must survive untouched.
    #[tokio::test]
    async fn deleting_enterprise_billing_data_clears_every_table_for_that_company_only() {
        let svc = service().await;
        for ent in ["ent1", "ent2"] {
            sqlx::query(
                "INSERT INTO one_enterprise_license (enterprise_id, tier, updated_at) VALUES (?, 'enterprise', 0)",
            )
            .bind(ent)
            .execute(&svc.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO one_usage_events (id, user_id, enterprise_id, model, created_at) \
                 VALUES (?, 'u1', ?, 'claude-opus', 0)",
            )
            .bind(format!("evt-{ent}"))
            .bind(ent)
            .execute(&svc.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO one_department_budgets (department_id, enterprise_id, cost_cap_micros, updated_at) \
                 VALUES (?, ?, 1000, 0)",
            )
            .bind(format!("dep-{ent}"))
            .bind(ent)
            .execute(&svc.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO one_media_assets (id, user_id, enterprise_id, kind, file_path, created_at) \
                 VALUES (?, 'u1', ?, 'image', '/tmp/x.png', 0)",
            )
            .bind(format!("asset-{ent}"))
            .bind(ent)
            .execute(&svc.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO one_media_ledger_settings (enterprise_id, retain_prompts, updated_at) VALUES (?, 1, 0)",
            )
            .bind(ent)
            .execute(&svc.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO one_license_activation \
                    (license_id, enterprise_id, customer, tier, issued_at, activated_at, activated_by) \
                 VALUES (?, ?, 'Acme', 'enterprise', 0, 0, 'admin1')",
            )
            .bind(format!("lic-{ent}"))
            .bind(ent)
            .execute(&svc.pool)
            .await
            .unwrap();
        }

        svc.delete_enterprise_billing_data("ent1").await.unwrap();

        for (table, column) in [
            ("one_enterprise_license", "enterprise_id"),
            ("one_usage_events", "enterprise_id"),
            ("one_department_budgets", "enterprise_id"),
            ("one_media_assets", "enterprise_id"),
            ("one_media_ledger_settings", "enterprise_id"),
            ("one_license_activation", "enterprise_id"),
        ] {
            let gone: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = 'ent1'"))
                .fetch_one(&svc.pool)
                .await
                .unwrap();
            assert_eq!(gone, 0, "{table} must have no ent1 rows left");
            let survives: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = 'ent2'"))
                .fetch_one(&svc.pool)
                .await
                .unwrap();
            assert_eq!(survives, 1, "{table} must not have touched ent2's row");
        }
    }

    #[tokio::test]
    async fn personal_no_enterprise_allows_all_and_unlimited_seats() {
        let svc = service().await;
        // No enterprise → every feature allowed, seats always addable.
        assert!(svc.resolve_enterprise_id("nobody").await.unwrap().is_none());
        assert!(svc.can_add_seat(None).await.unwrap());
        for f in aionui_common::license::ALL_FEATURES {
            assert!(svc.entitlement(None, f).await.unwrap(), "personal allows {f:?}");
        }
        // Recording usage with no enterprise is fine (NULL enterprise_id).
        svc.record_turn("nobody", Some("c1"), Some("claude-opus"), Some(10), Some(20))
            .await
            .unwrap();
    }

    /// Force a tier directly in the table, bypassing the license gate.
    ///
    /// Tests must not carry a real signing key (it would then live in the
    /// repo), so entitlement fixtures write the row directly. The *gate* on
    /// raising a tier is covered separately by
    /// `set_tier_refuses_upgrade_without_license`.
    async fn force_tier(svc: &BillingService, enterprise_id: &str, tier: Tier, expires_at: Option<i64>) {
        sqlx::query(
            "INSERT INTO one_enterprise_license (enterprise_id, tier, seat_limit, expires_at, updated_at) \
             VALUES (?, ?, NULL, ?, 0) \
             ON CONFLICT(enterprise_id) DO UPDATE SET tier = excluded.tier, expires_at = excluded.expires_at",
        )
        .bind(enterprise_id)
        .bind(tier.as_str())
        .bind(expires_at)
        .execute(&svc.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn free_tier_gates_features_and_caps_seats() {
        let svc = service().await;
        force_tier(&svc, "ent_free", Tier::Free, None).await;
        // Free: SSO disallowed.
        assert!(!svc.entitlement(Some("ent_free"), Feature::Sso).await.unwrap());
        // Seat cap 3: three ok, fourth blocked.
        add_members(&svc, "ent_free", 3).await;
        assert!(!svc.can_add_seat(Some("ent_free")).await.unwrap());
        // On team → SSO allowed, cap 25.
        force_tier(&svc, "ent_free", Tier::Team, None).await;
        assert!(svc.entitlement(Some("ent_free"), Feature::Sso).await.unwrap());
        assert!(svc.can_add_seat(Some("ent_free")).await.unwrap());
    }

    /// The commercial keystone: a customer's own admin must not be able to
    /// grant themselves a higher tier. Without this the entire licensing
    /// scheme is decorative.
    #[tokio::test]
    async fn set_tier_refuses_upgrade_without_license() {
        let svc = service().await;
        force_tier(&svc, "ent_x", Tier::Free, None).await;

        for target in [Tier::Team, Tier::Enterprise] {
            let err = svc.set_tier("ent_x", target, None).await.unwrap_err();
            assert_eq!(
                err.code(),
                "UPGRADE_REQUIRES_LICENSE",
                "raising free → {target:?} must be refused"
            );
        }
        // Still free — the refused calls changed nothing.
        assert!(!svc.entitlement(Some("ent_x"), Feature::Sso).await.unwrap());

        // Downgrades remain self-service.
        force_tier(&svc, "ent_x", Tier::Enterprise, None).await;
        svc.set_tier("ent_x", Tier::Team, None).await.unwrap();
        assert!(!svc.entitlement(Some("ent_x"), Feature::AuditLog).await.unwrap());
        svc.set_tier("ent_x", Tier::Free, None).await.unwrap();
        assert!(!svc.entitlement(Some("ent_x"), Feature::Sso).await.unwrap());
    }

    /// An expired license must degrade to free everywhere at once — including
    /// dropping any seat override it granted.
    #[tokio::test]
    async fn expired_license_degrades_to_free() {
        let svc = service().await;
        let past = aionui_common::now_ms() - 1000;
        force_tier(&svc, "ent_exp", Tier::Enterprise, Some(past)).await;
        // Give it an explicit generous seat override too.
        sqlx::query("UPDATE one_enterprise_license SET seat_limit = 500 WHERE enterprise_id = 'ent_exp'")
            .execute(&svc.pool)
            .await
            .unwrap();

        // Enterprise features are gone...
        assert!(!svc.entitlement(Some("ent_exp"), Feature::AuditLog).await.unwrap());
        assert!(!svc.entitlement(Some("ent_exp"), Feature::Sso).await.unwrap());
        // ...and the seat cap falls back to free's 3, not the 500 override.
        add_members(&svc, "ent_exp", 3).await;
        assert!(
            !svc.can_add_seat(Some("ent_exp")).await.unwrap(),
            "an expired license must not keep its seat override"
        );

        // A still-valid license keeps working.
        let future = aionui_common::now_ms() + 60_000;
        force_tier(&svc, "ent_ok", Tier::Enterprise, Some(future)).await;
        assert!(svc.entitlement(Some("ent_ok"), Feature::AuditLog).await.unwrap());
    }

    #[tokio::test]
    async fn existing_enterprise_grandfathered_to_top_tier() {
        let svc = service().await;
        // Simulate a pre-billing company, then re-run migration (grandfather).
        sqlx::query("INSERT INTO one_enterprises (id, provider, external_id, created_at, updated_at) VALUES ('ent_old', 'feishu', 'x', 0, 0)")
            .execute(&svc.pool)
            .await
            .unwrap();
        // Wipe ledger entry so the backfill re-runs against the new row.
        sqlx::query("DELETE FROM _one_migrations WHERE name = 'billing_001_init'")
            .execute(&svc.pool)
            .await
            .unwrap();
        crate::migrate::run_one_billing_migrations(&svc.pool).await.unwrap();
        // Grandfathered to enterprise: all features on, unlimited seats.
        assert!(svc.entitlement(Some("ent_old"), Feature::AuditLog).await.unwrap());
        let plan = svc.plan("ent_old").await.unwrap();
        assert_eq!(plan.tier, "enterprise");
        assert_eq!(plan.seat_limit, None);
    }

    #[tokio::test]
    async fn usage_summary_aggregates() {
        let svc = service().await;
        add_members(&svc, "ent1", 1).await;
        // Map the recording user to ent1 so record_turn resolves it.
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'alice' WHERE enterprise_id = 'ent1'")
            .execute(&svc.pool)
            .await
            .unwrap();
        svc.record_turn("alice", Some("c1"), Some("claude-opus-4-8"), Some(100), Some(200))
            .await
            .unwrap();
        svc.record_turn("alice", Some("c1"), Some("claude-opus-4-8"), Some(50), Some(50))
            .await
            .unwrap();
        let summary = svc.usage_summary("ent1", 0).await.unwrap();
        assert_eq!(summary.total_turns, 2);
        assert_eq!(summary.total_tokens, 400);
        assert!(summary.estimated_cost_micros > 0);
        assert_eq!(summary.by_user.len(), 1);
        assert_eq!(summary.by_user[0].key, "alice");
        assert_eq!(summary.by_model[0].key, "claude-opus-4-8");
    }

    /// aionrs conversations have no other way to surface a session cost (the
    /// ACP-only `/usage` snapshot structurally never fires for them — see
    /// `AgentInstance::get_usage`), so the frontend sums this directly.
    /// Personal/no-enterprise users must work too: `conversation_cost` is
    /// scoped by `user_id`, not by company membership.
    #[tokio::test]
    async fn conversation_cost_sums_every_turn_for_that_conversation() {
        let svc = service().await;
        svc.record_turn("solo", Some("conv_x"), Some("claude-opus-4-8"), Some(100), Some(200))
            .await
            .unwrap();
        svc.record_turn("solo", Some("conv_x"), Some("claude-opus-4-8"), Some(50), Some(50))
            .await
            .unwrap();
        // A different conversation for the same user must not bleed in.
        svc.record_turn("solo", Some("conv_y"), Some("claude-opus-4-8"), Some(9_999), Some(9_999))
            .await
            .unwrap();

        let cost = svc.conversation_cost("solo", "conv_x").await.unwrap();
        assert!(cost > 0);

        let single_turn_cost = svc.conversation_cost("solo", "conv_y").await.unwrap();
        // conv_y's lone turn has far more tokens than conv_x's two turns
        // combined; if the query were not scoping by conversation_id, conv_x's
        // total would exceed it instead of the other way around.
        assert!(single_turn_cost > cost);
    }

    #[tokio::test]
    async fn conversation_cost_is_zero_for_a_conversation_with_no_turns_yet() {
        let svc = service().await;
        assert_eq!(svc.conversation_cost("solo", "brand_new_conv").await.unwrap(), 0);
    }

    /// The dashboard has to name the models nothing priced, because a zero-cost
    /// media call consumes none of the spend cap — the cap quietly stops binding
    /// for that model, and the only fix is an admin entering a unit price. This
    /// is the visibility half of that (option 2), deliberately instead of
    /// inventing a fallback rate.
    #[tokio::test]
    async fn usage_summary_names_the_media_models_nothing_priced() {
        let svc = service().await;
        add_members(&svc, "ent1", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'alice' WHERE enterprise_id = 'ent1'")
            .execute(&svc.pool)
            .await
            .unwrap();

        let usage = |model: &'static str, price: Option<i64>| MediaUsage {
            user_id: "alice",
            kind: "image",
            model,
            count: 1,
            duration_seconds: 0,
            unit_price_micros: price,
            conversation_id: None,
        };

        // Nothing prices these two.
        svc.record_media_usage(usage("our-gateway-name-a", None)).await.unwrap();
        svc.record_media_usage(usage("our-gateway-name-b", None)).await.unwrap();
        // The built-in table prices this one…
        svc.record_media_usage(usage("gpt-image-2", None)).await.unwrap();
        // …and the admin priced this one themselves.
        svc.record_media_usage(usage("another-gateway-name", Some(50_000)))
            .await
            .unwrap();
        // A chat turn must not be mistaken for unpriced media.
        svc.record_turn("alice", Some("c1"), Some("some-chat-model"), Some(10), Some(10))
            .await
            .unwrap();

        let summary = svc.usage_summary("ent1", 0).await.unwrap();
        assert_eq!(summary.unpriced_media_calls, 2, "only the two nothing priced");
        let mut named = summary.unpriced_media_models.clone();
        named.sort();
        assert_eq!(named, vec!["our-gateway-name-a", "our-gateway-name-b"]);
    }

    /// A company that prices everything must see a clean dashboard — otherwise
    /// the warning becomes noise everyone learns to ignore.
    #[tokio::test]
    async fn a_fully_priced_company_sees_no_warning() {
        let svc = service().await;
        add_members(&svc, "ent1", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'alice' WHERE enterprise_id = 'ent1'")
            .execute(&svc.pool)
            .await
            .unwrap();
        svc.record_media_usage(MediaUsage {
            user_id: "alice",
            kind: "video",
            model: "seedance-2-0-fast",
            count: 1,
            duration_seconds: 5,
            unit_price_micros: None,
            conversation_id: None,
        })
        .await
        .unwrap();

        let summary = svc.usage_summary("ent1", 0).await.unwrap();
        assert_eq!(summary.unpriced_media_calls, 0);
        assert!(summary.unpriced_media_models.is_empty());
    }

    #[tokio::test]
    async fn manual_checkout_is_stubbed() {
        let svc = service().await;
        let result = svc.create_checkout("ent1", "team");
        assert_eq!(result.status, "manual");
        assert!(result.checkout_url.is_none());
    }

    #[tokio::test]
    async fn model_control_gates_send_by_allowlist_and_budget() {
        let svc = service().await;
        // Red line: no company → always allowed.
        assert!(svc.check_send_allowed("nobody", Some("gpt-4")).await.is_ok());

        add_members(&svc, "entX", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'zoe' WHERE enterprise_id = 'entX'")
            .execute(&svc.pool)
            .await
            .unwrap();

        // Allowlist: only claude-opus-4-8 permitted.
        svc.set_model_control("entX", None, &["claude-opus-4-8".to_owned()])
            .await
            .unwrap();
        assert!(svc.check_send_allowed("zoe", Some("claude-opus-4-8")).await.is_ok());
        assert_eq!(
            svc.check_send_allowed("zoe", Some("gpt-4")).await.unwrap_err().code(),
            "MODEL_NOT_ALLOWED"
        );
        // Unknown model (None) can't be checked → passes the allowlist.
        assert!(svc.check_send_allowed("zoe", None).await.is_ok());
        // Dedicated allowlist-only check (model-switch layer).
        assert!(svc.check_model_allowed("zoe", "claude-opus-4-8").await.is_ok());
        assert_eq!(
            svc.check_model_allowed("zoe", "gpt-4").await.unwrap_err().code(),
            "MODEL_NOT_ALLOWED"
        );
        assert!(svc.check_model_allowed("nobody", "anything").await.is_ok()); // personal red line

        // Spend cap: clear the allowlist, set a tiny cap, then overspend.
        svc.set_model_control("entX", Some(100), &[]).await.unwrap();
        assert!(svc.check_send_allowed("zoe", Some("gpt-4")).await.is_ok()); // under budget so far
        svc.record_turn("zoe", Some("c1"), Some("claude-opus-4-8"), Some(1000), Some(1000))
            .await
            .unwrap(); // ~90000 micros >> 100
        assert_eq!(
            svc.check_send_allowed("zoe", Some("claude-opus-4-8"))
                .await
                .unwrap_err()
                .code(),
            "BUDGET_EXCEEDED"
        );

        // The plan surfaces the cap + spend.
        let plan = svc.plan("entX").await.unwrap();
        assert_eq!(plan.cost_cap_micros, Some(100));
        assert!(plan.cost_used_micros >= 100);
    }

    async fn add_pending_member(svc: &BillingService, enterprise_id: &str, user_id: &str) {
        sqlx::query(
            "INSERT INTO one_enterprise_members (user_id, enterprise_id, role, seat_status, joined_at, updated_at) \
             VALUES (?, ?, 'member', 'pending', 0, 0)",
        )
        .bind(user_id)
        .bind(enterprise_id)
        .execute(&svc.pool)
        .await
        .unwrap();
    }

    /// ⚠️ T6-4, the regression this whole column exists to close. Before it,
    /// a member arriving over the seat cap got no row at all, so
    /// `resolve_enterprise_id` found nothing and treated them as personal —
    /// every gate here would have returned `Ok(())`. The company deliberately
    /// has NEITHER an allowlist NOR a spend cap configured (the worst case:
    /// the two checks below this point would pass literally everyone), so a
    /// green result here can only mean the seat check itself is doing its job.
    #[tokio::test]
    async fn a_pending_member_is_denied_even_with_no_allowlist_or_cap_configured() {
        let svc = service().await;
        add_pending_member(&svc, "entP", "waiting").await;

        assert_eq!(
            svc.check_send_allowed("waiting", Some("gpt-4"))
                .await
                .unwrap_err()
                .code(),
            "SEAT_LIMIT_EXCEEDED"
        );
        assert_eq!(
            svc.check_model_allowed("waiting", "gpt-4").await.unwrap_err().code(),
            "SEAT_LIMIT_EXCEEDED"
        );
        assert_eq!(
            svc.check_media_allowed("waiting", "seedance-2-0-fast")
                .await
                .unwrap_err()
                .code(),
            "SEAT_LIMIT_EXCEEDED"
        );

        // An ACTIVE member in the very same, still-unconfigured company passes
        // — the denial is about this one user's seat, not a company-wide lock.
        add_members(&svc, "entP", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'seated' WHERE enterprise_id = 'entP' AND user_id != 'waiting'")
            .execute(&svc.pool)
            .await
            .unwrap();
        assert!(svc.check_send_allowed("seated", Some("gpt-4")).await.is_ok());
    }

    /// `seat_used` feeds the "X / Y seats" dashboard number and `can_add_seat`;
    /// both must count only ACTIVE seats, or a pending row would either read as
    /// consumed capacity that was never actually granted, or (worse) make
    /// `can_add_seat` refuse forever since a pending row never goes away on its
    /// own.
    #[tokio::test]
    async fn seat_used_and_pending_are_reported_and_counted_separately() {
        let svc = service().await;
        force_tier(&svc, "entP", Tier::Free, None).await;
        sqlx::query("UPDATE one_enterprise_license SET seat_limit = 2 WHERE enterprise_id = 'entP'")
            .execute(&svc.pool)
            .await
            .unwrap();
        add_members(&svc, "entP", 2).await; // both default to 'active'
        add_pending_member(&svc, "entP", "waiting1").await;
        add_pending_member(&svc, "entP", "waiting2").await;

        let plan = svc.plan("entP").await.unwrap();
        assert_eq!(plan.seat_used, 2, "pending rows must not inflate the billed seat count");
        assert_eq!(plan.seat_pending, 2);
        assert_eq!(plan.seat_limit, Some(2));

        // At the cap with 2 pending on top — still full, not "4/2 so there's
        // negative room somehow".
        assert!(!svc.can_add_seat(Some("entP")).await.unwrap());
    }

    /// Billing is enterprise-scoped, so the guard must be too.
    ///
    /// The interesting case is `org_admin`: they administer ONE project group,
    /// but tier / seat cap / spend cap / model allowlist apply to the whole
    /// company. Letting them through would mean group A's admin can move the
    /// budget for every other group. `system_admin` is still accepted because
    /// that is the machine owner on a personal or single-server install, where
    /// no company row exists at all.
    #[tokio::test]
    async fn billing_admin_is_enterprise_scoped_not_project_group_scoped() {
        let svc = service().await;
        sqlx::raw_sql(
            "CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL, PRIMARY KEY (user_id, tenant_id));
             CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL);
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('group_admin', 't1', 'org_admin');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('machine_owner', 't1', 'system_admin');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('plain', 't1', 'member');
             INSERT INTO one_enterprise_members (user_id, enterprise_id, role, joined_at, updated_at) VALUES ('company_admin', 'entA', 'admin', 0, 0);",
        )
        .execute(&svc.pool)
        .await
        .unwrap();

        assert!(
            svc.is_billing_admin("company_admin").await.unwrap(),
            "a company admin owns the company's plan"
        );
        assert!(
            svc.is_billing_admin("machine_owner").await.unwrap(),
            "system_admin must keep working — personal installs have no company row"
        );
        assert!(
            !svc.is_billing_admin("group_admin").await.unwrap(),
            "a project-group admin must not be able to move the whole company's budget"
        );
        assert!(!svc.is_billing_admin("plain").await.unwrap());
    }
    /// Media generation reaches providers through the built-in MCP tool, which
    /// never passed through `SendGate` — so until the precheck existed, the
    /// priciest calls in the product ran outside the allowlist and the cap.
    #[tokio::test]
    async fn media_generation_obeys_the_same_allowlist_and_budget_as_chat() {
        let svc = service().await;
        add_members(&svc, "entM", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'mia' WHERE enterprise_id = 'entM'")
            .execute(&svc.pool)
            .await
            .unwrap();

        // Allowlist covers media models by the same rule as chat models.
        svc.set_model_control("entM", None, &["seedance-2-0-fast".to_owned()])
            .await
            .unwrap();
        assert!(svc.check_media_allowed("mia", "seedance-2-0-fast").await.is_ok());
        assert_eq!(
            svc.check_media_allowed("mia", "gpt-image-2").await.unwrap_err().code(),
            "MODEL_NOT_ALLOWED"
        );

        // Personal / no-company users are never gated — the standing red line.
        assert!(svc.check_media_allowed("nobody", "anything").await.is_ok());

        // Recorded media spend counts against the very same budget as chat, so
        // an expensive video cannot hide from the cap.
        svc.set_model_control("entM", Some(1_000), &[]).await.unwrap();
        assert!(svc.check_media_allowed("mia", "seedance-2-0-fast").await.is_ok());
        svc.record_media_usage(MediaUsage {
            user_id: "mia",
            kind: "video",
            model: "seedance-2-0-fast",
            count: 1,
            duration_seconds: 5,
            unit_price_micros: None,
            conversation_id: Some("conv_1"),
        })
        .await
        .unwrap();
        assert_eq!(
            svc.check_media_allowed("mia", "seedance-2-0-fast")
                .await
                .unwrap_err()
                .code(),
            "BUDGET_EXCEEDED"
        );

        // And it is visible in the dashboard rollup, not just the gate.
        let plan = svc.plan("entM").await.unwrap();
        assert!(plan.cost_used_micros >= 1_000);
    }

    /// The built-in rate table is a coarse illustration; a price the user
    /// entered for their own provider is the contract they are actually billed
    /// under, so it must win.
    #[tokio::test]
    async fn a_user_supplied_price_overrides_the_built_in_rate_table() {
        let svc = service().await;

        // Video: priced per second, so 4s at 2 USD-micros/s is 8.
        svc.record_media_usage(MediaUsage {
            user_id: "solo",
            kind: "video",
            model: "some-unknown-model",
            count: 1,
            duration_seconds: 4,
            unit_price_micros: Some(2),
            conversation_id: None,
        })
        .await
        .unwrap();
        let cost: i64 = sqlx::query_scalar(
            "SELECT estimated_cost_micros FROM one_usage_events WHERE user_id = 'solo' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        // Without a price this model is unknown to the table and would cost 0.
        assert_eq!(cost, 8);

        // Images: priced per asset.
        svc.record_media_usage(MediaUsage {
            user_id: "solo2",
            kind: "image",
            model: "some-unknown-model",
            count: 3,
            duration_seconds: 0,
            unit_price_micros: Some(7),
            conversation_id: None,
        })
        .await
        .unwrap();
        let cost2: i64 =
            sqlx::query_scalar("SELECT estimated_cost_micros FROM one_usage_events WHERE user_id = 'solo2' LIMIT 1")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(cost2, 21);

        // A zero / absent price falls back to the table rather than charging 0.
        svc.record_media_usage(MediaUsage {
            user_id: "solo3",
            kind: "image",
            model: "gpt-image-2",
            count: 1,
            duration_seconds: 0,
            unit_price_micros: Some(0),
            conversation_id: None,
        })
        .await
        .unwrap();
        let cost3: i64 =
            sqlx::query_scalar("SELECT estimated_cost_micros FROM one_usage_events WHERE user_id = 'solo3' LIMIT 1")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(cost3, 40_000);
    }

    /// Media has no token counts; charging it at a token rate would report zero
    /// and let the most expensive calls sit invisibly under any cap.
    #[tokio::test]
    async fn media_usage_is_priced_even_though_it_has_no_tokens() {
        let svc = service().await;
        svc.record_media_usage(MediaUsage {
            user_id: "solo",
            kind: "image",
            model: "gpt-image-2",
            count: 3,
            duration_seconds: 0,
            unit_price_micros: None,
            conversation_id: None,
        })
        .await
        .unwrap();
        let cost: i64 = sqlx::query_scalar(
            "SELECT estimated_cost_micros FROM one_usage_events WHERE user_id = 'solo' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(cost, 3 * 40_000);

        // Token columns stay NULL: media is metered per asset, not per token.
        let tokens: Option<i64> =
            sqlx::query_scalar("SELECT total_tokens FROM one_usage_events WHERE user_id = 'solo' LIMIT 1")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert!(tokens.is_none());
    }

    /// Zero-cost media is not a cosmetic reporting issue: it consumes none of
    /// the spend cap, so the cap quietly stops binding for that model. Pinned
    /// because the built-in table matches on model *name* and a gateway with its
    /// own naming — the common case — misses every entry.
    #[tokio::test]
    async fn an_unrecognised_media_model_costs_nothing_against_the_cap() {
        let svc = service().await;
        svc.record_media_usage(MediaUsage {
            user_id: "solo",
            kind: "image",
            model: "our-gateways-own-name",
            count: 1,
            duration_seconds: 0,
            unit_price_micros: None,
            conversation_id: None,
        })
        .await
        .unwrap();
        let cost: i64 = sqlx::query_scalar(
            "SELECT estimated_cost_micros FROM one_usage_events WHERE user_id = 'solo' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(
            cost, 0,
            "if this ever becomes non-zero, a rate was invented — that is a pricing decision"
        );

        // …and the escape hatch that makes it countable is the user's own price.
        svc.record_media_usage(MediaUsage {
            user_id: "solo",
            kind: "image",
            model: "our-gateways-own-name",
            count: 2,
            duration_seconds: 0,
            unit_price_micros: Some(30_000),
            conversation_id: None,
        })
        .await
        .unwrap();
        let priced: i64 = sqlx::query_scalar(
            "SELECT estimated_cost_micros FROM one_usage_events WHERE user_id = 'solo' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(priced, 60_000);
    }

    /// A charge an admin cannot trace back to anywhere is a charge they cannot
    /// act on. Media started from the compose box writes no conversation
    /// message at all, so this column is the only trail it leaves — it used to
    /// be hard-coded NULL for every media row.
    #[tokio::test]
    async fn media_usage_is_attributed_to_its_conversation() {
        let svc = service().await;
        svc.record_media_usage(MediaUsage {
            user_id: "solo",
            kind: "video",
            model: "seedance-2-0-fast",
            count: 1,
            duration_seconds: 5,
            unit_price_micros: None,
            conversation_id: Some("conv_abc"),
        })
        .await
        .unwrap();
        let conversation: Option<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM one_usage_events WHERE user_id = 'solo' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(conversation.as_deref(), Some("conv_abc"));

        // Still optional: a caller without one records the spend anyway rather
        // than dropping it, because the money was spent either way.
        svc.record_media_usage(MediaUsage {
            user_id: "solo2",
            kind: "image",
            model: "gpt-image-2",
            count: 1,
            duration_seconds: 0,
            unit_price_micros: None,
            conversation_id: None,
        })
        .await
        .unwrap();
        let none_conversation: Option<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM one_usage_events WHERE user_id = 'solo2' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert!(none_conversation.is_none());
    }

    /// Minimal `one_user_org` shape for T7 tests — this crate doesn't own the
    /// table (one-org does) so it isn't created by `run_one_billing_migrations`;
    /// tests exercising department resolution must stand up their own copy,
    /// same as `billing_admin_is_enterprise_scoped_not_project_group_scoped`
    /// already does for role resolution.
    async fn add_user_org(svc: &BillingService, user_id: &str, department_id: Option<&str>) {
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL, department_id TEXT, PRIMARY KEY (user_id, tenant_id))",
        )
        .execute(&svc.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO one_user_org (user_id, tenant_id, role, department_id) VALUES (?, 't1', 'member', ?)")
            .bind(user_id)
            .bind(department_id)
            .execute(&svc.pool)
            .await
            .unwrap();
    }

    /// T7: `record_turn` and `record_media_usage` both stamp the user's
    /// CURRENT department at write time. A user with no `one_user_org` row
    /// (or no department) gets a NULL — never an error, since one-billing
    /// must keep working for personal/standalone installs that have no
    /// one-org table at all.
    #[tokio::test]
    async fn department_id_is_stamped_at_record_time() {
        let svc = service().await;
        add_user_org(&svc, "dana", Some("deptA")).await;

        svc.record_turn("dana", None, Some("gpt-4"), Some(10), Some(10))
            .await
            .unwrap();
        svc.record_media_usage(MediaUsage {
            user_id: "dana",
            kind: "image",
            model: "gpt-image-2",
            count: 1,
            duration_seconds: 0,
            unit_price_micros: None,
            conversation_id: None,
        })
        .await
        .unwrap();

        let depts: Vec<Option<String>> =
            sqlx::query_scalar("SELECT department_id FROM one_usage_events WHERE user_id = 'dana' ORDER BY created_at")
                .fetch_all(&svc.pool)
                .await
                .unwrap();
        assert_eq!(depts, vec![Some("deptA".to_owned()), Some("deptA".to_owned())]);

        // No one_user_org row at all → NULL, not an error.
        svc.record_turn("no_org_row", None, Some("gpt-4"), Some(10), Some(10))
            .await
            .unwrap();
        let none_dept: Option<String> =
            sqlx::query_scalar("SELECT department_id FROM one_usage_events WHERE user_id = 'no_org_row' LIMIT 1")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert!(none_dept.is_none());
    }

    /// The department cap is a tighter constraint layered UNDER the
    /// company-wide one: it must bind even when no company cap is configured
    /// at all, and it must leave members of other departments (and members
    /// with no department) untouched.
    #[tokio::test]
    async fn department_budget_gates_sends_independently_of_company_wide_budget() {
        let svc = service().await;
        add_members(&svc, "entD", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'dana' WHERE enterprise_id = 'entD'")
            .execute(&svc.pool)
            .await
            .unwrap();
        add_user_org(&svc, "dana", Some("deptA")).await;
        add_user_org(&svc, "gio", None).await; // same company, no department

        // No company-wide cap anywhere; only a department cap.
        svc.set_department_budget("entD", "deptA", Some(100)).await.unwrap();
        assert!(svc.check_send_allowed("dana", Some("gpt-4")).await.is_ok());

        svc.record_turn("dana", Some("c1"), Some("claude-opus-4-8"), Some(1000), Some(1000))
            .await
            .unwrap(); // ~90000 micros >> 100
        assert_eq!(
            svc.check_send_allowed("dana", Some("gpt-4")).await.unwrap_err().code(),
            "DEPARTMENT_BUDGET_EXCEEDED"
        );
        // check_media_allowed delegates to check_send_allowed, so media is gated too.
        assert_eq!(
            svc.check_media_allowed("dana", "gpt-4").await.unwrap_err().code(),
            "DEPARTMENT_BUDGET_EXCEEDED"
        );

        // A member with no department is never subject to a department cap.
        assert!(svc.check_send_allowed("gio", Some("gpt-4")).await.is_ok());
    }

    #[tokio::test]
    async fn list_department_budgets_reports_cap_and_usage() {
        let svc = service().await;
        add_user_org(&svc, "dana", Some("deptA")).await;
        svc.set_department_budget("entD", "deptA", Some(500)).await.unwrap();
        svc.record_turn("dana", Some("c1"), Some("claude-opus-4-8"), Some(1000), Some(1000))
            .await
            .unwrap();

        let budgets = svc.list_department_budgets("entD").await.unwrap();
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0].department_id, "deptA");
        assert_eq!(budgets[0].cost_cap_micros, Some(500));
        assert!(budgets[0].cost_used_micros > 0);

        // Clearing the cap keeps the row (still reportable) but drops the cap.
        svc.set_department_budget("entD", "deptA", None).await.unwrap();
        let cleared = svc.list_department_budgets("entD").await.unwrap();
        assert_eq!(cleared[0].cost_cap_micros, None);
    }

    /// Write-time denormalization, not a live join: past spend must stay
    /// attributed to the department a user was in AT THE TIME, or reassigning
    /// someone would retroactively move last month's spend to their new
    /// department's cap.
    #[tokio::test]
    async fn reassigning_a_users_department_does_not_reshuffle_past_spend() {
        let svc = service().await;
        add_user_org(&svc, "dana", Some("deptA")).await;
        svc.record_turn("dana", Some("c1"), Some("claude-opus-4-8"), Some(1000), Some(1000))
            .await
            .unwrap();

        // Move dana to deptB going forward.
        sqlx::query("UPDATE one_user_org SET department_id = 'deptB' WHERE user_id = 'dana'")
            .execute(&svc.pool)
            .await
            .unwrap();
        svc.record_turn("dana", Some("c2"), Some("claude-opus-4-8"), Some(1000), Some(1000))
            .await
            .unwrap();

        let depts: Vec<Option<String>> =
            sqlx::query_scalar("SELECT department_id FROM one_usage_events WHERE user_id = 'dana' ORDER BY created_at")
                .fetch_all(&svc.pool)
                .await
                .unwrap();
        assert_eq!(depts, vec![Some("deptA".to_owned()), Some("deptB".to_owned())]);
    }

    /// T8: enterprise-scoped only — a personal/no-company user's generation
    /// must never create a ledger row. Same red line every other governance
    /// surface in this crate honors.
    #[tokio::test]
    async fn record_media_asset_is_a_noop_for_personal_users() {
        let svc = service().await;
        svc.record_media_asset(
            "nobody",
            "image",
            Some("gpt-image-2"),
            "/tmp/img.png",
            Some("a cat"),
            None,
        )
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_media_assets")
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Prompt retention defaults to OFF and is enforced server-side: the
    /// caller can send whatever it wants, only the company's own opt-in
    /// decides whether it lands in the column.
    #[tokio::test]
    async fn media_asset_prompt_is_retained_only_after_company_opts_in() {
        let svc = service().await;
        add_members(&svc, "entL", 1).await;
        sqlx::query("UPDATE one_enterprise_members SET user_id = 'lee' WHERE enterprise_id = 'entL'")
            .execute(&svc.pool)
            .await
            .unwrap();
        add_user_org(&svc, "lee", Some("deptL")).await;

        // Default: prompt is dropped even though the caller sent one.
        svc.record_media_asset(
            "lee",
            "image",
            Some("gpt-image-2"),
            "/w/a.png",
            Some("a red fox"),
            Some("c1"),
        )
        .await
        .unwrap();
        let prompt_before: Option<String> =
            sqlx::query_scalar("SELECT prompt FROM one_media_assets WHERE file_path = '/w/a.png'")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert!(prompt_before.is_none());

        // Opt in, then the same caller behavior actually retains it.
        svc.set_media_ledger_retain_prompts("entL", true).await.unwrap();
        assert!(svc.media_ledger_retain_prompts("entL").await.unwrap());
        svc.record_media_asset(
            "lee",
            "image",
            Some("gpt-image-2"),
            "/w/b.png",
            Some("a blue fox"),
            Some("c1"),
        )
        .await
        .unwrap();
        let prompt_after: Option<String> =
            sqlx::query_scalar("SELECT prompt FROM one_media_assets WHERE file_path = '/w/b.png'")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(prompt_after.as_deref(), Some("a blue fox"));

        // Department was resolved and attached, same as T7's usage rows.
        let department: Option<String> =
            sqlx::query_scalar("SELECT department_id FROM one_media_assets WHERE file_path = '/w/b.png'")
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(department.as_deref(), Some("deptL"));
    }

    #[tokio::test]
    async fn list_media_assets_filters_by_kind_model_user_since_and_prompt() {
        let svc = service().await;
        svc.set_media_ledger_retain_prompts("entS", true).await.unwrap();
        // `record_media_asset` requires a real enterprise membership row to
        // attribute to (T7/T8 both no-op for personal users) — seeding rows
        // directly here keeps the test focused on `list_media_assets`'s own
        // filter logic rather than membership setup.
        let now = aionui_common::now_ms();
        for (i, (user, kind, model, path, prompt)) in [
            ("ann", "image", "gpt-image-2", "/w/1.png", "a cat in a hat"),
            ("ann", "video", "seedance-2-0-fast", "/w/2.mp4", "a cat running"),
            ("bo", "image", "flux-pro", "/w/3.png", "a dog"),
        ]
        .into_iter()
        .enumerate()
        {
            sqlx::query(
                "INSERT INTO one_media_assets (id, user_id, enterprise_id, department_id, conversation_id, kind, model, file_path, prompt, created_at) \
                 VALUES (?, ?, 'entS', NULL, NULL, ?, ?, ?, ?, ?)",
            )
            .bind(format!("media_{i}"))
            .bind(user)
            .bind(kind)
            .bind(model)
            .bind(path)
            .bind(prompt)
            .bind(now + i as i64)
            .execute(&svc.pool)
            .await
            .unwrap();
        }

        let by_kind = svc
            .list_media_assets(
                "entS",
                MediaAssetFilters {
                    kind: Some("video"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(by_kind.len(), 1);
        assert_eq!(by_kind[0].file_path, "/w/2.mp4");

        let by_user = svc
            .list_media_assets(
                "entS",
                MediaAssetFilters {
                    user_id: Some("ann"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(by_user.len(), 2);

        let by_prompt = svc
            .list_media_assets(
                "entS",
                MediaAssetFilters {
                    prompt_contains: Some("cat"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(by_prompt.len(), 2);

        let by_since = svc
            .list_media_assets(
                "entS",
                MediaAssetFilters {
                    since: Some(now + 2),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(by_since.len(), 1);
        assert_eq!(by_since[0].file_path, "/w/3.png");

        // A company that never opted into retention has NULL prompts, so a
        // prompt search finds nothing — no special-casing, just how NULL LIKE
        // behaves.
        let unretained = svc
            .list_media_assets(
                "entU",
                MediaAssetFilters {
                    prompt_contains: Some("cat"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(unretained.is_empty());
    }
}
