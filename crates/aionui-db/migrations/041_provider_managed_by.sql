-- Who owns this provider row.
--
-- NULL (the default, and the only value on a personal install) means the user
-- configured it themselves: theirs to edit and delete, exactly as before.
-- 'enterprise' means it was provisioned by a company model channel and is
-- materialized here by a sync — the client renders it read-only and reconciles
-- it against the server, and its api_key holds a revocable channel token rather
-- than a real vendor credential.
--
-- A column rather than a separate table because everything downstream — chat,
-- the ACP bridges, all three media adapter forms — already reads providers and
-- must keep working with no branch for where the row came from.
ALTER TABLE providers ADD COLUMN managed_by TEXT;

CREATE INDEX IF NOT EXISTS idx_providers_managed_by ON providers(managed_by);
