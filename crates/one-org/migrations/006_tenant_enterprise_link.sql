-- Link a project-group tenant to a company (真实企业 / one-enterprise).
--
-- Direction B: a company (one_enterprises row) is a governance tier ABOVE
-- project groups and may OWN several of them. This nullable FK records which
-- company a tenant belongs to. NULL = a standalone, invite-code-only project
-- group with no company owner (the "可独立可归属" decision) — every existing
-- tenant, and every tenant created via the untouched `org_create` path, keeps
-- enterprise_id NULL, so the personal / standalone edition is unaffected.
--
-- No FK constraint declared: one_enterprises lives in a sibling domain crate
-- (one-enterprise) and this column is resolved at the app layer; SQLite FKs
-- across independently-migrated tables would couple their apply order.
ALTER TABLE one_tenants ADD COLUMN enterprise_id TEXT;

CREATE INDEX IF NOT EXISTS idx_one_tenants_enterprise ON one_tenants(enterprise_id);
