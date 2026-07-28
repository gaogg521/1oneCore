//! Expert marketplace catalog — embeds a curated persona manifest into the
//! binary (mirroring `builtin.rs`'s `include_dir!` approach) and keeps the
//! `assistant_marketplace_personas` table in sync with it at startup.
//!
//! Deliberately kept separate from `AssistantService`/`service.rs`: browsing
//! the marketplace never touches `assistant_definitions` or a user's own
//! assistant list. "Installing" an entry is just calling
//! `AssistantService::import_personas` with this catalog's own name/
//! description/rule_content — see `crates/aionui-assistant/src/routes.rs`.

use include_dir::{Dir, include_dir};
use serde::Deserialize;

use aionui_db::{IAssistantMarketplaceRepository, UpsertMarketplacePersonaParams};

use crate::error::AssistantError;

/// Assets compiled into the binary at build time. Paths are relative to
/// this embedded root, matching the on-disk layout under
/// `crates/aionui-app/assets/marketplace-personas/`.
static MARKETPLACE_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../aionui-app/assets/marketplace-personas");

#[derive(Debug, Deserialize)]
struct MarketplaceManifest {
    #[serde(default)]
    #[allow(dead_code)]
    version: String,
    #[serde(default)]
    personas: Vec<MarketplaceManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct MarketplaceManifestEntry {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// A single catalog entry with its rule content resolved from the embedded
/// `rules/{id}.md` file.
#[derive(Debug, Clone)]
pub struct MarketplacePersona {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub rule_content: String,
}

/// Load and parse the embedded marketplace manifest. Entries whose rule file
/// is missing are skipped (logged, not fatal — one bad entry shouldn't sink
/// the whole catalog).
pub fn load_marketplace_manifest() -> Vec<MarketplacePersona> {
    let Some(manifest_file) = MARKETPLACE_ASSETS.get_file("personas.json") else {
        tracing::warn!("marketplace-personas/personas.json not found in embedded assets");
        return Vec::new();
    };
    let manifest: MarketplaceManifest = match serde_json::from_slice(manifest_file.contents()) {
        Ok(m) => m,
        Err(error) => {
            tracing::warn!(error = %error, "failed to parse marketplace-personas/personas.json");
            return Vec::new();
        }
    };

    manifest
        .personas
        .into_iter()
        .filter_map(|entry| {
            let rule_path = format!("rules/{}.md", entry.id);
            let Some(rule_file) = MARKETPLACE_ASSETS.get_file(&rule_path) else {
                tracing::warn!(id = %entry.id, "marketplace persona missing rule file, skipping");
                return None;
            };
            let rule_content = String::from_utf8_lossy(rule_file.contents()).into_owned();
            Some(MarketplacePersona {
                id: entry.id,
                name: entry.name,
                description: entry.description,
                rule_content,
            })
        })
        .collect()
}

/// Upsert the full embedded catalog into `assistant_marketplace_personas`.
/// Idempotent — safe to call on every startup to keep the table in sync
/// with whatever manifest shipped in this build.
pub async fn materialize_marketplace_personas(
    repo: &dyn IAssistantMarketplaceRepository,
) -> Result<(), AssistantError> {
    let personas = load_marketplace_manifest();
    if personas.is_empty() {
        return Ok(());
    }

    let params: Vec<UpsertMarketplacePersonaParams<'_>> = personas
        .iter()
        .map(|p| UpsertMarketplacePersonaParams {
            id: &p.id,
            source: "workbuddy",
            name: &p.name,
            description: p.description.as_deref(),
            rule_content: &p.rule_content,
        })
        .collect();

    repo.upsert_many(&params).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_marketplace_manifest_reads_embedded_catalog() {
        let personas = load_marketplace_manifest();
        assert!(
            personas.len() > 200,
            "expected the shipped WorkBuddy catalog (~281 entries), got {}",
            personas.len()
        );
        let sample = personas
            .iter()
            .find(|p| p.id == "a-share-advisor")
            .expect("known persona id should be present");
        assert!(!sample.rule_content.trim().is_empty());
        assert!(sample.name.trim() != "");
    }
}
