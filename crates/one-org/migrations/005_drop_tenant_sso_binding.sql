-- Decouple the SSO company from project-group tenants.
--
-- The 07-16 "real enterprise = SSO-bound tenant" (B2) model tagged a tenant
-- with its SSO company (`sso_provider`/`sso_org_id`) and auto-joined
-- same-company logins into it. That conflated two orthogonal concepts. The SSO
-- company now lives in its own domain (one-enterprise: `one_enterprises` /
-- `one_enterprise_members`), so `one_tenants` goes back to being a pure
-- invite-code project group with no SSO binding.
--
-- SQLite >= 3.35 (bundled libsqlite3-sys 0.30.x ships 3.46+) supports
-- ALTER TABLE ... DROP COLUMN. Existing tenants keep all their data; they just
-- lose the (now unused) binding columns. Tenants previously "SSO-bound" simply
-- become ordinary project groups.
DROP INDEX IF EXISTS idx_one_tenants_sso_org;
ALTER TABLE one_tenants DROP COLUMN sso_provider;
ALTER TABLE one_tenants DROP COLUMN sso_org_id;
