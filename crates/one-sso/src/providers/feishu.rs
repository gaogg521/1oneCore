//! Feishu (Lark) OAuth provider.
//!
//! Direct translation of the 1ONE TS reference (`FeishuAuthProvider.ts`),
//! kept in Rust so the crate has no Node dependency.

use serde::{Deserialize, Serialize};

use crate::error::SsoError;
use crate::providers::ProviderUserInfo;

const FEISHU_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

const AUTHORIZE_URL: &str = "https://passport.feishu.cn/suite/passport/oauth/authorize";
const TOKEN_URL: &str = "https://open.feishu.cn/open-apis/authen/v2/oauth/token";
const USER_INFO_URL: &str = "https://open.feishu.cn/open-apis/authen/v1/user_info";
const TENANT_TOKEN_URL: &str = "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeishuProviderConfig {
    pub app_id: String,
    pub app_secret: String,
    pub redirect_uri: String,
    #[serde(default = "default_external_id_field")]
    pub external_id_field: String,
}

fn default_external_id_field() -> String {
    "union_id".into()
}

#[derive(Debug, Clone, Deserialize)]
struct FeishuApiResponse<T> {
    code: i64,
    msg: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Clone, Deserialize)]
struct FeishuTokenResponse {
    access_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct FeishuUserInfo {
    pub name: Option<String>,
    pub en_name: Option<String>,
    pub open_id: Option<String>,
    pub union_id: Option<String>,
    pub tenant_key: Option<String>,
    pub avatar_url: Option<String>,
}

pub struct FeishuProvider;

impl FeishuProvider {
    pub fn build_authorize_url(config: &FeishuProviderConfig, state: &str) -> String {
        // URL-encoded by hand — axum/reqwest don't expose a builder we can
        // use without pulling in another crate.
        format!(
            "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&state={}",
            urlencode(&config.app_id),
            urlencode(&config.redirect_uri),
            urlencode(state),
        )
    }

    pub async fn exchange_code(
        config: &FeishuProviderConfig,
        code: &str,
    ) -> Result<String, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;

        let mut body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": config.app_id,
            "client_secret": config.app_secret,
            "code": code,
        });
        if !config.redirect_uri.is_empty() {
            body["redirect_uri"] = serde_json::Value::String(config.redirect_uri.clone());
        }

        let resp = client.post(TOKEN_URL).json(&body).send().await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();

        let api: FeishuApiResponse<FeishuTokenResponse> =
            serde_json::from_value(json.clone()).unwrap_or(FeishuApiResponse {
                code: -1,
                msg: None,
                data: None,
            });
        if !status.is_success() {
            return Err(SsoError::Internal(format!(
                "Feishu token exchange failed: HTTP {status}"
            )));
        }
        if api.code != 0 {
            return Err(SsoError::Internal(format!(
                "Feishu token exchange failed: {}",
                api.msg.unwrap_or_else(|| "unknown error".into())
            )));
        }
        // Access token may appear at top-level or nested under data.
        let token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| api.data.and_then(|d| d.access_token))
            .ok_or_else(|| SsoError::Internal("Feishu token exchange: missing access_token".into()))?;
        Ok(token)
    }

    pub async fn fetch_user_info(access_token: &str) -> Result<FeishuUserInfo, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;

        let resp = client
            .get(USER_INFO_URL)
            .bearer_auth(access_token)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();

        let api: FeishuApiResponse<FeishuUserInfo> =
            serde_json::from_value(json).unwrap_or(FeishuApiResponse {
                code: -1,
                msg: None,
                data: None,
            });
        if !status.is_success() {
            return Err(SsoError::Internal(format!(
                "Feishu user_info failed: HTTP {status}"
            )));
        }
        if api.code != 0 {
            return Err(SsoError::Internal(format!(
                "Feishu user_info failed: {}",
                api.msg.unwrap_or_else(|| "unknown error".into())
            )));
        }
        Ok(api.data.unwrap_or_default())
    }

    /// Pick the configured external-id field, falling back to the other one
    /// — same rule as the TS reference.
    pub fn resolve_external_id(info: &FeishuUserInfo, field: &str) -> Option<String> {
        let (primary, fallback) = if field == "open_id" {
            (&info.open_id, &info.union_id)
        } else {
            (&info.union_id, &info.open_id)
        };
        if let Some(v) = primary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(v.to_owned());
        }
        fallback.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    }

    pub fn to_provider_user_info(info: &FeishuUserInfo, external_id: &str) -> ProviderUserInfo {
        let preferred = info
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| info.en_name.as_deref().map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("feishu_{}", &external_id[..external_id.len().min(16)]));
        ProviderUserInfo {
            external_id: external_id.to_owned(),
            preferred_username: preferred,
            org_unit_path: info.tenant_key.clone(),
        }
    }

    /// Validate App ID + App Secret by requesting a tenant access token.
    /// Used by the admin "Test connection" button.
    pub async fn test_credentials(app_id: &str, app_secret: &str) -> Result<(), SsoError> {
        let id = app_id.trim();
        let secret = app_secret.trim();
        if id.is_empty() || secret.is_empty() || secret == "******" {
            return Err(SsoError::BadRequest(
                "App ID and App Secret are required for connection test".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;
        let resp = client
            .post(TENANT_TOKEN_URL)
            .json(&serde_json::json!({ "app_id": id, "app_secret": secret }))
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        let api: FeishuApiResponse<serde_json::Value> =
            serde_json::from_value(json).unwrap_or(FeishuApiResponse {
                code: -1,
                msg: None,
                data: None,
            });
        if !status.is_success() {
            return Err(SsoError::Internal(format!("Feishu API error: HTTP {status}")));
        }
        if api.code != 0 {
            return Err(SsoError::Internal(
                api.msg.unwrap_or_else(|| "Feishu tenant token request failed".into()),
            ));
        }
        Ok(())
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_authorize_url_contains_required_params() {
        let cfg = FeishuProviderConfig {
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
            redirect_uri: "https://example.com/api/one/sso/feishu/callback".into(),
            external_id_field: "union_id".into(),
        };
        let url = FeishuProvider::build_authorize_url(&cfg, "state123");
        assert!(url.contains("client_id=cli_test"));
        assert!(url.contains("state=state123"));
        assert!(url.contains("response_type=code"));
    }

    #[test]
    fn resolve_external_id_prefers_configured_field() {
        let info = FeishuUserInfo {
            name: Some("张三".into()),
            en_name: None,
            open_id: Some("ou_123".into()),
            union_id: Some("on_456".into()),
            tenant_key: None,
            avatar_url: None,
        };
        assert_eq!(
            FeishuProvider::resolve_external_id(&info, "union_id").as_deref(),
            Some("on_456")
        );
        assert_eq!(
            FeishuProvider::resolve_external_id(&info, "open_id").as_deref(),
            Some("ou_123")
        );
    }

    #[test]
    fn resolve_external_id_falls_back_to_other_field() {
        let info = FeishuUserInfo {
            name: None,
            en_name: None,
            open_id: None,
            union_id: Some("on_789".into()),
            tenant_key: None,
            avatar_url: None,
        };
        assert_eq!(
            FeishuProvider::resolve_external_id(&info, "open_id").as_deref(),
            Some("on_789")
        );
    }

    #[test]
    fn to_provider_user_info_uses_display_name() {
        let info = FeishuUserInfo {
            name: Some("张三".into()),
            en_name: Some("Zhang San".into()),
            open_id: Some("ou_abc".into()),
            union_id: None,
            tenant_key: None,
            avatar_url: None,
        };
        let p = FeishuProvider::to_provider_user_info(&info, "ou_abc");
        assert_eq!(p.preferred_username, "张三");
        assert_eq!(p.external_id, "ou_abc");
    }

    #[test]
    fn to_provider_user_info_falls_back_to_provider_prefix() {
        let info = FeishuUserInfo {
            name: None,
            en_name: None,
            open_id: None,
            union_id: None,
            tenant_key: None,
            avatar_url: None,
        };
        let p = FeishuProvider::to_provider_user_info(&info, "ext_1234567890");
        assert!(p.preferred_username.starts_with("feishu_"));
    }
}
