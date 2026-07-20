-- one-enterprise: the SSO-company / "enterprise org" dimension.
--
-- Deliberately SEPARATE from one-org's project-group tenants: a person's SSO
-- company (Feishu tenant_key etc.) and their invite-code project groups are
-- two orthogonal concepts. Keeping enterprise membership in its own tables
-- means future enterprise-scoped policy / skills / MCP / permissions /
-- knowledge-base dispatch can target an `enterprise_id` without tangling with
-- `one_tenants` / `one_user_org`.

CREATE TABLE IF NOT EXISTS one_enterprises (
    id TEXT PRIMARY KEY,
    -- SSO provider this company came from (feishu / dingtalk / wecom / ...).
    provider TEXT NOT NULL,
    -- IdP company identifier (Feishu tenant_key etc.). Opaque id, not a name.
    external_id TEXT NOT NULL,
    -- Human-readable company name. Often NULL: Feishu SSO does not surface it
    -- (only the tenant_key); populated later if a directory API is available.
    display_name TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- One row per (provider, company): same-company logins converge here.
CREATE UNIQUE INDEX IF NOT EXISTS idx_one_enterprises_provider_ext
    ON one_enterprises(provider, external_id);

CREATE TABLE IF NOT EXISTS one_enterprise_members (
    -- One enterprise per user (their SSO company). PK enforces it.
    user_id TEXT PRIMARY KEY,
    enterprise_id TEXT NOT NULL,
    -- The member's own real name from the IdP (e.g. Feishu's 赵高). Stored here
    -- so the enterprise identity is self-contained (no cross-read into one-sso).
    display_name TEXT,
    -- Department path / job title, from the IdP directory. Only populated when
    -- the SSO grant includes a directory scope (Feishu Contacts); NULL else.
    department TEXT,
    job_title TEXT,
    role TEXT NOT NULL DEFAULT 'member',
    joined_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_enterprise_members_enterprise
    ON one_enterprise_members(enterprise_id);
