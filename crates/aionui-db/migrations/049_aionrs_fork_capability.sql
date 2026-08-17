-- Constructed fork capability for the builtin aionrs agent (Aion CLI).
--
-- Upstream ships this as migration 038; this fork renumbered it to 049 because
-- 038 is already taken here by `038_provider_model_settings.sql`. It pairs with
-- `048_conversation_fork.sql` (upstream 036).
--
-- aionrs is the in-process SDK integration path, never performs an ACP
-- handshake, so its `agent_metadata.agent_capabilities` column is only ever
-- written constructively (see also 015, which seeds its mode catalog the same
-- way). Note this is the ONE builtin where upstream's "nothing will ever
-- overwrite a constructed value" rationale still holds on this fork — the
-- claude/codex seeds were dropped from 048 precisely because `route_for_backend`
-- sends them through the ACP handshake here, which replaces the column.
--
-- Capability shape, verified against the aionrs session store:
--   * aionrs sessions are whole-file JSON state keyed by conversation id
--     (the aionui-ai-agent factory loads `SessionManager::load(<conversation_id>)`
--     from `<data_dir>/aionrs-sessions`); `SessionManager::fork_from` copies the
--     parent session's history into a new session id, optionally cut at a turn
--     anchor (`ForkBoundary::AtTurn`).
--     verified: aionrs-local/crates/aion-agent/src/session.rs:57 (ForkBoundary),
--     :141 (fork), :156 (fork_from)
--   * At-turn forks are supported ("at_turn": true): the aionrs manager emits
--     `BackendTurnBound` with the conversation-layer turn id at every turn
--     start, the stream persistence layer stamps it onto that turn's message
--     rows, and the aionrs engine stamps the same id onto its session messages
--     — one anchor for both history stores. Rows written before this wiring
--     existed stay NULL: they cannot anchor a mid-history fork and such
--     requests are rejected explicitly (FORK_POINT_UNSUPPORTED), never
--     silently degraded to a HEAD fork.
--
-- `json_patch` merges into any pre-existing JSON instead of clobbering the
-- column. The row is addressed by its stable seed id (001), never by display
-- fields.

UPDATE agent_metadata SET
    agent_capabilities = json_patch(
        COALESCE(agent_capabilities, '{}'),
        '{"session_capabilities":{"fork":{"at_turn":true}}}'
    ),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '632f31d2';
