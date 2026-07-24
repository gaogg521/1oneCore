-- one-org 008: onboarding (P2-4) — domain-based auto-join + SMTP reservation
-- for invite emails.
--
-- `allowed_email_domains` on a tenant lets a new SSO login whose email domain
-- matches auto-join that project group without an invite code (admin opt-in;
-- NULL/empty = disabled, the default — existing tenants are unaffected).
ALTER TABLE one_tenants ADD COLUMN allowed_email_domains TEXT;

-- Singleton SMTP configuration for sending invite emails. Reserved: no SMTP
-- client is wired in yet (see `EmailSender` trait in one-org) — an operator
-- with real SMTP credentials fills this in, and `enabled` flips on once a real
-- `EmailSender` is wired in the app layer. Until then every send is a no-op
-- with a clear "not configured" result, mirroring the payment-provider stub.
CREATE TABLE IF NOT EXISTS one_smtp_config (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    host               TEXT,
    port               INTEGER,
    username           TEXT,
    -- Encrypted with the same `aionui_common::crypto` helper as other stored
    -- secrets (provider API keys, SSO client secrets).
    password_encrypted TEXT,
    from_address       TEXT,
    enabled            INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL
);
