-- T8: a consolidated, searchable ledger of every generated media asset —
-- distinct from `one_usage_events`, which records cost/attribution only and
-- deliberately carries neither a file path nor a prompt (see `MediaUsage`).
--
-- One row per generated FILE (not per job — a job producing 4 images writes 4
-- rows), so each artifact is individually findable. Enterprise-scoped only:
-- personal/no-company users are never recorded here, same red line every
-- other governance surface in this crate honors.
CREATE TABLE IF NOT EXISTS one_media_assets (
    id               TEXT PRIMARY KEY,
    user_id          TEXT NOT NULL,
    enterprise_id    TEXT NOT NULL,
    department_id    TEXT,
    conversation_id  TEXT,
    kind             TEXT NOT NULL,
    model            TEXT,
    file_path        TEXT NOT NULL,
    -- NULL unless the company has opted into prompt retention
    -- (`one_media_ledger_settings.retain_prompts`). The server enforces this,
    -- never the caller: a client always reports the true prompt, and this
    -- column silently stays NULL when the company has not opted in.
    prompt           TEXT,
    created_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_media_assets_enterprise ON one_media_assets(enterprise_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_media_assets_user ON one_media_assets(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_media_assets_conversation ON one_media_assets(conversation_id);

-- One row per company that has ever touched the setting. Absence = the
-- default (prompts not retained) — same "no row = ungoverned/default" idiom
-- as `one_department_budgets`.
CREATE TABLE IF NOT EXISTS one_media_ledger_settings (
    enterprise_id    TEXT PRIMARY KEY,
    retain_prompts   INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL
);
