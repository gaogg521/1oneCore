-- Pending company invites: an admin picks a person from the synced Feishu
-- directory and gets a shareable link/code to hand them.
--
-- Deliberately NOT an access gate — `sync_member` (SSO login) still
-- auto-joins ANY successful login to the sole manual company, unchanged.
-- This table exists purely so the admin can (a) see "invited, not yet
-- joined" people in the Members tab before they log in, and (b) hand out a
-- link pre-labelled with who it's for. A row here is consumed (deleted) the
-- moment that same (provider, external_id) actually completes SSO login.
CREATE TABLE IF NOT EXISTS one_enterprise_invites (
    id TEXT PRIMARY KEY,
    enterprise_id TEXT NOT NULL,
    -- Same identity space as one_enterprises/one_enterprise_members: the IdP
    -- provider and the invitee's opaque IdP id (Feishu union_id etc.).
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    -- Directory-sourced display fields, shown in the Members tab's pending
    -- row before the real SSO login (which will bring its own, authoritative
    -- copies) exists.
    display_name TEXT,
    department TEXT,
    job_title TEXT,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Re-inviting the same directory person replaces their pending invite rather
-- than piling up duplicates.
CREATE UNIQUE INDEX IF NOT EXISTS idx_one_enterprise_invites_person
    ON one_enterprise_invites(enterprise_id, provider, external_id);

CREATE INDEX IF NOT EXISTS idx_one_enterprise_invites_enterprise
    ON one_enterprise_invites(enterprise_id);
