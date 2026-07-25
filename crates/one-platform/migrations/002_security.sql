-- P1-4 security/compliance config: IP allowlist + SIEM audit-log export.
-- Same reserved-adapter shape as 001 (per-tenant singleton, credential
-- encrypted, enabled defaults off).
--
-- IP allowlist: the CIDR ranges permitted to reach this project group's
-- server. The config + match logic are real, but request-blocking middleware is
-- NOT wired by default (it could lock out the operator) — a deployment enables
-- enforcement by dropping in the middleware. `cidrs` is a JSON array of strings.
CREATE TABLE IF NOT EXISTS one_ip_allowlist_config (
    tenant_id  TEXT    PRIMARY KEY NOT NULL,
    cidrs      TEXT,          -- JSON array of CIDR / IP strings
    enabled    INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

-- SIEM export: forward audit logs to an external SIEM (Splunk HEC / syslog /
-- generic HTTP). Reserved adapter — no exporter ships; an export reports
-- "not configured" until a real `SiemExporter` is wired at the app layer.
CREATE TABLE IF NOT EXISTS one_siem_config (
    tenant_id        TEXT    PRIMARY KEY NOT NULL,
    kind             TEXT,          -- 'splunk' | 'syslog' | 'http' | 'none'
    endpoint         TEXT,
    secret_encrypted TEXT,          -- HEC token / auth header (encrypted)
    enabled          INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL
);
