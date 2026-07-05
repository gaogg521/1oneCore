-- Milestones: group requirements by a delivery target. one_requirements
-- already carries a milestone_id column (added in 001) that referenced
-- nothing; this table gives it a home. No FK constraint (SQLite soft link,
-- matching the rest of the one-* schema) — deleting a milestone leaves
-- requirements with a dangling milestone_id, cleared by the service layer.
CREATE TABLE IF NOT EXISTS one_milestones (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'completed', 'archived')),
    due_at INTEGER,
    creator_id TEXT NOT NULL,
    creator_name TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_milestones_status ON one_milestones(status, updated_at DESC);
