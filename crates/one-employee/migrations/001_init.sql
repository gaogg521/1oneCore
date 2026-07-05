-- one-employee 001: digital employee definitions + structured run history.
-- Shares the `_one_migrations` ledger with one-org (names are prefixed
-- `employee_` to keep the key space disjoint). Mirrors the 1ONE ClaudeCode
-- `personal_agents` table; run history graduates from the legacy
-- automation_config JSON blob into a proper table.

CREATE TABLE IF NOT EXISTS one_personal_agents (
    id TEXT PRIMARY KEY,
    owner_user_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    description TEXT,
    agent_type TEXT NOT NULL,
    custom_agent_id TEXT,
    cli_path TEXT,
    automation_config TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_one_personal_agents_owner
    ON one_personal_agents(owner_user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_personal_agents_tenant
    ON one_personal_agents(tenant_id);

CREATE TABLE IF NOT EXISTS one_employee_runs (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    owner_user_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL DEFAULT 'default',
    team_id TEXT,
    slot_id TEXT,
    conversation_id TEXT NOT NULL,
    turn_id TEXT,
    status TEXT NOT NULL CHECK(status IN ('running', 'success', 'failed')),
    summary TEXT,
    error TEXT,
    trigger_source TEXT NOT NULL DEFAULT 'manual',
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_one_employee_runs_agent
    ON one_employee_runs(agent_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_employee_runs_conversation
    ON one_employee_runs(conversation_id);
