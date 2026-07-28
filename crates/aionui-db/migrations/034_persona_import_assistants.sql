-- Migration 034: allow `assistant_definitions.source = 'imported'`
--
-- Widens the `source` CHECK constraint so persona-import (Claude Code
-- sub-agent .md files imported via the assistant library) can be tagged
-- distinctly from hand-authored `user` assistants, while still classifying
-- as a mutable non-builtin/non-generated row everywhere else in the
-- service layer (see `AssistantService::classify_source`).
--
-- SQLite has no `ALTER TABLE ... MODIFY CHECK`, so this rebuilds the table
-- following the same pattern already used in this codebase (see migration
-- 013's `_assistants_new` / `_assistant_overrides_new` rebuilds).

CREATE TABLE IF NOT EXISTS _assistant_definitions_new (
    id                                  TEXT PRIMARY KEY,
    assistant_id                        TEXT    NOT NULL,
    source                              TEXT    NOT NULL
                                                 CHECK (source IN ('builtin', 'user', 'generated', 'imported')),
    owner_type                          TEXT    NOT NULL
                                                 CHECK (owner_type IN ('system', 'user')),
    source_ref                          TEXT,
    name                                TEXT    NOT NULL,
    name_i18n                           TEXT    NOT NULL DEFAULT '{}',
    description                         TEXT,
    description_i18n                    TEXT    NOT NULL DEFAULT '{}',
    avatar_type                         TEXT    NOT NULL DEFAULT 'none'
                                                 CHECK (avatar_type IN ('none', 'emoji', 'builtin_asset', 'user_asset')),
    avatar_value                        TEXT,
    agent_id                            TEXT    NOT NULL,
    rule_resource_type                  TEXT    NOT NULL
                                                 CHECK (
                                                     rule_resource_type IN (
                                                         'none',
                                                         'builtin_asset',
                                                         'user_file',
                                                         'inline',
                                                         'extension'
                                                     )
                                                 ),
    rule_resource_ref                   TEXT,
    recommended_prompts                 TEXT    NOT NULL DEFAULT '[]',
    recommended_prompts_i18n            TEXT    NOT NULL DEFAULT '{}',
    default_model_mode                  TEXT    NOT NULL
                                                 CHECK (default_model_mode IN ('auto', 'fixed')),
    default_model_value                 TEXT,
    default_permission_mode             TEXT    NOT NULL
                                                 CHECK (default_permission_mode IN ('auto', 'fixed')),
    default_permission_value            TEXT,
    default_thought_level_mode          TEXT    NOT NULL DEFAULT 'auto'
                                                 CHECK (default_thought_level_mode IN ('auto', 'fixed')),
    default_thought_level_value         TEXT,
    default_skills_mode                 TEXT    NOT NULL
                                                 CHECK (default_skills_mode IN ('auto', 'fixed')),
    default_skill_ids                   TEXT    NOT NULL DEFAULT '[]',
    custom_skill_names                  TEXT    NOT NULL DEFAULT '[]',
    default_disabled_builtin_skill_ids  TEXT    NOT NULL DEFAULT '[]',
    default_mcps_mode                   TEXT    NOT NULL
                                                 CHECK (default_mcps_mode IN ('auto', 'fixed')),
    default_mcp_ids                     TEXT    NOT NULL DEFAULT '[]',
    created_at                          INTEGER NOT NULL,
    updated_at                          INTEGER NOT NULL,
    deleted_at                          INTEGER
);

INSERT OR IGNORE INTO _assistant_definitions_new (
    id, assistant_id, source, owner_type, source_ref, name, name_i18n,
    description, description_i18n, avatar_type, avatar_value, agent_id,
    rule_resource_type, rule_resource_ref, recommended_prompts,
    recommended_prompts_i18n, default_model_mode, default_model_value,
    default_permission_mode, default_permission_value,
    default_thought_level_mode, default_thought_level_value,
    default_skills_mode, default_skill_ids, custom_skill_names,
    default_disabled_builtin_skill_ids, default_mcps_mode, default_mcp_ids,
    created_at, updated_at, deleted_at
)
SELECT
    id, assistant_id, source, owner_type, source_ref, name, name_i18n,
    description, description_i18n, avatar_type, avatar_value, agent_id,
    rule_resource_type, rule_resource_ref, recommended_prompts,
    recommended_prompts_i18n, default_model_mode, default_model_value,
    default_permission_mode, default_permission_value,
    default_thought_level_mode, default_thought_level_value,
    default_skills_mode, default_skill_ids, custom_skill_names,
    default_disabled_builtin_skill_ids, default_mcps_mode, default_mcp_ids,
    created_at, updated_at, deleted_at
FROM assistant_definitions;

ALTER TABLE assistant_definitions RENAME TO _assistant_definitions_old;
ALTER TABLE _assistant_definitions_new RENAME TO assistant_definitions;
DROP TABLE IF EXISTS _assistant_definitions_old;

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_source_ref
    ON assistant_definitions(source, source_ref)
    WHERE source_ref IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_assistant_definitions_source
    ON assistant_definitions(source);

CREATE INDEX IF NOT EXISTS idx_assistant_definitions_agent_id
    ON assistant_definitions(agent_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_assistant_id
    ON assistant_definitions(assistant_id);
