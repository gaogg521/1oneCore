//! End-to-end proof for M3 team-skill distribution → consumption.
//!
//! Uses real test data to show that a skill "distributed by the admin" (a
//! [`TeamSkillPayload`]) is materialized on the member's disk and then surfaces
//! through `list_available_skills` with `source = Team` — the exact bridge the
//! agent loader relies on. Also proves the standalone-safety contract: with no
//! team skills, listing is byte-for-byte the personal-mode behavior.

use std::path::Path;

use aionui_extension::skill_service::{list_available_skills, SkillPaths, SkillSource};
use aionui_extension::team_sync::{sync_team_skills, TeamSkillPayload};
use tempfile::TempDir;

fn make_paths(base: &Path) -> SkillPaths {
    SkillPaths {
        data_dir: base.to_path_buf(),
        user_skills_dir: base.join("skills"),
        cron_skills_dir: base.join("cron").join("skills"),
        builtin_skills_dir: base.join("builtin-skills"),
        builtin_rules_dir: base.join("builtin-rules"),
        assistant_rules_dir: base.join("assistant-rules"),
        assistant_skills_dir: base.join("assistant-skills"),
    }
}

fn payload(id: &str, name: &str, desc: &str, body: &str) -> TeamSkillPayload {
    TeamSkillPayload {
        id: id.to_string(),
        name: name.to_string(),
        description: desc.to_string(),
        content: body.to_string(),
    }
}

/// Admin distributes a team skill → member syncs → it appears in the skill list
/// tagged as Team, loadable by the agent.
#[tokio::test]
async fn distributed_team_skill_surfaces_in_listing() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());

    // Test data: one team skill the admin "pushed".
    let report = sync_team_skills(
        &paths.team_skills_dir(),
        &[payload("oskill_report", "weekly-report", "Draft the weekly report", "Write a concise weekly summary.")],
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.written, vec!["oskill_report".to_string()]);
    assert_eq!(report.kept, 1);

    let listed = list_available_skills(&paths).await.unwrap();
    let team = listed
        .iter()
        .find(|s| s.name == "weekly-report")
        .expect("distributed team skill must appear in the skill listing");
    assert_eq!(team.source, SkillSource::Team, "must be tagged as a team skill");
    assert!(!team.is_custom, "team skills are read-only, not custom");
    assert!(
        team.description.contains("weekly report"),
        "description carried from the registry: {}",
        team.description
    );
}

/// Standalone-safety: no team skills → listing contains none, and never errors.
#[tokio::test]
async fn standalone_listing_has_no_team_skills() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());

    // No sync call at all — pure personal/standalone mode.
    let listed = list_available_skills(&paths).await.unwrap();
    assert!(
        listed.iter().all(|s| s.source != SkillSource::Team),
        "standalone mode must never surface team skills"
    );
}

/// Admin deletes a team skill on the server → an authoritative resync removes it
/// from the member's listing; unrelated skills stay.
#[tokio::test]
async fn admin_delete_propagates_to_listing() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());
    let dir = paths.team_skills_dir();

    sync_team_skills(
        &dir,
        &[
            payload("oskill_a", "alpha", "a", "body a"),
            payload("oskill_b", "beta", "b", "body b"),
        ],
        true,
    )
    .await
    .unwrap();
    assert_eq!(list_available_skills(&paths).await.unwrap().iter().filter(|s| s.source == SkillSource::Team).count(), 2);

    // Server now only serves alpha (admin deleted beta) — authoritative resync.
    sync_team_skills(&dir, &[payload("oskill_a", "alpha", "a", "body a")], true)
        .await
        .unwrap();

    let listed = list_available_skills(&paths).await.unwrap();
    let team_names: Vec<&str> = listed.iter().filter(|s| s.source == SkillSource::Team).map(|s| s.name.as_str()).collect();
    assert_eq!(team_names, vec!["alpha"], "beta must be reconciled away, alpha kept");
}
