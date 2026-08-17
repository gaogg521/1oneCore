use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

/// Migration 049 (upstream 038; renumbered — 038 is taken on this fork): the
/// builtin aionrs agent (Aion CLI, seed id `632f31d2`) carries a constructed
/// at-turn fork capability. Turn anchors are stamped by the aionrs manager +
/// engine, so a mid-history fork can resolve a cut point.
///
/// aionrs is the only builtin seeded this way here: upstream also seeds
/// claude/codex in its 036, but this fork routes those through the ACP
/// handshake (`route_for_backend` → `AcpManager`), which replaces
/// `agent_capabilities` wholesale — see `048_conversation_fork.sql`.
#[tokio::test]
async fn aionrs_builtin_agent_declares_at_turn_fork_capability() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let aionrs = repo.get("632f31d2").await.unwrap().expect("seeded Aion CLI row");
    assert_eq!(aionrs.agent_type, "aionrs");
    assert_eq!(aionrs.backend, None, "aionrs resolves by agent_type, not backend");

    let capabilities: serde_json::Value =
        serde_json::from_str(aionrs.agent_capabilities.as_deref().expect("constructed capabilities")).unwrap();
    assert_eq!(
        capabilities["session_capabilities"]["fork"],
        serde_json::json!({"at_turn": true}),
        "at-turn fork: anchors are stamped on aionrs rows and session messages"
    );
}
