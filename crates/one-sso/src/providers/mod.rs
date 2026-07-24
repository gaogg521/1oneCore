//! SSO providers — Feishu / DingTalk / WeCom OAuth + LDAP bind.
//!
//! Each OAuth provider exposes the same shape:
//! - `build_authorize_url(config, redirect_uri, state) -> String`
//! - `exchange_code(config, code, redirect_uri) -> String` (access token)
//! - `fetch_user_info(token) -> ProviderUserInfo`
//! - `test_credentials(config) -> Result<()>`
//!
//! LDAP is password-based and lives in `ldap.rs`; it exposes
//! `authenticate(config, username, password) -> LdapAuthSuccess`.
//!
//! OIDC (`oidc.rs`) is the standard OpenID Connect authorization-code flow
//! (Okta / Azure AD / Google Workspace); it discovers its endpoints and adds a
//! `discover` step before `build_authorize_url` / `exchange_code`.

pub mod dingtalk;
pub mod feishu;
pub mod ldap;
pub mod oidc;
pub mod wecom;

pub use dingtalk::DingtalkProvider;
pub use feishu::FeishuProvider;
pub use ldap::LdapProvider;
pub use oidc::OidcProvider;
pub use wecom::WecomProvider;

/// Normalized user info across OAuth providers.
#[derive(Debug, Clone)]
pub struct ProviderUserInfo {
    pub external_id: String,
    pub preferred_username: String,
    pub org_unit_path: Option<String>,
    /// Job title — only Feishu populates this today (via its Contact API,
    /// see `feishu::FeishuProvider::fetch_org_profile`); other providers
    /// leave it `None`.
    pub job_title: Option<String>,
    /// Company/organization identifier from the IdP (Feishu `tenant_key`,
    /// DingTalk `corp_id`, WeCom `corpid`). Used to bind/auto-join the SSO
    /// "real enterprise" tenant — same-company logins resolve to the same
    /// enterprise without an invite code. `None` for LDAP/local logins and
    /// providers that don't surface it yet.
    pub org_external_id: Option<String>,
}
