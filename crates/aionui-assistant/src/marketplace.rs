//! Expert marketplace catalog — embeds a curated persona manifest into the
//! binary (mirroring `builtin.rs`'s `include_dir!` approach) and keeps the
//! `assistant_marketplace_personas` table in sync with it at startup.
//!
//! Deliberately kept separate from `AssistantService`/`service.rs`: browsing
//! the marketplace never touches `assistant_definitions` or a user's own
//! assistant list. "Installing" an entry is just calling
//! `AssistantService::import_personas` with this catalog's own name/
//! description/rule_content — see `crates/aionui-assistant/src/routes.rs`.

use std::collections::HashMap;

use include_dir::{Dir, include_dir};
use serde::Deserialize;
use tracing::{debug, info, warn};

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
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    role_name: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    avatar: bool,
}

/// A single catalog entry with its rule content resolved from the embedded
/// `rules/{id}.md` file.
#[derive(Debug, Clone)]
pub struct MarketplacePersona {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub rule_content: String,
    pub display_name: Option<String>,
    pub role_name: Option<String>,
    pub category: Option<String>,
    pub has_avatar: bool,
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
                display_name: entry.display_name,
                role_name: entry.role_name,
                category: entry.category,
                has_avatar: entry.avatar,
            })
        })
        .collect()
}

/// Read the raw avatar bytes for a marketplace persona from the embedded
/// bundle (`avatars/{id}.webp`). Returns `None` when the manifest doesn't
/// declare an avatar for this id or the file is missing.
pub fn marketplace_avatar_bytes(id: &str) -> Option<Vec<u8>> {
    let path = format!("avatars/{id}.webp");
    MARKETPLACE_ASSETS.get_file(&path).map(|f| f.contents().to_vec())
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
            display_name: p.display_name.as_deref(),
            role_name: p.role_name.as_deref(),
            category: p.category.as_deref(),
            has_avatar: p.has_avatar,
        })
        .collect();

    repo.upsert_many(&params).await?;

    // upsert_many never deletes — without this, swapping the manifest (as
    // happened once already, from a generic template catalog to this
    // WorkBuddy export) leaves the previous generation's ids as permanent
    // orphaned rows.
    let keep_ids: Vec<&str> = personas.iter().map(|p| p.id.as_str()).collect();
    repo.delete_missing(&keep_ids).await?;

    Ok(())
}

/// Move already-installed copies forward when this build ships a new version of
/// their persona — but only the copies the user has never edited.
///
/// Installing writes the catalog's rule text into the user's own assistant, and
/// nothing linked the two afterwards. So shipping an improved persona updated
/// the catalog and left every existing install on the old text permanently:
/// the improvement reached only people who installed it *after* upgrading, and
/// nobody was told a newer version existed. That is the same shape as the media
/// MCP's `enabled` flag, which was computed once at row creation and had to be
/// given an explicit update path for exactly this reason.
///
/// `previous_catalog` is the catalog as it stood *before* this build upserted
/// over it, which is what makes "has the user edited their copy?" answerable
/// without storing a hash: a local copy still byte-identical to what the last
/// build shipped was never touched. Anything else is the user's own text and is
/// left alone — silently overwriting an edited persona would be worse than
/// leaving it stale.
///
/// Failures here are logged and skipped: a persona that cannot be refreshed is
/// still usable at its old version, and must not stop the app from starting.
pub async fn refresh_unedited_installed_personas(
    service: &crate::service::AssistantService,
    previous_catalog: &HashMap<String, String>,
) -> usize {
    let mut refreshed = 0usize;

    for persona in load_marketplace_manifest() {
        // Only personas this build actually changed are worth a disk read —
        // otherwise every start would stat all ~250 catalog entries.
        let Some(previous) = previous_catalog.get(&persona.id) else {
            continue;
        };
        if !catalog_entry_changed(previous, &persona.rule_content) {
            continue;
        }

        // `read_rule` failing is the normal "not installed" case.
        let Ok(local) = service.read_rule(&persona.id, None).await else {
            continue;
        };
        if !local_copy_is_unedited(&local, previous) {
            debug!(
                persona_id = %persona.id,
                "marketplace: installed persona was edited locally; leaving it as the user wrote it"
            );
            continue;
        }

        match service.write_rule(&persona.id, None, &persona.rule_content).await {
            Ok(()) => {
                refreshed += 1;
                info!(
                    persona_id = %persona.id,
                    "marketplace: refreshed an unedited installed persona to this build's version"
                );
            }
            Err(error) => warn!(
                persona_id = %persona.id,
                error = %error,
                "marketplace: failed to refresh installed persona; it stays on the previous version"
            ),
        }
    }

    refreshed
}

/// Compare persona text without letting line endings decide the answer.
///
/// The catalog holds the manifest bytes as authored (CRLF here), while
/// installing writes the rule out through the assistant storage layer, which
/// normalises to LF. The two are therefore *never* byte-identical even when
/// nothing was edited — a byte comparison would classify every install as
/// "user-modified" and quietly turn this whole refresh into dead code.
///
/// Caught by diffing the real rows on a live machine (catalog 2192 bytes vs
/// installed 2137, the difference being exactly the newline count), not by
/// reading the code — the byte comparison looks entirely correct.
fn same_persona_text(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

/// Whether this build ships a different text than the last one did.
///
/// Guards the disk read: with ~250 catalog entries, checking every install on
/// every start would cost hundreds of file reads to discover nothing changed.
fn catalog_entry_changed(previous: &str, shipped: &str) -> bool {
    !same_persona_text(previous, shipped)
}

/// Whether the installed copy is still what the last build shipped.
///
/// This is the whole safety property. Anything but a match means the user has
/// their own version — refreshing it would silently discard work they did,
/// which is strictly worse than leaving them on an older persona.
fn local_copy_is_unedited(local: &str, previous: &str) -> bool {
    same_persona_text(local, previous)
}

/// The catalog as it currently stands, for comparison after the upsert.
pub async fn snapshot_catalog_rules(
    repo: &dyn IAssistantMarketplaceRepository,
) -> Result<HashMap<String, String>, AssistantError> {
    Ok(repo
        .list()
        .await?
        .into_iter()
        .map(|row| (row.id, row.rule_content))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog stores the manifest's own CRLF bytes; installing writes the
    /// rule out as LF. Comparing bytes would mark every untouched install as
    /// edited and silently disable the refresh entirely — verified against a
    /// live machine, where the same persona measured 2192 bytes in the catalog
    /// and 2137 on disk.
    #[test]
    fn line_endings_alone_do_not_make_a_copy_look_edited() {
        let catalog = "# 视频生成专家\r\n\r\n3. 技术栈默认 Remotion\r\n";
        let installed = "# 视频生成专家\n\n3. 技术栈默认 Remotion\n";

        assert_ne!(catalog, installed, "the fixture must actually differ in bytes");
        assert!(local_copy_is_unedited(installed, catalog));
        assert!(!catalog_entry_changed(catalog, installed));
    }

    /// The refresh must never overwrite a persona the user has edited. This is
    /// the property the whole feature turns on, so it is pinned directly.
    #[test]
    fn an_edited_local_copy_is_never_refreshed() {
        let previous = "3. 技术栈默认 Remotion";
        let shipped = "3. 动手前先判断该走哪条路";

        assert!(catalog_entry_changed(previous, shipped));
        for edited in [
            "3. 技术栈默认 Remotion\n\n我自己加的一条",
            "完全重写过的人设",
            "3. 技术栈默认 Remotion ", // even a trailing space is the user's
        ] {
            assert!(
                !local_copy_is_unedited(edited, previous),
                "must not overwrite: {edited:?}"
            );
        }
    }

    #[test]
    fn an_untouched_local_copy_is_refreshed_when_the_build_ships_a_new_version() {
        let previous = "3. 技术栈默认 Remotion";
        let shipped = "3. 动手前先判断该走哪条路";

        assert!(catalog_entry_changed(previous, shipped));
        assert!(local_copy_is_unedited(previous, previous));
    }

    /// Most starts ship the same catalog as the last one. Nothing may be read
    /// or written then — otherwise every launch pays ~250 file reads.
    #[test]
    fn an_unchanged_catalog_entry_is_left_entirely_alone() {
        let same = "3. 技术栈默认 Remotion";

        assert!(!catalog_entry_changed(same, same));
    }

    #[test]
    fn load_marketplace_manifest_reads_embedded_catalog() {
        let personas = load_marketplace_manifest();
        assert!(
            personas.len() > 200,
            "expected the shipped WorkBuddy catalog (~252 entries), got {}",
            personas.len()
        );
        let sample = personas
            .iter()
            .find(|p| p.id == "AShareAnalysis")
            .expect("known persona id should be present");
        assert!(!sample.rule_content.trim().is_empty());
        assert!(sample.name.trim() != "");
        assert_eq!(sample.display_name.as_deref(), Some("A股研究团队"));
        assert_eq!(sample.category.as_deref(), Some("金融投资"));
        assert!(sample.has_avatar);
    }

    #[test]
    fn marketplace_avatar_bytes_returns_data_for_known_id_and_none_for_unknown() {
        assert!(marketplace_avatar_bytes("AShareAnalysis").is_some());
        assert!(marketplace_avatar_bytes("does-not-exist").is_none());
    }
}
