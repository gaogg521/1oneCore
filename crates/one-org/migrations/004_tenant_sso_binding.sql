-- Bind a tenant to the SSO company it represents, so an SSO login whose
-- company identifier (Feishu tenant_key / DingTalk corp_id / WeCom corpid)
-- matches can auto-join without an invite code (the "real enterprise" tier).
-- A tenant with a non-null sso_org_id is an SSO-company enterprise; a tenant
-- with a null sso_org_id is an invite-code project group. The two are mutually
-- exclusive per the "one server = one tenant" (D3) rule.
ALTER TABLE one_tenants ADD COLUMN sso_provider TEXT;
ALTER TABLE one_tenants ADD COLUMN sso_org_id TEXT;

-- At most one tenant per (provider, company) so a second employee of the same
-- company resolves to the existing enterprise rather than creating a duplicate.
CREATE UNIQUE INDEX IF NOT EXISTS idx_one_tenants_sso_org
    ON one_tenants (sso_provider, sso_org_id)
    WHERE sso_org_id IS NOT NULL;
