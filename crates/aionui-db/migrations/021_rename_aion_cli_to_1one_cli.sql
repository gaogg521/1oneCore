-- Rebrand internal aionrs agent display name for 1ONE fork.
UPDATE agent_metadata
SET name = '1ONE CLI',
    updated_at = unixepoch('now','subsec') * 1000
WHERE agent_type = 'aionrs'
  AND agent_source = 'internal';

UPDATE assistant_definitions
SET name = '1ONE CLI',
    updated_at = unixepoch('now','subsec') * 1000
WHERE source = 'generated'
  AND source_ref IN (
    SELECT id
    FROM agent_metadata
    WHERE agent_type = 'aionrs'
      AND agent_source = 'internal'
  );
