-- one-org 001: enterprise tenant core tables.
-- Managed by one-org's own migrator (_one_migrations), fully decoupled from
-- the upstream sqlx migrator (_sqlx_migrations) so upstream rebases never
-- conflict. Schema mirrors 1ONE ClaudeCode SQLite schema v53 (see
-- src/process/services/database/schema.ts in the 1one-command repo), with a
-- `one_` prefix and enterprise user attributes split out of the upstream
-- `users` table into `one_user_org`.

CREATE TABLE IF NOT EXISTS one_tenants (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    exit_password_hash TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS one_tenant_invites (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    code TEXT NOT NULL UNIQUE,
    created_by TEXT NOT NULL,
    max_uses INTEGER,
    use_count INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (tenant_id) REFERENCES one_tenants(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_one_tenant_invites_tenant
    ON one_tenant_invites(tenant_id, created_at DESC);

-- Enterprise attributes for upstream users. No FK to the upstream `users`
-- table on purpose: upstream owns that table and we never ALTER it.
CREATE TABLE IF NOT EXISTS one_user_org (
    user_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'member',
    org_unit_path TEXT,
    org_profile_source TEXT,
    org_profile_synced_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_user_org_tenant ON one_user_org(tenant_id);

CREATE TABLE IF NOT EXISTS one_runtime_nodes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    machine_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    hostnames TEXT NOT NULL DEFAULT '[]',
    ip_addresses TEXT NOT NULL DEFAULT '[]',
    installed_agents TEXT NOT NULL DEFAULT '[]',
    last_seen_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_runtime_nodes_tenant_seen
    ON one_runtime_nodes(tenant_id, last_seen_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_runtime_nodes_user
    ON one_runtime_nodes(tenant_id, user_id);

CREATE TABLE IF NOT EXISTS one_audit_logs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default',
    user_id TEXT,
    username TEXT,
    action TEXT NOT NULL,
    resource TEXT,
    ip_address TEXT,
    user_agent TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_audit_tenant
    ON one_audit_logs(tenant_id, created_at DESC);
