-- P2-1 integration connectors (reserved framework): per-(tenant, provider)
-- external-system connector config (GitHub / GitLab / Jira / Feishu, ...).
--
-- This is the "reserved adapter" pattern (mirrors one_smtp_config / the billing
-- payment provider): storing a row here does NOT perform any real sync — no
-- connector client is wired in. It lets an org admin fill in credentials ahead
-- of time so a real `IntegrationProvider` can be dropped in at the app layer
-- later without a schema change.
--
-- `config_json` holds NON-secret provider-specific fields (org / project /
-- repo / board id, ...); the credential (token / API key) lives encrypted in
-- `secret_encrypted` (same at-rest encryption as the SMTP password), never
-- returned to the client. `enabled` defaults off so a half-filled connector is
-- inert until the admin explicitly turns it on.
CREATE TABLE IF NOT EXISTS one_integrations (
    tenant_id        TEXT    NOT NULL,
    provider         TEXT    NOT NULL,
    base_url         TEXT,
    config_json      TEXT,
    secret_encrypted TEXT,
    enabled          INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, provider)
);

CREATE INDEX IF NOT EXISTS idx_one_integrations_tenant ON one_integrations(tenant_id);
