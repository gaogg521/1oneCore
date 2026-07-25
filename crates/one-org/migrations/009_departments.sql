-- one-org 009: organizational hierarchy (P2-3) — sub-teams / departments
-- within a project group. Distinct from `one_user_org.org_unit_path` (a
-- free-text SSO-synced department string, e.g. Feishu's contact-book path):
-- this is a structured, admin-managed tree the operator builds by hand.
CREATE TABLE IF NOT EXISTS one_departments (
    id         TEXT    PRIMARY KEY NOT NULL,
    tenant_id  TEXT    NOT NULL,
    parent_id  TEXT,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (parent_id) REFERENCES one_departments(id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_one_departments_tenant ON one_departments(tenant_id);
CREATE INDEX IF NOT EXISTS idx_one_departments_parent ON one_departments(parent_id);

-- A member's assigned department (nullable = unassigned, the default for
-- every existing row — zero behavior change).
ALTER TABLE one_user_org ADD COLUMN department_id TEXT;
