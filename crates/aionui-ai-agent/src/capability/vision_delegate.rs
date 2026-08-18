//! Vision-delegate resolution shared by aionrs's `ReadImage` and the ACP
//! (Claude/Codex bridge) vision fallback.
//!
//! Moved out of `factory/aionrs.rs`, where it originated, because it is
//! fully backend-agnostic: nothing in it depends on aionrs specifically, and
//! `factory/acp.rs` needs the exact same "pick a vision-capable model from
//! the user's providers, gated by company policy" logic for bridged Claude/
//! Codex sessions whose actual model cannot see images either.

use std::collections::HashMap;

use aion_types::message::ImageInputCapability;
use tracing::{info, warn};

use crate::factory::aionrs::{
    map_aionrs_provider, resolve_aionrs_url_and_compat_with_mode, resolve_model_compat_overrides,
    resolve_provider_config_for_bridge,
};

/// Resolved vision policy for an ACP (Claude/Codex bridge) session.
///
/// Computed once at session-build time in `factory/acp.rs::build()` and
/// cached on `AcpSessionParams` for the session's lifetime — mirrors how the
/// session's own model is resolved once, not re-checked per message.
#[derive(Debug, Clone, Default)]
pub enum AcpVisionPolicy {
    /// Native CLI login (no bridge), or the bridged model itself already
    /// supports images. The image-attachment hook is a no-op in both cases.
    #[default]
    NotBridged,
    /// The bridged model cannot see images, but a delegate was found.
    Delegate(Box<aion_config::config::VisionModelConfig>),
    /// The bridged model cannot see images and no delegate is available
    /// (nothing configured, or everything that qualified was policy-blocked).
    Unavailable { reason: Option<String> },
}

/// Find a vision-capable model among the user's configured providers so a
/// text-only session still has a way to read images.
///
/// Capability is decided by exactly the same rules the session model goes
/// through — an explicit per-model `image_input` setting, otherwise
/// [`crate::capability::image_input::resolve_image_input_capability`]'s
/// allowlist. This function only widens *which models are examined*; it
/// never relaxes the judgement, so a text-only look-alike such as
/// `deepseek-v4-flash` is rejected here too.
///
/// Company policy is a second, independent filter layered on top: a model the
/// admin removed from the allowlist must not come back in through this door.
/// See [`crate::model_policy::ModelAllowlistGate`] for why the check arrives as
/// a trait rather than a direct `one-billing` call.
///
/// Returns an empty [`VisionDelegate`] when nothing qualifies, which the
/// caller (`ReadImage` for aionrs, the image-attachment hook for ACP)
/// reports to the user as an actionable error.
pub(crate) async fn resolve_vision_delegate(
    provider_repo: &dyn aionui_db::IProviderRepository,
    encryption_key: &[u8],
    user_id: &str,
    conversation_id: &str,
    allowlist: Option<&dyn crate::model_policy::ModelAllowlistGate>,
) -> VisionDelegate {
    let rows = match provider_repo.list(user_id).await {
        Ok(rows) => rows,
        Err(error) => {
            warn!(
                conversation_id = %conversation_id,
                error = %error,
                "Failed to list providers while looking for a vision delegate"
            );
            return VisionDelegate::default();
        }
    };
    let mut policy_blocked: Vec<String> = Vec::new();

    for row in rows {
        if !row.enabled {
            continue;
        }
        let models = serde_json::from_str::<Vec<String>>(&row.models).unwrap_or_default();
        let model_enabled = row
            .model_enabled
            .as_deref()
            .and_then(|json| serde_json::from_str::<HashMap<String, bool>>(json).ok())
            .unwrap_or_default();

        for model in models {
            if model_enabled.get(&model) == Some(&false) {
                continue;
            }
            // A malformed protocol/settings entry disqualifies this candidate
            // only; it must never abort the session being built.
            let Ok(provider) = map_aionrs_provider(&row.platform, &model, row.model_protocols.as_deref()) else {
                continue;
            };
            let Ok(model_overrides) = resolve_model_compat_overrides(&model, &row.model_settings) else {
                continue;
            };
            let (base_url, _) = resolve_aionrs_url_and_compat_with_mode(
                &row.platform,
                &row.base_url,
                &provider,
                &model,
                row.is_full_url,
                model_overrides.openai_api_mode,
            );
            let capability = model_overrides.image_input.unwrap_or_else(|| {
                crate::capability::image_input::resolve_image_input_capability(&provider, base_url.as_deref(), &model)
            });
            if !capability.supports_images() {
                continue;
            }

            // Company allowlist. Checked after the capability filter so the log
            // only names models that were otherwise about to be used, and
            // *before* the provider config is decrypted — a model the admin
            // banned has no business having its credentials unwrapped.
            if let Some(gate) = allowlist {
                match gate.is_model_allowed(user_id, &model).await {
                    Ok(true) => {}
                    Ok(false) => {
                        info!(
                            conversation_id = %conversation_id,
                            vision_provider = %row.id,
                            vision_model = %model,
                            "Vision-capable model skipped: not allowed by the company's model policy"
                        );
                        policy_blocked.push(model);
                        continue;
                    }
                    // Fail closed, same posture as `BillingSendGate`: a policy
                    // that cannot be evaluated is not a policy that passed.
                    Err(error) => {
                        warn!(
                            conversation_id = %conversation_id,
                            vision_provider = %row.id,
                            vision_model = %model,
                            error = %error,
                            "Model policy check failed; skipping this vision delegate candidate"
                        );
                        policy_blocked.push(model);
                        continue;
                    }
                }
            }

            match resolve_provider_config_for_bridge(provider_repo, encryption_key, user_id, &row.id, &model).await {
                Ok(mut config) => {
                    config.compat.image_input = Some(ImageInputCapability::Supported);
                    info!(
                        conversation_id = %conversation_id,
                        vision_provider = %row.id,
                        vision_model = %model,
                        "Resolved vision delegate for text-only session"
                    );
                    return VisionDelegate {
                        config: Some(aion_config::config::VisionModelConfig::from_config(&config)),
                        policy_blocked,
                    };
                }
                Err(error) => {
                    warn!(
                        conversation_id = %conversation_id,
                        vision_provider = %row.id,
                        vision_model = %model,
                        error = %error,
                        "Vision-capable model could not be resolved into a provider config"
                    );
                }
            }
        }
    }

    info!(
        conversation_id = %conversation_id,
        policy_blocked = policy_blocked.len(),
        "No vision-capable model available; images will be reported as unreadable"
    );
    VisionDelegate {
        config: None,
        policy_blocked,
    }
}

/// Outcome of [`resolve_vision_delegate`].
///
/// `policy_blocked` exists so the "no vision model" message can tell the truth.
/// Without it, a company that banned every vision model produces exactly the
/// same advice as a user who configured none ("add a model that accepts
/// images") — advice that cannot work and that the user will keep retrying.
#[derive(Debug, Default)]
pub(crate) struct VisionDelegate {
    pub(crate) config: Option<aion_config::config::VisionModelConfig>,
    /// Models that passed the capability check but were refused by company
    /// policy (or whose policy check itself failed — same user-visible answer).
    pub(crate) policy_blocked: Vec<String>,
}

impl VisionDelegate {
    /// User-facing explanation of why no delegate is available, when the reason
    /// is company policy rather than a missing configuration.
    pub(crate) fn unavailable_reason(&self) -> Option<String> {
        if self.config.is_some() || self.policy_blocked.is_empty() {
            return None;
        }
        Some(format!(
            "Your organization's model policy does not allow the vision-capable model(s) configured here ({}). \
             Ask an administrator to add one to the allowed models list.",
            self.policy_blocked.join(", ")
        ))
    }
}

#[cfg(test)]
#[path = "vision_delegate_test.rs"]
mod vision_delegate_test;
