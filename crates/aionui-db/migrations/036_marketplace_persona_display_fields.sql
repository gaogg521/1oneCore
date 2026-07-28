-- Migration 036: expert marketplace catalog display fields
--
-- The catalog's original data source had no Chinese display name, persona
-- character name, category, or avatar image at all (plain kebab-case ids,
-- no frontmatter). The manifest has been swapped for a richer WorkBuddy
-- export that carries these fields — add columns to store them.
--
-- `has_avatar` is a plain flag: the actual bytes live in the embedded
-- `marketplace-personas/avatars/{id}.webp` asset, not in this table.

ALTER TABLE assistant_marketplace_personas ADD COLUMN display_name TEXT;
ALTER TABLE assistant_marketplace_personas ADD COLUMN role_name TEXT;
ALTER TABLE assistant_marketplace_personas ADD COLUMN category TEXT;
ALTER TABLE assistant_marketplace_personas ADD COLUMN has_avatar INTEGER NOT NULL DEFAULT 0;
