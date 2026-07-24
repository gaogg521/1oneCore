//! Row types and API DTOs for one-sso.

use serde::{Deserialize, Serialize};

/// Provider kind. LDAP is password-based; Feishu/DingTalk/WeCom are国产 OAuth;
/// OIDC is the standard OpenID Connect flow (Okta / Azure AD / Google
/// Workspace all speak it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SsoProviderKind {
    Feishu,
    Dingtalk,
    Wecom,
    Ldap,
    Oidc,
}

impl SsoProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::Dingtalk => "dingtalk",
            Self::Wecom => "wecom",
            Self::Ldap => "ldap",
            Self::Oidc => "oidc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "feishu" => Some(Self::Feishu),
            "dingtalk" => Some(Self::Dingtalk),
            "wecom" => Some(Self::Wecom),
            "ldap" => Some(Self::Ldap),
            "oidc" => Some(Self::Oidc),
            _ => None,
        }
    }
}

/// Provider config row — `config` is opaque JSON shaped per provider.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SsoProviderRow {
    pub provider: String,
    pub enabled: bool,
    pub config: String,
    pub updated_at: i64,
    pub updated_by: Option<String>,
}

/// External identity binding (provider + external_id → local user_id).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SsoIdentityRow {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub user_id: String,
    pub tenant_id: String,
    pub last_seen_at: Option<i64>,
    pub created_at: i64,
}

/// Public provider status (returned to the login page so it can show which
/// SSO buttons to render). Secrets are stripped.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoProviderStatusDto {
    pub provider: String,
    pub enabled: bool,
    pub configured: bool,
}

/// Admin-only status + non-secret config values, used to pre-fill the
/// settings form so re-editing a provider doesn't require retyping fields
/// (App ID, Redirect URI, ...) that are already saved. Secret fields (App
/// Secret / Secret / Bind Password) are still stripped — only the admin
/// route serves this, never the public one.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoProviderConfigDto {
    pub provider: String,
    pub enabled: bool,
    pub configured: bool,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProviderBody {
    pub enabled: Option<bool>,
    /// Replaces the stored config JSON when present.
    pub config: Option<serde_json::Value>,
}
