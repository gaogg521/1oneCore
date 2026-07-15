-- one-org 003: job title on the tenant membership row, alongside
-- display_name/org_unit_path from 002. Populated at join time from the
-- joining user's SSO identity, same as those two columns.
ALTER TABLE one_user_org ADD COLUMN job_title TEXT;
