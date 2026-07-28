-- Migration 035: expert marketplace catalog
--
-- A browsable persona catalog entirely separate from `assistant_definitions`.
-- Rows here are NOT real assistants and never affect a user's own list;
-- browsing/searching this table has zero interaction with `assistant_overlays`.
-- A user "installs" an entry via the existing persona-import path
-- (`AssistantService::import_personas`), which materializes exactly one real
-- `assistant_definitions` row (`source='imported'`) on demand.
--
-- Populated at startup from the embedded manifest
-- (`crates/aionui-app/assets/marketplace-personas/`) via
-- `materialize_marketplace_personas()` — see `aionui-assistant/src/marketplace.rs`.

CREATE TABLE IF NOT EXISTS assistant_marketplace_personas (
    id           TEXT PRIMARY KEY,
    source       TEXT NOT NULL DEFAULT 'workbuddy',
    name         TEXT NOT NULL,
    description  TEXT,
    rule_content TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_assistant_marketplace_personas_source
    ON assistant_marketplace_personas(source);
