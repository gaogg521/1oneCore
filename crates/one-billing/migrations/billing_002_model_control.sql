-- one-billing 002: model control (P1-2) — per-company spend cap + model
-- allowlist, layered on the license row.
--
-- Both NULL = no control (unlimited spend, all models). A company only gets
-- gated once an admin sets a cap / allowlist. Personal / standalone users have
-- no license row and are never gated (the red line).

-- Rolling-30-day estimated-cost budget in USD-micros. NULL = no cap.
ALTER TABLE one_enterprise_license ADD COLUMN monthly_cost_cap_micros INTEGER;

-- JSON array of allowed model names (e.g. ["claude-opus-4-8","gpt-4"]). NULL or
-- empty array = every model allowed.
ALTER TABLE one_enterprise_license ADD COLUMN allowed_models TEXT;
