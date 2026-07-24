-- one-devops 009: per-resource read visibility (P0-4 fine-grained RBAC).
--
-- The board is instance-per-enterprise. Skill / MCP / RAG rows already carry
-- `scope` ('org' default) + `team_id` from 001_init, but the write path
-- hardcoded them to 'org'/NULL, so every resource was org-wide and reads did
-- no filtering at all. P0-4 turns them into a real read ACL along two
-- dimensions:
--
--   * scope/team_id (existing columns): `scope='org'` = visible to the whole
--     enterprise; `scope='team'` + `team_id` (a one_tenants.id project group) =
--     visible only to members of that project group (resolved via
--     one_user_org, which P0-1 made multi-membership).
--   * visibility (this column): `'all'` = every member in scope may read;
--     `'admin'` = only org/system admins may read (sensitive knowledge).
--
-- Default 'all' keeps every existing row byte-identical in behavior; combined
-- with the existing 'org' scope default, the personal/standalone edition and
-- all pre-P0-4 resources are unaffected.

ALTER TABLE one_skill_registry ADD COLUMN visibility TEXT NOT NULL DEFAULT 'all';
ALTER TABLE one_mcp_registry   ADD COLUMN visibility TEXT NOT NULL DEFAULT 'all';
ALTER TABLE one_rag_documents  ADD COLUMN visibility TEXT NOT NULL DEFAULT 'all';

-- Team-scoped reads filter on team_id; index the three tables for it.
CREATE INDEX IF NOT EXISTS idx_one_skill_registry_scope ON one_skill_registry(scope, team_id);
CREATE INDEX IF NOT EXISTS idx_one_mcp_registry_scope   ON one_mcp_registry(scope, team_id);
CREATE INDEX IF NOT EXISTS idx_one_rag_documents_scope  ON one_rag_documents(scope, team_id);
