//! License tiers, feature entitlements, seat caps, and usage cost estimation.
//!
//! Pure, dependency-free policy logic — the **single source of truth** for what
//! each tier includes, shared by `one-billing` (the storage / enforcement crate)
//! and every gating crate (one-sso / one-org / one-devops / one-enterprise) so
//! the matrix is never duplicated. Storage of a company's chosen tier lives in
//! `one-billing`; this module only maps tier → entitlements.
//!
//! Red line: personal / standalone users are **not** in the licensing system at
//! all. Callers must treat "no enterprise" as fully allowed *before* consulting
//! this matrix — the matrix only answers "given a tier, what is allowed".

use serde::{Deserialize, Serialize};

/// Subscription tier a company (SSO enterprise) is on. String form is the
/// on-the-wire / on-disk representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Entry tier: small seat cap, no advanced features.
    Free,
    /// Mid tier: SSO + team resource distribution.
    Team,
    /// Top tier: everything, unlimited seats.
    Enterprise,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Free => "free",
            Tier::Team => "team",
            Tier::Enterprise => "enterprise",
        }
    }

    /// Parse a stored tier string. Unknown / malformed values fall back to
    /// `Free` (the least-privileged tier) so a bad row never over-grants.
    pub fn parse(s: &str) -> Tier {
        match s.trim().to_ascii_lowercase().as_str() {
            "team" => Tier::Team,
            "enterprise" => Tier::Enterprise,
            _ => Tier::Free,
        }
    }
}

/// A gate-able capability. Enforced only for companies; personal mode bypasses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Single sign-on (configure providers + SSO login).
    Sso,
    /// Enterprise audit log visibility.
    AuditLog,
    /// Distributing skills / MCP / knowledge scoped to a specific project group
    /// (P0-4 `scope='team'`).
    TeamResourceScope,
    /// Admin-only read visibility on distributed resources (P0-4
    /// `visibility='admin'`).
    AdminOnlyVisibility,
    /// Fine-grained / custom RBAC beyond the built-in three roles.
    FineGrainedRbac,
}

/// Every feature, for building an entitlement map for the client.
pub const ALL_FEATURES: [Feature; 5] = [
    Feature::Sso,
    Feature::AuditLog,
    Feature::TeamResourceScope,
    Feature::AdminOnlyVisibility,
    Feature::FineGrainedRbac,
];

impl Feature {
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::Sso => "sso",
            Feature::AuditLog => "audit_log",
            Feature::TeamResourceScope => "team_resource_scope",
            Feature::AdminOnlyVisibility => "admin_only_visibility",
            Feature::FineGrainedRbac => "fine_grained_rbac",
        }
    }
}

/// Whether `tier` includes `feature`. The product matrix — adjust here only.
pub fn tier_allows(tier: Tier, feature: Feature) -> bool {
    match (tier, feature) {
        // Enterprise: everything.
        (Tier::Enterprise, _) => true,
        // Team: SSO + team resource distribution; no audit / fine-grained RBAC.
        (Tier::Team, Feature::Sso) => true,
        (Tier::Team, Feature::TeamResourceScope) => true,
        (Tier::Team, _) => false,
        // Free: no advanced features.
        (Tier::Free, _) => false,
    }
}

/// Seat cap for a tier. `None` = unlimited. A stored `seat_limit` override on
/// the license row takes precedence over this default (handled in one-billing).
pub fn tier_seat_limit(tier: Tier) -> Option<u32> {
    match tier {
        Tier::Free => Some(3),
        Tier::Team => Some(25),
        Tier::Enterprise => None,
    }
}

/// Per-1K-token cost in USD-micros (1e-6 USD) as `(input, output)`. Rough
/// estimates for the usage dashboard only — never a billing source of truth.
/// Unknown models return `(0, 0)`; the dashboard labels cost as an estimate.
fn model_rate_micros_per_1k(model: &str) -> (i64, i64) {
    let m = model.to_ascii_lowercase();
    // Coarse buckets by family; extend as needed. Values are illustrative.
    if m.contains("opus") {
        (15_000, 75_000)
    } else if m.contains("sonnet") {
        (3_000, 15_000)
    } else if m.contains("haiku") {
        (800, 4_000)
    } else if m.contains("gpt-4") || m.contains("gpt4") {
        (10_000, 30_000)
    } else if m.contains("deepseek") || m.contains("glm") || m.contains("kimi") {
        (500, 1_500)
    } else {
        (0, 0)
    }
}

/// Estimate a turn's cost in USD-micros from token counts. Best-effort; returns
/// 0 for unknown models or when token counts are unavailable.
pub fn estimate_cost_micros(model: &str, input_tokens: i64, output_tokens: i64) -> i64 {
    let (in_rate, out_rate) = model_rate_micros_per_1k(model);
    let input = input_tokens.max(0);
    let output = output_tokens.max(0);
    (input * in_rate) / 1000 + (output * out_rate) / 1000
}

/// Per-image cost in USD-micros. Media is not metered in tokens, so it needs its
/// own table — charging it at a token rate would report zero and let the most
/// expensive calls in the product sit invisibly under any spend cap.
fn image_rate_micros(model: &str) -> i64 {
    let m = model.to_ascii_lowercase();
    if m.contains("gpt-image") || m.contains("dall-e-3") {
        40_000
    } else if m.contains("seedream") || m.contains("jimeng") {
        20_000
    } else if m.contains("flux") || m.contains("stable-diffusion") || m.contains("sd3") {
        10_000
    } else if m.contains("wanx") || m.contains("cogview") || m.contains("dall-e") {
        15_000
    } else if m.contains("image") || m.contains("imagine") || m.contains("banana") {
        // Chat-multimodal image models (gemini image-preview and friends).
        10_000
    } else {
        0
    }
}

/// Per-second-of-video cost in USD-micros. Video is the priciest thing the
/// product can invoke, which is exactly why it must not be free to the meter.
fn video_rate_micros_per_second(model: &str) -> i64 {
    let m = model.to_ascii_lowercase();
    if m.contains("sora") || m.contains("veo") {
        500_000
    } else if m.contains("seedance") || m.contains("kling") {
        200_000
    } else if m.contains("wan") || m.contains("cogvideox") || m.contains("vidu") {
        100_000
    } else {
        0
    }
}

/// Estimate a media generation's cost in USD-micros.
///
/// `count` is the number of assets produced; `duration_seconds` applies to video
/// and is ignored for images. Unknown models return 0, same convention as the
/// token estimator — the dashboard labels every figure an estimate.
pub fn estimate_media_cost_micros(kind: &str, model: &str, count: i64, duration_seconds: i64) -> i64 {
    let count = count.max(0);
    if kind.eq_ignore_ascii_case("video") {
        // Treat a missing duration as the common 5s clip rather than free.
        let seconds = if duration_seconds > 0 { duration_seconds } else { 5 };
        count * seconds * video_rate_micros_per_second(model)
    } else {
        count * image_rate_micros(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_parse_defaults_to_free_on_unknown() {
        assert_eq!(Tier::parse("team"), Tier::Team);
        assert_eq!(Tier::parse("ENTERPRISE"), Tier::Enterprise);
        assert_eq!(Tier::parse("bogus"), Tier::Free);
        assert_eq!(Tier::parse(""), Tier::Free);
    }

    /// Media is metered per asset / per second, never per token. Regressing
    /// this to the token estimator would silently price every image and video
    /// at zero — the failure mode is invisible, which is why it is pinned.
    #[test]
    fn media_is_priced_per_asset_and_per_second() {
        // Images: cost scales with how many were produced.
        assert_eq!(estimate_media_cost_micros("image", "gpt-image-2", 1, 0), 40_000);
        assert_eq!(estimate_media_cost_micros("image", "gpt-image-2", 3, 0), 120_000);

        // Video: cost scales with duration too, and is the priciest kind.
        assert_eq!(
            estimate_media_cost_micros("video", "seedance-2-0-fast", 1, 5),
            1_000_000
        );
        assert_eq!(estimate_media_cost_micros("video", "sora-2", 1, 10), 5_000_000);

        // A missing duration must not make a video free; it falls back to a
        // typical clip rather than zero.
        assert_eq!(
            estimate_media_cost_micros("video", "seedance-2-0-fast", 1, 0),
            estimate_media_cost_micros("video", "seedance-2-0-fast", 1, 5)
        );

        // Unknown models estimate 0, same convention as the token estimator.
        assert_eq!(estimate_media_cost_micros("image", "some-unknown-model", 2, 0), 0);

        // Negative / nonsense counts never produce a negative charge.
        assert_eq!(estimate_media_cost_micros("image", "gpt-image-2", -5, 0), 0);
    }

    #[test]
    fn entitlement_matrix() {
        // Enterprise: all on.
        assert!(ALL_FEATURES.iter().all(|f| tier_allows(Tier::Enterprise, *f)));
        // Team: SSO + team scope only.
        assert!(tier_allows(Tier::Team, Feature::Sso));
        assert!(tier_allows(Tier::Team, Feature::TeamResourceScope));
        assert!(!tier_allows(Tier::Team, Feature::AuditLog));
        assert!(!tier_allows(Tier::Team, Feature::FineGrainedRbac));
        // Free: nothing.
        assert!(ALL_FEATURES.iter().all(|f| !tier_allows(Tier::Free, *f)));
    }

    #[test]
    fn seat_limits() {
        assert_eq!(tier_seat_limit(Tier::Free), Some(3));
        assert_eq!(tier_seat_limit(Tier::Team), Some(25));
        assert_eq!(tier_seat_limit(Tier::Enterprise), None);
    }

    #[test]
    fn cost_estimate_is_best_effort() {
        // Known family.
        assert!(estimate_cost_micros("claude-opus-4-8", 1000, 1000) > 0);
        // Unknown model → 0, never negative on junk input.
        assert_eq!(estimate_cost_micros("mystery-model", 1000, 1000), 0);
        assert_eq!(estimate_cost_micros("claude-opus", -5, -5), 0);
    }
}
