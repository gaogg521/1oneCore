-- Security fix: the requirements board / milestones / test plans / pipelines
-- were built under the "v2 instance-per-enterprise model needs no tenant
-- column" assumption documented in 001_init.sql. Direction B later let one
-- server host multiple project groups (tenants) under one company, which
-- invalidated that assumption for these five tables — every list/update/
-- delete query here was, and until this migration still is, unscoped: any
-- authenticated org_admin on ANY tenant of a shared server could read and
-- write every OTHER tenant's requirements, milestones, test plans and
-- pipelines. Skills/MCP/RAG never had this problem — they carry their own
-- `scope`/`team_id`/`visibility` columns from day one (001_init.sql).
--
-- This file only adds the column + indexes. Backfilling existing rows from
-- `one_user_org` (one-org's table, not this crate's) can't happen inside this
-- migration script itself: this crate's own migration ledger is exercised in
-- isolation in unit tests (see `migrate.rs`'s `migrations_are_idempotent`),
-- against a bare pool that has none of one-org's tables — a cross-crate table
-- reference here would break that test even though it's always safe in the
-- real app (one-org's migrations run first, see `aionui-app/router/routes.rs`).
-- The backfill instead runs as `backfill_collaboration_tenant_ids` in
-- `migrate.rs`, gated on `one_user_org` actually existing, and only ever
-- touches rows still at the 'default' sentinel — so it can safely no-op on
-- every boot rather than needing its own one-shot ledger entry.
ALTER TABLE one_requirements ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE one_requirement_comments ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE one_milestones ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE one_test_plans ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE one_test_cases ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE one_pipelines ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE one_pipeline_runs ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';

CREATE INDEX IF NOT EXISTS idx_one_requirements_tenant ON one_requirements(tenant_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_requirement_comments_tenant ON one_requirement_comments(tenant_id);
CREATE INDEX IF NOT EXISTS idx_one_milestones_tenant ON one_milestones(tenant_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_test_plans_tenant ON one_test_plans(tenant_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_test_cases_tenant ON one_test_cases(tenant_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_pipelines_tenant ON one_pipelines(tenant_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_pipeline_runs_tenant ON one_pipeline_runs(tenant_id, created_at DESC);
