-- one-employee 002: per-agent schedule for cron-driven runs.
-- Adds three columns to one_personal_agents so the in-crate 30s scanner
-- can pick up due schedules without touching upstream aionui-cron state.
-- schedule stores CronScheduleDto JSON (tag = "kind": at/every/cron);
-- schedule_enabled gates the scanner; next_run_at is the cached next
-- fire time (ms epoch) recomputed via aionui_cron::compute_next_run.

ALTER TABLE one_personal_agents ADD COLUMN schedule TEXT;
ALTER TABLE one_personal_agents ADD COLUMN schedule_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE one_personal_agents ADD COLUMN next_run_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_one_personal_agents_schedule
    ON one_personal_agents(schedule_enabled, next_run_at)
    WHERE schedule_enabled = 1;
