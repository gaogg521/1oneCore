-- Single-row config for redirecting the built-in Claude Code ACP agent to a
-- user-saved provider/model. Unlike the Codex bridge, this needs no local
-- HTTP compatibility layer: the Anthropic Messages wire protocol is already
-- what Claude Code speaks, so the saved provider's own base_url/api_key are
-- injected directly as ANTHROPIC_BASE_URL/ANTHROPIC_AUTH_TOKEN.
CREATE TABLE IF NOT EXISTS claude_bridge_config (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0,
    provider_id TEXT,
    model TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
