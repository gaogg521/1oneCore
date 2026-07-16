//! WeCom (企业微信) OAuth provider.
//!
//! Translation of `WeComAuthProvider.ts`. Unlike Feishu/DingTalk, WeCom
//! uses a corp access token (not a user access token) + OAuth code →
//! corp UserId. The "external id" is the corp UserId.

use serde::{Deserialize, Serialize};

use crate::error::SsoError;
use crate::providers::ProviderUserInfo;

const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

const AUTHORIZE_URL: &str = "https://open.weixin.qq.com/connect/oauth2/authorize";
const GETTOKEN_URL: &str = "https://qyapi.weixin.qq.com/cgi-bin/gettoken";
const USER_INFO_URL: &str = "https://qyapi.weixin.qq.com/cgi-bin/user/getuserinfo";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WecomProviderConfig {
    pub corp_id: String,
    pub agent_id: String,
    pub secret: String,
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct WecomTokenResponse {
    errcode: Option<i64>,
    errmsg: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all(serialize = "camelCase", deserialize = "PascalCase"))]
struct WecomUserInfoResponse {
    errcode: Option<i64>,
    errmsg: Option<String>,
    #[serde(alias = "UserId", alias = "userid")]
    user_id: Option<String>,
    #[serde(alias = "OpenId", alias = "openid")]
    open_id: Option<String>,
}

pub struct WecomProvider;

impl WecomProvider {
    pub fn build_authorize_url(config: &WecomProviderConfig, state: &str) -> String {
        let redirect = config.redirect_uri.as_deref().unwrap_or("");
        format!(
            "{AUTHORIZE_URL}?appid={}&redirect_uri={}&response_type=code&scope=snsapi_base&state={}&agentid={}#wechat_redirect",
            urlencode(&config.corp_id),
            urlencode(redirect),
            urlencode(state),
            urlencode(&config.agent_id),
        )
    }

    pub async fn fetch_corp_access_token(corp_id: &str, secret: &str) -> Result<String, SsoError> {
        let client = http_client()?;
        let resp = client
            .get(GETTOKEN_URL)
            .query(&[("corpid", corp_id), ("corpsecret", secret)])
            .send()
            .await?;
        let status = resp.status();
        let data: WecomTokenResponse = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("WeCom API error: HTTP {status}")));
        }
        if data.errcode.unwrap_or(-1) != 0 {
            return Err(SsoError::Internal(
                data.errmsg.unwrap_or_else(|| "WeCom token request failed".into()),
            ));
        }
        data.access_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SsoError::Internal("WeCom token request: missing access_token".into()))
    }

    /// Returns the corp UserId (the external id for WeCom).
    pub async fn fetch_user_id_by_code(access_token: &str, code: &str) -> Result<String, SsoError> {
        let client = http_client()?;
        let resp = client
            .get(USER_INFO_URL)
            .query(&[("access_token", access_token), ("code", code)])
            .send()
            .await?;
        let status = resp.status();
        let data: WecomUserInfoResponse = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("WeCom user info failed: HTTP {status}")));
        }
        if data.errcode.unwrap_or(-1) != 0 {
            return Err(SsoError::Internal(
                data.errmsg.unwrap_or_else(|| "WeCom user info failed".into()),
            ));
        }
        data.user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| SsoError::Internal("WeCom user info: missing UserId".into()))
    }

    pub fn to_provider_user_info(user_id: &str) -> ProviderUserInfo {
        ProviderUserInfo {
            external_id: user_id.to_owned(),
            preferred_username: format!("wecom_{}", &user_id[..user_id.len().min(16)]),
            org_unit_path: None,
            job_title: None,
            org_external_id: None,
        }
    }

    pub async fn test_credentials(corp_id: &str, secret: &str) -> Result<(), SsoError> {
        let cid = corp_id.trim();
        let sec = secret.trim();
        if cid.is_empty() || sec.is_empty() || sec == "******" {
            return Err(SsoError::BadRequest(
                "CorpId and Secret are required for connection test".into(),
            ));
        }
        Self::fetch_corp_access_token(cid, sec).await.map(|_| ())
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
        let cfg = WecomProviderConfig {
            corp_id: "corp_abc".into(),
            agent_id: "1000002".into(),
            secret: "secret".into(),
            redirect_uri: Some("https://example.com/api/one/sso/wecom/callback".into()),
        };
        let url = WecomProvider::build_authorize_url(&cfg, "state789");
        assert!(url.contains("appid=corp_abc"));
        assert!(url.contains("agentid=1000002"));
        assert!(url.contains("state=state789"));
        assert!(url.ends_with("#wechat_redirect"));
    }
}
