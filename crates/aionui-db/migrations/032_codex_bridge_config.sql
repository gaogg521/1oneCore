-- Single-row config for the local Codex-compatibility bridge: lets the
-- external Codex CLI (which only speaks the OpenAI Responses wire format)
-- reach a user-saved provider/model through the app's own hardened
-- Chat Completions provider layer instead of the model's raw API.
CREATE TABLE IF NOT EXISTS codex_bridge_config (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0,
    provider_id TEXT,
    model TEXT,
    bearer_token TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
