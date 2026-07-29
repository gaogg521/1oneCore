//! Billing-plane business logic: license tier, seat enforcement, usage
//! metering, and the (stubbed) payment provider.
//!
//! License attaches to an SSO company (`one_enterprises`). A user's company is
//! resolved from `one_enterprise_members`. Personal / standalone users have no
//! company row → not in the billing system (every check is permissive, seats
//! uncounted, usage recorded with a NULL enterprise). This is the red line.

use std::sync::Arc;

use aionui_common::license::{Feature, Tier, estimate_cost_micros, tier_allows, tier_seat_limit};
use aionui_common::{generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::error::BillingError;
use crate::models::{CheckoutResultDto, EntitlementDto, LicenseInfoDto, PlanDto, UsageBucketDto, UsageSummaryDto};

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

    async fn license_of(&self, enterprise_id: &str) -> Result<License, BillingError> {
        let row: Option<(String, Option<i64>, Option<i64>, Option<i64>, Option<String>)> = sqlx::query_as(
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

    async fn seat_used(&self, enterprise_id: &str) -> Result<i64, BillingError> {
        let used: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprise_members WHERE enterprise_id = ?")
            .bind(enterprise_id)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        Ok(used)
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

    /// Pre-send gate (P1-2): reject when the company is over its spend budget,
    /// or the requested `model` is not on its allowlist. Personal / no-company
    /// users, and companies with neither control set, always pass (red line).
    pub async fn check_send_allowed(&self, user_id: &str, model: Option<&str>) -> Result<(), BillingError> {
        let Some(enterprise_id) = self.resolve_enterprise_id(user_id).await? else {
            return Ok(());
        };
        let license = self.license_of(&enterprise_id).await?;

        // Model allowlist.
        if !license.allowed_models.is_empty()
            && let Some(model) = model.map(str::trim).filter(|s| !s.is_empty())
            && !license.allowed_models.iter().any(|m| m == model)
        {
            return Err(BillingError::ModelNotAllowed(model.to_owned()));
        }

        // Spend cap.
        if let Some(cap) = license.cost_cap_micros
            && self.budget_used_micros(&enterprise_id).await? >= cap
        {
            return Err(BillingError::BudgetExceeded);
        }
        Ok(())
    }

    /// Allowlist-only check (P1-2): whether `model` may be selected under the
    /// company policy. Used at the model-switch point (budget is enforced
    /// separately at send). Personal / no-allowlist → allowed.
    pub async fn check_model_allowed(&self, user_id: &str, model: &str) -> Result<(), BillingError> {
        let Some(enterprise_id) = self.resolve_enterprise_id(user_id).await? else {
            return Ok(());
        };
        let license = self.license_of(&enterprise_id).await?;
        let model = model.trim();
        if !license.allowed_models.is_empty() && !model.is_empty() && !license.allowed_models.iter().any(|m| m == model)
        {
            return Err(BillingError::ModelNotAllowed(model.to_owned()));
        }
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
        let total_tokens = match (input_tokens, output_tokens) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        let cost = model.map(|m| estimate_cost_micros(m, input_tokens.unwrap_or(0), output_tokens.unwrap_or(0)));
        sqlx::query(
            "INSERT INTO one_usage_events \
                (id, user_id, enterprise_id, conversation_id, model, input_tokens, output_tokens, total_tokens, estimated_cost_micros, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(generate_prefixed_id("usage"))
        .bind(user_id)
        .bind(enterprise_id)
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

        let (total_turns, total_tokens, total_cost): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(SUM(estimated_cost_micros), 0) \
             FROM one_usage_events WHERE enterprise_id = ? AND created_at >= ?",
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

    /// Whether the caller may see the usage dashboard / provision a tier: a
    /// company admin (`one_enterprise_members.role='admin'`) or a server
    /// org/system admin. Tolerant of absent tables (personal mode → false).
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
        Ok(matches!(org_role.as_deref(), Some("system_admin") | Some("org_admin")))
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
}
