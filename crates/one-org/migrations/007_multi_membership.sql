-- one-org 007: multi-membership (一人多项目组) + per-user active tenant.
--
-- Phase 1 (企业三层) sidestepped the `one_user_org` PK = `user_id` constraint
-- (one user can only be in one project group) by "creating empty groups on
-- behalf of the company" without ever touching that PK. Phase 2 opens it up:
-- a real company member necessarily spans several project groups, so the PK
-- becomes `(user_id, tenant_id)` and each user carries an *active tenant*
-- (the project group currently in effect). `tenant_of(user_id)` /
-- `effective_role(user_id)` keep their signatures — they now resolve through
-- the active tenant instead of the single membership row — so the RBAC
-- extractors (OrgActor) and the team-resource TenantResolver need no change.
--
-- Personal / standalone edition is unaffected: it has no membership rows, so
-- `one_active_tenant` stays empty and every resolver falls back to the
-- DEFAULT_TENANT_ID / system_default_user path exactly as before.
--
-- First table-rebuild migration in one-org. SQLite ≥ 3.35 (already relied on
-- by earlier ALTER ADD/DROP COLUMN migrations) is assumed. Wrapped by the
-- migrator in a single transaction; foreign_keys is OFF during startup
-- migration so the rebuild does not trip the one_tenant_invites FK.

-- 1) Rebuild one_user_org with a composite primary key (user_id, tenant_id).
--    Columns are copied verbatim from the 001+002+003 shape.
CREATE TABLE one_user_org_new (
    user_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'member',
    org_unit_path TEXT,
    org_profile_source TEXT,
    org_profile_synced_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    display_name TEXT,
    job_title TEXT,
    PRIMARY KEY (user_id, tenant_id)
);

INSERT INTO one_user_org_new (
    user_id, tenant_id, role, org_unit_path, org_profile_source,
    org_profile_synced_at, created_at, updated_at, display_name, job_title
)
SELECT
    user_id, tenant_id, role, org_unit_path, org_profile_source,
    org_profile_synced_at, created_at, updated_at, display_name, job_title
FROM one_user_org;

DROP TABLE one_user_org;
ALTER TABLE one_user_org_new RENAME TO one_user_org;

CREATE INDEX IF NOT EXISTS idx_one_user_org_tenant ON one_user_org(tenant_id);

-- 2) Per-user active tenant (which project group a user is currently acting in).
CREATE TABLE IF NOT EXISTS one_active_tenant (
    user_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- 3) Backfill: every existing single membership becomes that user's active
--    tenant, so behaviour is byte-identical until a user joins a second group.
INSERT OR IGNORE INTO one_active_tenant (user_id, tenant_id, updated_at)
SELECT user_id, tenant_id, updated_at FROM one_user_org;
