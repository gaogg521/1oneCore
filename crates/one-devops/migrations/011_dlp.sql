-- one-devops 011: content inspection rules + the findings they produce (T4).
--
-- The company governance that existed before this was financial: how much was
-- spent, which models may be used. The question a security team asks first is a
-- content question — what did an employee paste into a model, and can we tell
-- afterwards. This is that.
--
-- # Why rules live here but are enforced on the member's machine
--
-- The check has to see the prompt, and the prompt is composed on the member's
-- own co-located backend. So rules are authored here, distributed like team
-- skills and MCP connectors, and evaluated locally; findings travel back up the
-- same way. That also means enforcement covers a member's *personal* provider,
-- which a proxy-only design would miss entirely — and pasting a contract into
-- a personally-configured model is exactly the leak worth catching.
--
-- ⚠️ Accepted tradeoff: rule patterns land on member machines, and a pattern can
-- itself be sensitive (a customer name, a project codename). This was a
-- deliberate product decision over the alternative of a per-send round trip to
-- the server, which would add latency to every message and fail open whenever
-- the server is unreachable.

CREATE TABLE IF NOT EXISTS one_dlp_rules (
    id          TEXT    PRIMARY KEY NOT NULL,
    name        TEXT    NOT NULL,
    -- 'keyword' (case-insensitive substring), 'regex', or 'builtin' (a shipped
    -- pattern selected by id — see aionui_common::dlp::BUILTIN_PATTERN_IDS).
    matcher     TEXT    NOT NULL DEFAULT 'keyword' CHECK(matcher IN ('keyword','regex','builtin')),
    pattern     TEXT    NOT NULL,
    -- 'log' records and lets the send through; 'block' refuses it. New rules
    -- default to 'log': a rule that blocks before anyone has seen its real hit
    -- rate produces false positives that push people to paste into a browser
    -- instead, which is worse for the company than the leak it was meant to stop.
    action      TEXT    NOT NULL DEFAULT 'log' CHECK(action IN ('log','block')),
    enabled     INTEGER NOT NULL DEFAULT 1,
    -- Same scoping as the other registries: 'org' applies company-wide, 'team'
    -- + team_id only inside that project group.
    scope       TEXT    NOT NULL DEFAULT 'org',
    team_id     TEXT,
    created_by  TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_dlp_rules_scope ON one_dlp_rules(scope, team_id);

-- One rule hitting one message.
--
-- `excerpt` is the surrounding sentence with the matched span MASKED — never the
-- matched value. An audit table that quotes the ID numbers it caught has moved
-- the leak into the one place that aggregates every such value in the company,
-- making itself the highest-value target in the deployment. See
-- `aionui_common::dlp` for the masking.
CREATE TABLE IF NOT EXISTS one_dlp_events (
    id              TEXT    PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL,
    -- Where it happened, so a reviewer can follow it back. Nullable because a
    -- caller may genuinely not have one.
    conversation_id TEXT,
    -- Which model the text was on its way to.
    model           TEXT,
    rule_id         TEXT    NOT NULL,
    -- Denormalised so a finding still reads correctly after the rule is renamed
    -- or deleted — the same reason the board tables keep `creator_name`.
    rule_name       TEXT    NOT NULL,
    action          TEXT    NOT NULL,
    hits            INTEGER NOT NULL DEFAULT 1,
    excerpt         TEXT    NOT NULL DEFAULT '',
    -- Which project group the member was acting in, for scoped review.
    team_id         TEXT,
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_dlp_events_time ON one_dlp_events(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_one_dlp_events_user ON one_dlp_events(user_id, created_at DESC);
