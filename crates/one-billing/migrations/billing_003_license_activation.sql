-- billing_003: offline license-key activation.
--
-- Before this, `tier` was a plain column any customer admin could PUT, so a
-- deployment could grant itself the top tier. Tier now comes from a
-- vendor-signed license key; this table records which key produced the current
-- entitlement so an operator can see (and support can audit) what is active.
--
-- Idempotency: `license_id` is the key's `lid` claim and is UNIQUE, so
-- re-pasting the same key is a no-op rather than a duplicate row.

CREATE TABLE IF NOT EXISTS one_license_activation (
    license_id    TEXT    PRIMARY KEY NOT NULL,
    enterprise_id TEXT    NOT NULL,
    customer      TEXT    NOT NULL,
    tier          TEXT    NOT NULL,
    seats         INTEGER,
    expires_at    INTEGER,
    issued_at     INTEGER NOT NULL,
    activated_at  INTEGER NOT NULL,
    activated_by  TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_license_activation_enterprise
    ON one_license_activation(enterprise_id, activated_at DESC);
