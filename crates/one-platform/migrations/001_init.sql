-- one-platform reserved infrastructure config (P1-3 containerization + P2-2
-- realtime collaboration). Both are per-project-group singletons keyed by
-- tenant_id, admin-managed, with any credential encrypted at rest.
--
-- "Reserved adapter" pattern (mirrors one_smtp_config / one_integrations):
-- storing a row does NOT run anything — no container runtime or collaboration
-- backend is wired in. It lets an admin fill in config ahead of time so a real
-- `ContainerRuntime` / `CollaborationProvider` can be dropped in at the app
-- layer later without a schema change. `enabled` defaults off so a half-filled
-- config stays inert.

-- P1-3: where a project group's agents/workspaces run (Docker / K8s / ...).
CREATE TABLE IF NOT EXISTS one_container_config (
    tenant_id                 TEXT    PRIMARY KEY NOT NULL,
    runtime_kind              TEXT,          -- 'docker' | 'kubernetes' | 'none'
    endpoint                  TEXT,          -- docker socket / k8s api server
    default_image             TEXT,          -- image agents run in
    registry                  TEXT,          -- image registry host
    registry_secret_encrypted TEXT,          -- registry pull credential (encrypted)
    enabled                   INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL
);

-- P2-2: the realtime collaboration backend (presence + shared sessions).
CREATE TABLE IF NOT EXISTS one_collaboration_config (
    tenant_id        TEXT    PRIMARY KEY NOT NULL,
    provider         TEXT,          -- 'builtin' | 'external' | 'none'
    endpoint         TEXT,          -- external relay / CRDT sync endpoint
    secret_encrypted TEXT,          -- auth token for the relay (encrypted)
    presence         INTEGER NOT NULL DEFAULT 0,  -- broadcast presence/cursors
    enabled          INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL
);
