-- D5: team MCP connectors need their credentials to actually work on member
-- machines. Offline-first (the whole refactor principle) rules out a
-- server-side proxy, so the connector's secrets — stdio `env` / sse `headers`
-- as a JSON object — are stored here and distributed to members for local
-- materialization. Security tradeoff: any org member who can read the registry
-- receives these secrets. Only store credentials meant to be team-shared.
ALTER TABLE one_mcp_registry ADD COLUMN secrets_json TEXT;
