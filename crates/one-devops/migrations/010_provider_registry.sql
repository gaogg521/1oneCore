-- one-devops 010: company-provisioned model channels (T2).
--
-- Until now the three registries distributed skills, MCP connectors and
-- knowledge — but never model credentials, so every employee had to obtain and
-- paste their own API key. For image and video models that is both expensive
-- and the thing enterprise procurement objects to hardest: keys handed out are
-- keys lost, and a leaver keeps theirs.
--
-- Unlike `one_mcp_registry` (008), the credential is NOT distributed. It stays
-- encrypted here and is only ever decrypted server-side by the model proxy.
-- Members receive a channel token instead, which is revocable and identifies
-- them at the proxy. That deliberately trades the offline-first property 008
-- chose for MCP: a company model channel needs the server to be reachable.
--
-- Scope / team_id / visibility mirror 009 exactly, so the existing
-- `member_visibility_where()` filter and `validate_resource_scope()` apply
-- unchanged: enterprise-wide by default, overridable per project group.

CREATE TABLE IF NOT EXISTS one_provider_registry (
    id                 TEXT    PRIMARY KEY NOT NULL,
    name               TEXT    NOT NULL,
    -- Platform id as understood by the desktop model catalogue (e.g. 'openai',
    -- 'dashscope'). Carried through to the materialized provider row so the
    -- client renders and routes it like any other.
    platform           TEXT    NOT NULL DEFAULT 'openai',
    -- Where the proxy forwards to. Stored exactly as an admin would type a
    -- provider base_url, because the proxy is path-preserving.
    upstream_base_url  TEXT    NOT NULL,
    -- The real credential, encrypted with the deployment's data secret. Never
    -- returned by any endpoint and never distributed.
    api_key_encrypted  TEXT    NOT NULL DEFAULT '',
    -- JSON array of model names offered on this channel.
    models             TEXT    NOT NULL DEFAULT '[]',
    -- JSON object of per-model settings (model_kind / media_endpoint /
    -- media_unit_price_usd), same shape as `providers.model_settings`.
    model_settings     TEXT,
    enabled            INTEGER NOT NULL DEFAULT 1,
    scope              TEXT    NOT NULL DEFAULT 'org',
    team_id            TEXT,
    visibility         TEXT    NOT NULL DEFAULT 'all',
    created_by         TEXT    NOT NULL,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_provider_registry_scope ON one_provider_registry(scope, team_id);

-- One long-lived, revocable token per (member, channel).
--
-- Deliberately not the session JWT: that rotates whenever membership changes
-- (one-org rotates it to kill a removed member's sessions), which would break
-- every provisioned channel until the next sync. A dedicated token is
-- decoupled from that, revocable per member on offboarding, and gives the
-- proxy per-member attribution — which is what a content audit will need.
--
-- Only the hash is stored: a leaked database must not yield working tokens.
CREATE TABLE IF NOT EXISTS one_provider_channel_tokens (
    token_hash TEXT    PRIMARY KEY NOT NULL,
    user_id    TEXT    NOT NULL,
    channel_id TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    last_used  INTEGER,
    revoked_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_one_provider_channel_tokens_user ON one_provider_channel_tokens(user_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_one_provider_channel_tokens_pair
    ON one_provider_channel_tokens(user_id, channel_id);
