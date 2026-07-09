//! End-to-end HTTP test for enterprise team resource distribution.
//!
//! Drives the REAL router (auth middleware → one-devops registry → skills /
//! mcp team-sync → local listing) with real data, proving the full chain a
//! member's desktop client exercises:
//!   admin defines in registry → member syncs → materialized locally →
//!   surfaces in the local skill/MCP list (team source, auto-active, secrets)
//!   → authoritative empty resync reconciles it away.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app_with_skill_paths, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn team_skill_distribution_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    // create_router_with_states (used by the harness) skips the one-devops
    // migration that create_router runs, so bring up the registry tables.
    one_devops::run_one_devops_migrations(services.database.pool())
        .await
        .unwrap();
    let (token, csrf) = setup_and_login(&mut app, &services, "owner", "pw12345678").await;

    // 1. Admin defines an auto-active (mandatory) team skill in the registry.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/one/devops/skills",
            json!({
                "name": "weekly-report",
                "description": "Draft the weekly report",
                "content": "Write a concise weekly summary.",
                "autoActive": true
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "admin creates team skill");
    let created = body_json(resp).await;
    let skill_id = created["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["data"]["autoActive"], true);

    // 2. Member syncs the registry to local disk.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/team-sync",
            json!({
                "skills": [{
                    "id": skill_id,
                    "name": "weekly-report",
                    "description": "Draft the weekly report",
                    "content": "Write a concise weekly summary.",
                    "autoActive": true
                }],
                "authoritative": true
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "team-sync materializes");
    let report = body_json(resp).await;
    assert_eq!(report["data"]["written"].as_array().unwrap().len(), 1);

    // 3. It surfaces in the local skill list as a team, auto-active skill.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/skills", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let team = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "weekly-report")
        .expect("materialized team skill appears in /api/skills");
    assert_eq!(team["source"], "team", "tagged as team");
    assert_eq!(team["is_auto_inject"], true, "auto-active skill badged auto-inject");

    // 4. Authoritative empty resync (e.g. leaving the org) reconciles it away.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/team-sync",
            json!({ "skills": [], "authoritative": true }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/skills", &token))
        .await
        .unwrap();
    let list = body_json(resp).await;
    assert!(
        !list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "weekly-report"),
        "team skill removed after authoritative empty resync"
    );
}

#[tokio::test]
async fn team_mcp_distribution_with_secrets_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    // create_router_with_states (used by the harness) skips the one-devops
    // migration that create_router runs, so bring up the registry tables.
    one_devops::run_one_devops_migrations(services.database.pool())
        .await
        .unwrap();
    let (token, csrf) = setup_and_login(&mut app, &services, "owner", "pw12345678").await;

    // 1. Admin defines a credentialed team MCP connector.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/one/devops/mcp-registry",
            json!({
                "name": "team-search",
                "type": "sse",
                "endpoint": "https://mcp.corp/sse",
                "enabled": true,
                "hasKeys": true,
                "secretsJson": "{\"Authorization\":\"Bearer team-token\"}"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "admin creates team MCP");
    let created = body_json(resp).await;
    let mcp_id = created["data"]["id"].as_str().unwrap().to_owned();

    // 2. Member syncs it → materialized into the local MCP config with secrets.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/mcp/team-sync",
            json!({
                "servers": [{
                    "id": mcp_id,
                    "name": "team-search",
                    "type": "sse",
                    "endpoint": "https://mcp.corp/sse",
                    "enabled": true,
                    "secretsJson": "{\"Authorization\":\"Bearer team-token\"}"
                }],
                "authoritative": true
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "mcp team-sync materializes");
    let report = body_json(resp).await;
    assert_eq!(report["data"]["written"].as_array().unwrap().len(), 1);

    // 3. The local MCP server list carries the distributed credential.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/mcp/servers", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let server = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "team-search")
        .expect("materialized team MCP appears locally");
    assert_eq!(server["transport"]["type"], "sse");
    assert_eq!(
        server["transport"]["headers"]["Authorization"], "Bearer team-token",
        "distributed credential materialized into transport headers"
    );
}
