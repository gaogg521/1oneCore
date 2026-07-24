-- one-billing: the commercialization "billing plane" — subscription tier,
-- seat cap, and per-turn usage metering. Deliberately separate from
-- one-enterprise (identity) so licensing/usage can evolve independently.
--
-- License attaches to an SSO company (one-enterprise's `one_enterprises`).
-- Personal / standalone users have no enterprise and are NOT in this system.

CREATE TABLE IF NOT EXISTS one_enterprise_license (
    enterprise_id TEXT PRIMARY KEY,
    -- 'free' | 'team' | 'enterprise'. New companies default 'free' (set on
    -- create/sync); existing companies are grandfathered below.
    tier          TEXT    NOT NULL DEFAULT 'free',
    -- Explicit seat override; NULL = use the tier's default cap.
    seat_limit    INTEGER,
    -- License expiry (ms); NULL = no expiry. Informational for now.
    expires_at    INTEGER,
    updated_at    INTEGER NOT NULL
);

-- One row per metered turn. `enterprise_id` NULL for personal users (kept
-- locally but never shown in a company dashboard). Token columns are
-- best-effort (sourced from ACP context usage); NULL when unavailable.
CREATE TABLE IF NOT EXISTS one_usage_events (
    id                    TEXT    PRIMARY KEY,
    user_id               TEXT    NOT NULL,
    enterprise_id         TEXT,
    conversation_id       TEXT,
    model                 TEXT,
    input_tokens          INTEGER,
    output_tokens         INTEGER,
    total_tokens          INTEGER,
    estimated_cost_micros INTEGER,
    created_at            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_usage_events_ent  ON one_usage_events(enterprise_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_usage_events_user ON one_usage_events(user_id, created_at DESC);

-- Grandfather every company that already exists to the top tier, so a
-- pre-billing enterprise keeps all the features it was already using. New
-- companies created after this migration default to 'free'. Runs after the
-- one-enterprise migrations (ordering enforced in aionui-app).
INSERT OR IGNORE INTO one_enterprise_license (enterprise_id, tier, updated_at)
    SELECT id, 'enterprise', CAST(strftime('%s', 'now') AS INTEGER) * 1000
    FROM one_enterprises;
