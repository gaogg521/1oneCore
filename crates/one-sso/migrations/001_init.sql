-- one-sso 001: SSO provider configs + external identity bindings.
-- Shares the `_one_migrations` ledger with one-org/one-employee; entry names
-- carry the `sso_` prefix to keep the key space disjoint. Mirrors the 1ONE
-- ClaudeCode `auth_providers` + `auth_identities` tables (see
-- src/process/webserver/auth/repository/ in the 1one-command repo).

CREATE TABLE IF NOT EXISTS one_sso_providers (
    provider TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0,
    config TEXT NOT NULL DEFAULT '{}',
    updated_at INTEGER NOT NULL,
    updated_by TEXT
);

CREATE TABLE IF NOT EXISTS one_sso_identities (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL DEFAULT 'default',
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE(provider, external_id)
);
CREATE INDEX IF NOT EXISTS idx_one_sso_identities_user
    ON one_sso_identities(user_id);
CREATE INDEX IF NOT EXISTS idx_one_sso_identities_provider
    ON one_sso_identities(provider, external_id);
