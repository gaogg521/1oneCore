-- T6 stage 3: departments mapped in from the company directory mirror
-- (one-enterprise's `one_directory_departments`), as opposed to a manual
-- department an admin typed in.
--
-- `source` is NULL for every existing row (zero behavior change for manual
-- trees) and `'directory'` for a row created by the mapping sync.
-- `directory_external_id` is only set alongside `'directory'` and is what a
-- re-sync matches against to update the same row instead of duplicating it.
-- The partial unique index enforces "one local department per directory node,
-- per project group" without constraining the (overwhelmingly common) NULL
-- case at all.
ALTER TABLE one_departments ADD COLUMN source TEXT;
ALTER TABLE one_departments ADD COLUMN directory_external_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_one_departments_directory_external
    ON one_departments(tenant_id, directory_external_id)
    WHERE directory_external_id IS NOT NULL;
