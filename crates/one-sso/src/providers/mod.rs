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

pub mod feishu;
pub mod dingtalk;
pub mod wecom;

pub use feishu::FeishuProvider;
pub use dingtalk::DingtalkProvider;
pub use wecom::WecomProvider;

/// Normalized user info across OAuth providers.
#[derive(Debug, Clone)]
pub struct ProviderUserInfo {
    pub external_id: String,
    pub preferred_username: String,
    pub org_unit_path: Option<String>,
}
