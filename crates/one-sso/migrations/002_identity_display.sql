-- one-sso 002: capture a raw display-name/org-path snapshot alongside the
-- identity binding.
--
-- `users.username` stays ASCII-only (system-wide login identifier enforced
-- by `aionui_auth::validate_username`) so JIT-provisioned users with
-- non-ASCII real names (e.g. Chinese) were silently falling back to a
-- `sso_<random>` placeholder with the actual name discarded nowhere. These
-- two columns store the UNSANITIZED profile fields the provider returned at
-- last login — independent of the login username, never touching it.
ALTER TABLE one_sso_identities ADD COLUMN display_name TEXT;
ALTER TABLE one_sso_identities ADD COLUMN org_unit_path TEXT;
