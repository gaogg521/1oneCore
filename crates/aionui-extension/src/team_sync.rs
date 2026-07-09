//! Team skill sync — materialize team-distributed skills onto the member's
//! disk so they survive server outages (offline-first) and are picked up by
//! the normal skill loader.
//!
//! Distribution model (see `docs/guides/enterprise-product-master-plan.zh-CN.md`,
//! mechanism M3): the enterprise admin defines team skills in the server-side
//! `one-devops` registry. A member (client mode) fetches the ones visible to
//! them and calls [`sync_team_skills`], which writes each as a real `SKILL.md`
//! under `{data_dir}/team-skills/{id}/` plus a `.team-origin` marker.
//!
//! Reconciliation deletes only skills the server no longer serves, and only
//! when the fetch was **authoritative** (server reachable). A transient
//! network failure passes `authoritative = false`, so cached team skills are
//! kept and stay usable offline — exactly the "server down doesn't affect use,
//! admin delete does" contract.

use std::collections::HashSet;
use std::path::Path;

use crate::constants::SKILL_MANIFEST_FILE;
use crate::error::ExtensionError;

/// Marker file dropped in every materialized team-skill directory so
/// reconciliation only ever touches dirs this sync owns (never a user skill
/// that happens to share the folder). Holds the source registry id.
const TEAM_ORIGIN_MARKER: &str = ".team-origin";

/// Marker for admin-required (auto-active) team skills: member agents load
/// them without a per-assistant opt-in (mixed distribution model).
pub(crate) const TEAM_AUTO_MARKER: &str = ".team-auto";

/// One team skill as fetched from the server registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamSkillPayload {
    /// Stable server registry id; used as the on-disk directory name.
    pub id: String,
    pub name: String,
    pub description: String,
    /// SKILL.md body, or a full SKILL.md (with its own frontmatter).
    pub content: String,
    /// Admin marked this skill auto-active for member agents.
    pub auto_active: bool,
}

/// Outcome of a sync pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TeamSyncReport {
    /// Directory ids written or refreshed this pass.
    pub written: Vec<String>,
    /// Directory ids removed by reconciliation (server no longer serves them).
    pub removed: Vec<String>,
    /// Number of team skills present after the pass.
    pub kept: usize,
}

/// Reduce an arbitrary registry id to a safe single-segment directory name.
/// Registry ids are already `oskill_<uuid>`-shaped, but we defend against
/// path traversal regardless.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Build a well-formed `SKILL.md`. If the registry content already carries
/// YAML frontmatter (`---` at the very start) we trust it as a complete file;
/// otherwise we synthesize frontmatter from the name/description so the skill
/// loader's frontmatter parser can pick up name + description.
fn build_skill_md(payload: &TeamSkillPayload) -> String {
    if payload.content.trim_start().starts_with("---") {
        return payload.content.clone();
    }
    let name = payload.name.trim();
    let description = payload.description.trim().replace(['\r', '\n'], " ");
    format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n{}",
        payload.content
    )
}

/// Materialize `payloads` under `team_skills_dir` and reconcile removals.
///
/// `authoritative` MUST be `true` only when the payload set is the complete,
/// current server view (server reachable). When `false`, no deletion happens
/// so offline / partial-failure never wipes the local cache.
pub async fn sync_team_skills(
    team_skills_dir: &Path,
    payloads: &[TeamSkillPayload],
    authoritative: bool,
) -> Result<TeamSyncReport, ExtensionError> {
    tokio::fs::create_dir_all(team_skills_dir).await?;

    let mut report = TeamSyncReport::default();
    let mut wanted: HashSet<String> = HashSet::new();

    for payload in payloads {
        let dir_id = sanitize_id(&payload.id);
        if dir_id.is_empty() {
            continue;
        }
        let skill_dir = team_skills_dir.join(&dir_id);
        tokio::fs::create_dir_all(&skill_dir).await?;
        tokio::fs::write(skill_dir.join(SKILL_MANIFEST_FILE), build_skill_md(payload)).await?;
        tokio::fs::write(skill_dir.join(TEAM_ORIGIN_MARKER), payload.id.as_bytes()).await?;
        let auto_marker = skill_dir.join(TEAM_AUTO_MARKER);
        if payload.auto_active {
            tokio::fs::write(&auto_marker, b"1").await?;
        } else if let Err(e) = tokio::fs::remove_file(&auto_marker).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(ExtensionError::Io(e));
        }
        wanted.insert(dir_id.clone());
        report.written.push(dir_id);
    }

    if authoritative {
        let mut entries = match tokio::fs::read_dir(team_skills_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(ExtensionError::Io(e)),
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Only reconcile dirs this sync owns.
            if !path.join(TEAM_ORIGIN_MARKER).exists() {
                continue;
            }
            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if !wanted.contains(&dir_name) {
                tokio::fs::remove_dir_all(&path).await?;
                report.removed.push(dir_name);
            }
        }
    }

    report.kept = wanted.len();
    report.written.sort();
    report.removed.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(id: &str, name: &str, content: &str) -> TeamSkillPayload {
        TeamSkillPayload {
            id: id.to_string(),
            name: name.to_string(),
            description: format!("{name} description"),
            content: content.to_string(),
            auto_active: false,
        }
    }

    #[tokio::test]
    async fn auto_marker_written_and_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("team-skills");
        let mut p = payload("oskill_auto", "auto-skill", "body");
        p.auto_active = true;
        sync_team_skills(&dir, &[p.clone()], true).await.unwrap();
        assert!(dir.join("oskill_auto").join(TEAM_AUTO_MARKER).exists());

        // Admin flips it back to opt-in → marker cleared on resync.
        p.auto_active = false;
        sync_team_skills(&dir, &[p], true).await.unwrap();
        assert!(!dir.join("oskill_auto").join(TEAM_AUTO_MARKER).exists());
    }

    #[tokio::test]
    async fn writes_skill_md_with_synthesized_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("team-skills");
        let report = sync_team_skills(&dir, &[payload("oskill_a", "alpha", "do alpha")], true)
            .await
            .unwrap();
        assert_eq!(report.written, vec!["oskill_a".to_string()]);
        assert_eq!(report.kept, 1);

        let md = std::fs::read_to_string(dir.join("oskill_a").join(SKILL_MANIFEST_FILE)).unwrap();
        assert!(md.starts_with("---\nname: alpha\n"));
        assert!(md.contains("do alpha"));
        assert!(dir.join("oskill_a").join(TEAM_ORIGIN_MARKER).exists());
    }

    #[tokio::test]
    async fn preserves_full_skill_md_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("team-skills");
        let full = "---\nname: beta\ndescription: pre-authored\n---\nbody";
        sync_team_skills(&dir, &[payload("oskill_b", "ignored", full)], true)
            .await
            .unwrap();
        let md = std::fs::read_to_string(dir.join("oskill_b").join(SKILL_MANIFEST_FILE)).unwrap();
        assert_eq!(md, full);
    }

    #[tokio::test]
    async fn authoritative_sync_removes_server_deleted_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("team-skills");
        sync_team_skills(
            &dir,
            &[payload("oskill_a", "alpha", "a"), payload("oskill_b", "beta", "b")],
            true,
        )
        .await
        .unwrap();

        // Server now only serves alpha -> beta must be reconciled away.
        let report = sync_team_skills(&dir, &[payload("oskill_a", "alpha", "a")], true)
            .await
            .unwrap();
        assert_eq!(report.removed, vec!["oskill_b".to_string()]);
        assert!(dir.join("oskill_a").exists());
        assert!(!dir.join("oskill_b").exists());
    }

    #[tokio::test]
    async fn non_authoritative_sync_keeps_cache_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("team-skills");
        sync_team_skills(
            &dir,
            &[payload("oskill_a", "alpha", "a"), payload("oskill_b", "beta", "b")],
            true,
        )
        .await
        .unwrap();

        // Offline: empty payload but NOT authoritative -> nothing removed.
        let report = sync_team_skills(&dir, &[], false).await.unwrap();
        assert!(report.removed.is_empty());
        assert!(dir.join("oskill_a").exists());
        assert!(dir.join("oskill_b").exists());
    }

    #[tokio::test]
    async fn reconcile_never_touches_foreign_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("team-skills");
        tokio::fs::create_dir_all(dir.join("hand-made")).await.unwrap();
        tokio::fs::write(dir.join("hand-made").join(SKILL_MANIFEST_FILE), "x")
            .await
            .unwrap();

        // Authoritative empty sync: the foreign dir has no marker, so it stays.
        let report = sync_team_skills(&dir, &[], true).await.unwrap();
        assert!(report.removed.is_empty());
        assert!(dir.join("hand-made").exists());
    }

    #[test]
    fn sanitize_id_blocks_traversal() {
        assert_eq!(sanitize_id("oskill_abc-123"), "oskill_abc-123");
        assert_eq!(sanitize_id("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_id("a/b\\c"), "abc");
    }
}
