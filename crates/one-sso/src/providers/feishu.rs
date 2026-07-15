//! Feishu (Lark) OAuth provider.
//!
//! Direct translation of the 1ONE TS reference (`FeishuAuthProvider.ts`),
//! kept in Rust so the crate has no Node dependency.

use serde::{Deserialize, Serialize};

use crate::error::SsoError;
use crate::providers::ProviderUserInfo;

const FEISHU_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

const DEFAULT_BASE_URL: &str = "https://open.feishu.cn";
const AUTHORIZE_URL: &str = "https://passport.feishu.cn/suite/passport/oauth/authorize";
const TOKEN_PATH: &str = "/open-apis/authen/v2/oauth/token";
const USER_INFO_PATH: &str = "/open-apis/authen/v1/user_info";
const TENANT_TOKEN_PATH: &str = "/open-apis/auth/v3/tenant_access_token/internal";
const CONTACT_USER_PATH: &str = "/open-apis/contact/v3/users";
const CONTACT_DEPARTMENT_PATH: &str = "/open-apis/contact/v3/departments";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeishuProviderConfig {
    pub app_id: String,
    pub app_secret: String,
    pub redirect_uri: String,
    #[serde(default = "default_external_id_field")]
    pub external_id_field: String,
    /// Test-only override for the Feishu API host (points at a wiremock
    /// server); never set in production, never surfaced in the admin form.
    /// Same pattern as `aionui-shell`'s LLM provider configs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl FeishuProviderConfig {
    fn base(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }
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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct FeishuContactUser {
    job_title: Option<String>,
    department_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FeishuContactUserWrapper {
    user: Option<FeishuContactUser>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FeishuDepartment {
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FeishuDepartmentWrapper {
    department: Option<FeishuDepartment>,
}

/// Result of `FeishuProvider::fetch_org_profile` — see its doc comment for
/// why every field is best-effort (`None` on any failure, never an error).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeishuOrgProfile {
    pub job_title: Option<String>,
    pub department_name: Option<String>,
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

    pub async fn exchange_code(config: &FeishuProviderConfig, code: &str) -> Result<String, SsoError> {
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

        let resp = client
            .post(format!("{}{TOKEN_PATH}", config.base()))
            .json(&body)
            .send()
            .await?;
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

    pub async fn fetch_user_info(
        config: &FeishuProviderConfig,
        access_token: &str,
    ) -> Result<FeishuUserInfo, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;

        let resp = client
            .get(format!("{}{USER_INFO_PATH}", config.base()))
            .bearer_auth(access_token)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();

        let api: FeishuApiResponse<FeishuUserInfo> = serde_json::from_value(json).unwrap_or(FeishuApiResponse {
            code: -1,
            msg: None,
            data: None,
        });
        if !status.is_success() {
            return Err(SsoError::Internal(format!("Feishu user_info failed: HTTP {status}")));
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
        fallback
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// `org_unit_path`/`job_title` are left unset here — `to_provider_user_info`
    /// only has what the lightweight `authen/v1/user_info` endpoint returns,
    /// which doesn't include department or job title. Callers fill both in
    /// afterward via `fetch_org_profile` (a separate, best-effort Contact API
    /// round trip; see its doc comment for why it's kept infallible).
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
            org_unit_path: None,
            job_title: None,
        }
    }

    async fn fetch_tenant_access_token(base: &str, app_id: &str, app_secret: &str) -> Result<String, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;
        let resp = client
            .post(format!("{base}{TENANT_TOKEN_PATH}"))
            .json(&serde_json::json!({ "app_id": app_id, "app_secret": app_secret }))
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("Feishu tenant token: HTTP {status}")));
        }
        let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = json.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(SsoError::Internal(format!("Feishu tenant token request failed: {msg}")));
        }
        json.get("tenant_access_token")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| SsoError::Internal("Feishu tenant token: missing tenant_access_token".into()))
    }

    async fn fetch_contact_user(
        base: &str,
        tenant_token: &str,
        external_id: &str,
        id_type: &str,
    ) -> Result<FeishuContactUser, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;
        let resp = client
            .get(format!("{base}{CONTACT_USER_PATH}/{external_id}"))
            .query(&[("user_id_type", id_type), ("department_id_type", "open_department_id")])
            .bearer_auth(tenant_token)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("Feishu contact user: HTTP {status}")));
        }
        let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = json.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(SsoError::Internal(format!("Feishu contact user request failed: {msg}")));
        }
        let wrapper: FeishuContactUserWrapper = json
            .get("data")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default();
        Ok(wrapper.user.unwrap_or_default())
    }

    async fn fetch_department_name(
        base: &str,
        tenant_token: &str,
        department_id: &str,
    ) -> Result<Option<String>, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;
        let resp = client
            .get(format!("{base}{CONTACT_DEPARTMENT_PATH}/{department_id}"))
            .query(&[("department_id_type", "open_department_id")])
            .bearer_auth(tenant_token)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("Feishu department: HTTP {status}")));
        }
        let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = json.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(SsoError::Internal(format!("Feishu department request failed: {msg}")));
        }
        let wrapper: FeishuDepartmentWrapper = json
            .get("data")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default();
        Ok(wrapper.department.and_then(|d| d.name))
    }

    /// Job title + primary department name via the Feishu Contact API — the
    /// lightweight `authen/v1/user_info` call `to_provider_user_info` works
    /// from doesn't carry either. Requires an app-level tenant_access_token
    /// (not the per-user OAuth token) plus Contact API scopes the admin may
    /// or may not have granted.
    ///
    /// **Never returns an error.** By the time this runs, the OAuth login
    /// itself has already succeeded — a missing scope, a transient network
    /// blip, or a person with no department assigned must not turn a
    /// successful login into a failed one. Every failure mode degrades to
    /// `FeishuOrgProfile::default()` (or a partial result: job_title present,
    /// department_name still None if only the department lookup failed).
    pub async fn fetch_org_profile(
        config: &FeishuProviderConfig,
        external_id: &str,
        external_id_field: &str,
    ) -> FeishuOrgProfile {
        let base = config.base();
        let tenant_token = match Self::fetch_tenant_access_token(base, &config.app_id, &config.app_secret).await {
            Ok(token) => token,
            Err(_) => return FeishuOrgProfile::default(),
        };
        let id_type = if external_id_field == "open_id" {
            "open_id"
        } else {
            "union_id"
        };
        let user = match Self::fetch_contact_user(base, &tenant_token, external_id, id_type).await {
            Ok(user) => user,
            Err(_) => return FeishuOrgProfile::default(),
        };
        let department_name = match user.department_ids.as_ref().and_then(|ids| ids.first()) {
            Some(department_id) => Self::fetch_department_name(base, &tenant_token, department_id)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        FeishuOrgProfile {
            job_title: user.job_title,
            department_name,
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
            .post(format!("{DEFAULT_BASE_URL}{TENANT_TOKEN_PATH}"))
            .json(&serde_json::json!({ "app_id": id, "app_secret": secret }))
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        let api: FeishuApiResponse<serde_json::Value> = serde_json::from_value(json).unwrap_or(FeishuApiResponse {
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
            base_url: None,
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

    #[test]
    fn to_provider_user_info_no_longer_derives_org_unit_path_from_tenant_key() {
        // tenant_key is the Feishu tenant/company identifier, not a
        // department — org_unit_path must come from fetch_org_profile's
        // Contact API lookup instead, or stay None.
        let info = FeishuUserInfo {
            name: Some("张三".into()),
            en_name: None,
            open_id: Some("ou_abc".into()),
            union_id: None,
            tenant_key: Some("tenant_should_not_leak".into()),
            avatar_url: None,
        };
        let p = FeishuProvider::to_provider_user_info(&info, "ou_abc");
        assert_eq!(p.org_unit_path, None);
        assert_eq!(p.job_title, None);
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn feishu_config_with_base(base: &str) -> FeishuProviderConfig {
        FeishuProviderConfig {
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
            redirect_uri: "https://example.com/api/one/sso/feishu/callback".into(),
            external_id_field: "open_id".into(),
            base_url: Some(base.to_owned()),
        }
    }

    #[tokio::test]
    async fn fetch_org_profile_returns_job_title_and_department_name_on_success() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok", "tenant_access_token": "t-token"
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-apis/contact/v3/users/ou_abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "user": { "job_title": "高级工程师", "department_ids": ["od_1"] } }
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-apis/contact/v3/departments/od_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "department": { "name": "研发中心" } }
            })))
            .mount(&mock_server)
            .await;

        let cfg = feishu_config_with_base(&mock_server.uri());
        let profile = FeishuProvider::fetch_org_profile(&cfg, "ou_abc", "open_id").await;
        assert_eq!(profile.job_title.as_deref(), Some("高级工程师"));
        assert_eq!(profile.department_name.as_deref(), Some("研发中心"));
    }

    #[tokio::test]
    async fn fetch_org_profile_degrades_to_default_when_tenant_token_fails() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 10003, "msg": "invalid app_secret"
            })))
            .mount(&mock_server)
            .await;

        let cfg = feishu_config_with_base(&mock_server.uri());
        let profile = FeishuProvider::fetch_org_profile(&cfg, "ou_abc", "open_id").await;
        assert_eq!(profile, FeishuOrgProfile::default());
    }

    #[tokio::test]
    async fn fetch_org_profile_leaves_department_name_none_without_department_ids() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok", "tenant_access_token": "t-token"
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-apis/contact/v3/users/ou_abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "user": { "job_title": "实习生", "department_ids": [] } }
            })))
            .mount(&mock_server)
            .await;

        let cfg = feishu_config_with_base(&mock_server.uri());
        let profile = FeishuProvider::fetch_org_profile(&cfg, "ou_abc", "open_id").await;
        assert_eq!(profile.job_title.as_deref(), Some("实习生"));
        assert_eq!(profile.department_name, None);
    }

    #[tokio::test]
    async fn fetch_org_profile_keeps_job_title_when_department_lookup_fails() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok", "tenant_access_token": "t-token"
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-apis/contact/v3/users/ou_abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "user": { "job_title": "高级工程师", "department_ids": ["od_missing"] } }
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-apis/contact/v3/departments/od_missing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 99991663, "msg": "department not found"
            })))
            .mount(&mock_server)
            .await;

        let cfg = feishu_config_with_base(&mock_server.uri());
        let profile = FeishuProvider::fetch_org_profile(&cfg, "ou_abc", "open_id").await;
        assert_eq!(profile.job_title.as_deref(), Some("高级工程师"));
        assert_eq!(profile.department_name, None);
    }
}
