//! DingTalk OAuth provider.
//!
//! Translation of `DingTalkAuthProvider.ts`. DingTalk uses a modern
//! userAccessToken endpoint (v1.0) and the legacy gettoken endpoint for
//! credential validation.

use serde::{Deserialize, Serialize};

use crate::error::SsoError;
use crate::providers::ProviderUserInfo;

const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

const AUTHORIZE_URL: &str = "https://login.dingtalk.com/oauth2/auth";
const TOKEN_URL: &str = "https://api.dingtalk.com/v1.0/oauth2/userAccessToken";
const USER_INFO_URL: &str = "https://api.dingtalk.com/v1.0/contact/users/me";
const GETTOKEN_URL: &str = "https://oapi.dingtalk.com/gettoken";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DingtalkProviderConfig {
    pub app_key: String,
    pub app_secret: String,
    #[serde(default)]
    pub corp_id: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default = "default_external_id_field")]
    pub external_id_field: String,
}

fn default_external_id_field() -> String {
    "unionId".into()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DingtalkUserTokenResponse {
    access_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DingtalkUserInfo {
    pub union_id: Option<String>,
    pub open_id: Option<String>,
    pub nick: Option<String>,
    pub mobile: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct DingtalkLegacyTokenResponse {
    errcode: Option<i64>,
    errmsg: Option<String>,
    access_token: Option<String>,
}

pub struct DingtalkProvider;

impl DingtalkProvider {
    pub fn build_authorize_url(config: &DingtalkProviderConfig, state: &str) -> String {
        let redirect = config.redirect_uri.as_deref().unwrap_or("");
        format!(
            "{AUTHORIZE_URL}?redirect_uri={}&response_type=code&client_id={}&scope=openid&state={}&prompt=consent",
            urlencode(redirect),
            urlencode(&config.app_key),
            urlencode(state),
        )
    }

    pub async fn exchange_code(config: &DingtalkProviderConfig, code: &str) -> Result<String, SsoError> {
        let client = http_client()?;
        let body = serde_json::json!({
            "clientId": config.app_key,
            "clientSecret": config.app_secret,
            "code": code,
            "grantType": "authorization_code",
        });
        let resp = client.post(TOKEN_URL).json(&body).send().await?;
        let status = resp.status();
        let data: DingtalkUserTokenResponse = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!(
                "DingTalk token exchange failed: HTTP {status}"
            )));
        }
        data.access_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SsoError::Internal("DingTalk token exchange: missing access_token".into()))
    }

    pub async fn fetch_user_info(access_token: &str) -> Result<DingtalkUserInfo, SsoError> {
        let client = http_client()?;
        let resp = client
            .get(USER_INFO_URL)
            .header("x-acs-dingtalk-access-token", access_token)
            .send()
            .await?;
        let status = resp.status();
        let data: DingtalkUserInfo = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("DingTalk user info failed: HTTP {status}")));
        }
        Ok(data)
    }

    pub fn resolve_external_id(info: &DingtalkUserInfo, field: &str) -> Option<String> {
        let (primary, fallback) = if field == "openId" {
            (&info.open_id, &info.union_id)
        } else {
            (&info.union_id, &info.open_id)
        };
        if let Some(v) = primary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(v.to_owned());
        }
        fallback
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    pub fn to_provider_user_info(info: &DingtalkUserInfo, external_id: &str) -> ProviderUserInfo {
        let preferred = info
            .nick
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("dingtalk_{}", &external_id[..external_id.len().min(16)]));
        ProviderUserInfo {
            external_id: external_id.to_owned(),
            preferred_username: preferred,
            org_unit_path: info.mobile.clone(),
            job_title: None,
            // DingTalk corp_id is available but not threaded through yet — the
            // auto-join enterprise path currently targets Feishu only.
            org_external_id: None,
        }
    }

    pub async fn test_credentials(app_key: &str, app_secret: &str) -> Result<(), SsoError> {
        let key = app_key.trim();
        let secret = app_secret.trim();
        if key.is_empty() || secret.is_empty() || secret == "******" {
            return Err(SsoError::BadRequest(
                "AppKey and AppSecret are required for connection test".into(),
            ));
        }
        let client = http_client()?;
        let resp = client
            .get(GETTOKEN_URL)
            .query(&[("appkey", key), ("appsecret", secret)])
            .send()
            .await?;
        let status = resp.status();
        let data: DingtalkLegacyTokenResponse = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("DingTalk API error: HTTP {status}")));
        }
        if data.errcode.unwrap_or(-1) != 0 {
            return Err(SsoError::Internal(
                data.errmsg.unwrap_or_else(|| "DingTalk token request failed".into()),
            ));
        }
        Ok(())
    }
}

fn http_client() -> Result<reqwest::Client, SsoError> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| SsoError::Internal(format!("http client: {e}")))
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
        let cfg = DingtalkProviderConfig {
            app_key: "ding_test".into(),
            app_secret: "secret".into(),
            corp_id: None,
            redirect_uri: Some("https://example.com/api/one/sso/dingtalk/callback".into()),
            external_id_field: "unionId".into(),
        };
        let url = DingtalkProvider::build_authorize_url(&cfg, "state456");
        assert!(url.contains("client_id=ding_test"));
        assert!(url.contains("state=state456"));
        assert!(url.contains("scope=openid"));
    }

    #[test]
    fn resolve_external_id_prefers_configured_field() {
        let info = DingtalkUserInfo {
            union_id: Some("uid".into()),
            open_id: Some("oid".into()),
            nick: None,
            mobile: None,
        };
        assert_eq!(
            DingtalkProvider::resolve_external_id(&info, "unionId").as_deref(),
            Some("uid")
        );
        assert_eq!(
            DingtalkProvider::resolve_external_id(&info, "openId").as_deref(),
            Some("oid")
        );
    }
}
