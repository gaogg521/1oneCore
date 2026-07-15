-- one-org 002: display_name snapshot on tenant membership rows.
--
-- Pairs with the existing org_unit_path / org_profile_source /
-- org_profile_synced_at columns from 001_init — those were added but never
-- actually written by any code path. Populated at join time (join_with_invite
-- / create_tenant) from the joining user's SSO identity, if any; NULL for
-- locally-created members.
ALTER TABLE one_user_org ADD COLUMN display_name TEXT;
