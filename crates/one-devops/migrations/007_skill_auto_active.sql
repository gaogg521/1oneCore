-- Mixed distribution model (user decision 2026-07-09): admins can mark a team
-- skill as auto-active. Auto-active skills are loaded by member agents without
-- the member opting in per-assistant; optional ones stay opt-in.
ALTER TABLE one_skill_registry ADD COLUMN auto_active INTEGER NOT NULL DEFAULT 0;
