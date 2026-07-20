-- The per-model max output tokens override on providers.model_max_tokens
-- (added in 024) is no longer read anywhere: the aionrs embedded runtime
-- config now always resolves max_tokens to None to match upstream's
-- "ignore max token limits for aionui requests" fix, isolating the runtime
-- from any leaked config-file value. Drop the now-dead column.
ALTER TABLE providers DROP COLUMN model_max_tokens;
