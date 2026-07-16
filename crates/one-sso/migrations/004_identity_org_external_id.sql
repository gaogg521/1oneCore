-- Persist the IdP's company/organization identifier (Feishu `tenant_key`,
-- DingTalk `corp_id`, WeCom `corpid`) alongside the SSO identity.
--
-- Needed so the enterprise tenant can be bound to the company its creator
-- belongs to, and so a later same-company SSO login can be matched against
-- that binding and auto-join without an invite code. NULL for LDAP/local
-- logins and providers that don't surface a company id.
ALTER TABLE one_sso_identities ADD COLUMN org_external_id TEXT;

CREATE INDEX IF NOT EXISTS idx_one_sso_identities_org
    ON one_sso_identities (provider, org_external_id);
