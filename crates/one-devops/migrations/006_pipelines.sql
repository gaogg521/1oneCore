-- CI/CD-style pipelines with run history.
-- Trigger values: manual | push | schedule.
-- Run status mirrors standard CI conventions.
CREATE TABLE IF NOT EXISTS one_pipelines (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'disabled')),
    trigger TEXT NOT NULL DEFAULT 'manual'
        CHECK (trigger IN ('manual', 'push', 'schedule')),
    creator_id TEXT NOT NULL,
    creator_name TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_pipelines_status ON one_pipelines(status, updated_at DESC);

CREATE TABLE IF NOT EXISTS one_pipeline_runs (
    id TEXT PRIMARY KEY,
    pipeline_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'success', 'failed', 'cancelled')),
    triggered_by TEXT,
    started_at INTEGER,
    finished_at INTEGER,
    log TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_pipeline_runs_pipeline ON one_pipeline_runs(pipeline_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_pipeline_runs_status ON one_pipeline_runs(status);
