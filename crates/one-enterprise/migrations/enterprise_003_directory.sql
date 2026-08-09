-- T6: a mirror of the company's IdP directory (Feishu Contacts today).
--
-- Deliberately SEPARATE from one_enterprise_members. That table is the *seat*
-- table — `SELECT COUNT(*)` over it is literally how billing bills
-- (one-billing/src/service.rs) — and it is keyed by local `user_id`, so it can
-- only ever hold people who have logged in at least once. A directory has
-- neither property: a 1000-person company has 1000 directory rows and maybe 30
-- people who have actually signed in, and mirroring the directory into the seat
-- table would bill for all 1000 the moment sync first ran.
--
-- So: this is what the IdP says exists. one_enterprise_members stays what it
-- always was — who bound a local account and holds a licensed seat. The join
-- between them is one_sso_identities (user_id ↔ provider + external_id).

-- One node of the company's org tree, as the IdP sees it.
CREATE TABLE IF NOT EXISTS one_directory_departments (
    enterprise_id      TEXT    NOT NULL,
    -- The IdP's own id (Feishu open_department_id). Not our short_id: it has to
    -- survive across syncs and be comparable with what the IdP sends next time.
    external_id        TEXT    NOT NULL,
    -- NULL for a top-level department. Feishu's root sentinel ("0") is
    -- normalized away before it gets here.
    parent_external_id TEXT,
    name               TEXT    NOT NULL,
    first_seen_at      INTEGER NOT NULL,
    last_seen_at       INTEGER NOT NULL,
    PRIMARY KEY (enterprise_id, external_id)
);
CREATE INDEX IF NOT EXISTS idx_one_directory_departments_parent
    ON one_directory_departments(enterprise_id, parent_external_id);

-- One person in the directory. NOT a member, NOT a seat, NOT necessarily
-- someone with an account here.
CREATE TABLE IF NOT EXISTS one_directory_people (
    enterprise_id       TEXT    NOT NULL,
    -- Matches one_sso_identities.external_id for the same provider, which is
    -- what lets a directory row be tied back to a local account. Whether it
    -- holds an open_id or a union_id follows the provider's external_id_field
    -- config — if an admin changes that, ids stop lining up, which is why the
    -- sync stores the field it used in one_directory_sync_state.
    external_id         TEXT    NOT NULL,
    name                TEXT,
    job_title           TEXT,
    -- Primary department. A person can be in several; the rest are not modelled
    -- until something needs them (T7 department budgets probably will).
    department_external_id TEXT,
    -- 0 when the IdP flags them as resigned. Feishu keeps leavers in the
    -- directory rather than deleting them, so "still listed" is not "still
    -- here" — see one-sso/src/providers/feishu.rs.
    active              INTEGER NOT NULL DEFAULT 1,
    first_seen_at       INTEGER NOT NULL,
    last_seen_at        INTEGER NOT NULL,
    -- When this person stopped appearing in a COMPLETE pull, or was flagged
    -- resigned. NULL = present. This is the only column an offboarding
    -- suggestion is derived from, and nothing may set it from a partial pull.
    missing_since       INTEGER,
    PRIMARY KEY (enterprise_id, external_id)
);
CREATE INDEX IF NOT EXISTS idx_one_directory_people_missing
    ON one_directory_people(enterprise_id, missing_since);

-- Last run per company, for the admin console's status line and so a failed
-- sync is visible rather than silent.
CREATE TABLE IF NOT EXISTS one_directory_sync_state (
    enterprise_id     TEXT    PRIMARY KEY NOT NULL,
    provider          TEXT    NOT NULL,
    -- Which id field the last successful pull keyed people by. Recorded because
    -- changing it invalidates every stored external_id.
    external_id_field TEXT,
    last_run_at       INTEGER,
    -- 'ok' | 'partial' | 'error'. 'partial' means the pull did not complete, so
    -- the mirror was written but no departure conclusions were drawn from it.
    last_status       TEXT,
    last_error        TEXT,
    department_count  INTEGER NOT NULL DEFAULT 0,
    people_count      INTEGER NOT NULL DEFAULT 0,
    updated_at        INTEGER NOT NULL
);
