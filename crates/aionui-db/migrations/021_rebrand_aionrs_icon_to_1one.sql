-- Point the built-in aionrs agent at the 1ONE mascot instead of legacy Aion.svg.
UPDATE agent_metadata
SET icon = '/api/assets/logos/brand/1one.png',
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE agent_type = 'aionrs'
  AND agent_source = 'internal';

UPDATE assistant_definitions
SET avatar_type = 'builtin_asset',
    avatar_value = '/api/assets/logos/brand/1one.png',
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE source = 'generated'
  AND source_ref IN (
    SELECT id
    FROM agent_metadata
    WHERE agent_type = 'aionrs'
      AND agent_source = 'internal'
  )
  AND (
    avatar_value IS NULL
    OR avatar_value LIKE '%aion.svg%'
    OR avatar_value LIKE '%/brand/aion%'
  );
