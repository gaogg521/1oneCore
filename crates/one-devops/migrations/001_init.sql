-- one-devops 001: requirements board + collaboration registries.
--
-- Rebuild of the 1ONE ClaudeCode DevOps slice consumed by the
-- superAssistant IssuesWorkbench + EnterpriseCollaboration panels.
-- Field vocabulary mirrors the 1one reference (RequirementType/Status/
-- Priority, SkillRecord, McpRegistryRecord, RagDocumentRecord). The v2
-- instance-per-enterprise model needs no tenant column.

CREATE TABLE IF NOT EXISTS one_requirements (
    id           TEXT    PRIMARY KEY NOT NULL,
    parent_id    TEXT,
    type         TEXT    NOT NULL DEFAULT 'task'
                         CHECK(type IN ('epic', 'feature', 'story', 'bug', 'task')),
    subject      TEXT    NOT NULL,
    description  TEXT,
    status       TEXT    NOT NULL DEFAULT 'backlog'
                         CHECK(status IN ('backlog', 'planning', 'developing', 'testing', 'completed')),
    priority     TEXT    NOT NULL DEFAULT 'medium'
                         CHECK(priority IN ('low', 'medium', 'high', 'urgent')),
    assigned_to  TEXT,
    milestone_id TEXT,
    creator_id   TEXT    NOT NULL,
    creator_name TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_requirements_parent ON one_requirements(parent_id);
CREATE INDEX IF NOT EXISTS idx_one_requirements_status ON one_requirements(status, updated_at DESC);

CREATE TABLE IF NOT EXISTS one_requirement_comments (
    id             TEXT    PRIMARY KEY NOT NULL,
    requirement_id TEXT    NOT NULL,
    author_type    TEXT    NOT NULL DEFAULT 'user'
                           CHECK(author_type IN ('user', 'agent', 'autopilot')),
    author_id      TEXT,
    author_name    TEXT    NOT NULL,
    body           TEXT    NOT NULL,
    metadata       TEXT,
    created_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_requirement_comments_req
    ON one_requirement_comments(requirement_id, created_at);

CREATE TABLE IF NOT EXISTS one_skill_registry (
    id          TEXT    PRIMARY KEY NOT NULL,
    name        TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    content     TEXT    NOT NULL DEFAULT '',
    enabled     INTEGER NOT NULL DEFAULT 1,
    scope       TEXT    NOT NULL DEFAULT 'org',
    team_id     TEXT,
    created_by  TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS one_mcp_registry (
    id         TEXT    PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL,
    type       TEXT    NOT NULL DEFAULT 'stdio' CHECK(type IN ('stdio', 'sse')),
    endpoint   TEXT    NOT NULL DEFAULT '',
    enabled    INTEGER NOT NULL DEFAULT 1,
    has_keys   INTEGER NOT NULL DEFAULT 0,
    scope      TEXT    NOT NULL DEFAULT 'org',
    team_id    TEXT,
    created_by TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS one_rag_documents (
    id          TEXT    PRIMARY KEY NOT NULL,
    title       TEXT    NOT NULL,
    file_path   TEXT,
    file_size   INTEGER,
    mime_type   TEXT,
    status      TEXT    NOT NULL DEFAULT 'pending',
    last_error  TEXT,
    chunk_count INTEGER NOT NULL DEFAULT 0,
    scope       TEXT    NOT NULL DEFAULT 'org',
    team_id     TEXT,
    created_by  TEXT    NOT NULL,
    created_at  INTEGER NOT NULL
);
