-- Cursor Agent CLI was renamed: modern installs expose `agent` (and legacy
-- `cursor-agent`) under %LOCALAPPDATA%\cursor-agent, not `cursor` on PATH.
-- Spawn for ACP mode is `agent acp`, not `cursor acp`.
UPDATE agent_metadata
SET command = 'agent',
    agent_source_info = '{"binary_name":"agent"}',
    updated_at = unixepoch('now','subsec') * 1000
WHERE id = 'a0dfb1ec'
   OR backend = 'cursor';
