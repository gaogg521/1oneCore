-- Test plans: high-level plans linking test cases to a requirement milestone.
-- test cases hold individual test steps + result.
CREATE TABLE IF NOT EXISTS one_test_plans (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'active', 'completed', 'archived')),
    requirement_id TEXT,
    creator_id TEXT NOT NULL,
    creator_name TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_test_plans_req ON one_test_plans(requirement_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_test_plans_status ON one_test_plans(status, updated_at DESC);

CREATE TABLE IF NOT EXISTS one_test_cases (
    id TEXT PRIMARY KEY,
    plan_id TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    steps TEXT,      -- JSON string: [{action, expected}]
    expected TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'passed', 'failed', 'blocked', 'skipped')),
    creator_id TEXT NOT NULL,
    creator_name TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_test_cases_plan ON one_test_cases(plan_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_test_cases_status ON one_test_cases(status);
