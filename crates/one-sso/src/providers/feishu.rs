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
/// Users of one department, paged. Appended to `CONTACT_USER_PATH`.
const CONTACT_USERS_BY_DEPARTMENT_SEGMENT: &str = "find_by_department";
/// Descendants of one department, paged. Appended after a department id.
const CONTACT_DEPARTMENT_CHILDREN_SEGMENT: &str = "children";
/// Feishu's id for "the root of the company tree".
pub(crate) const FEISHU_ROOT_DEPARTMENT_ID: &str = "0";
/// Upper bound Feishu accepts for `page_size` on the two list endpoints.
const CONTACT_PAGE_SIZE: &str = "50";
/// Refuse to loop forever if `has_more`/`page_token` never terminate. At 50 per
/// page this is 50k departments or people — far past any real tenant, but a
/// bounded number rather than a hung sync holding a DB connection.
const MAX_PAGES: usize = 1_000;

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

// ── Bulk directory pull (T6) ────────────────────────────────────────────────
//
// ⚠️ **The wire shapes below are written from Feishu's public documentation,
// not from captured traffic.** Everything else in this file was translated from
// a working reference implementation; these two endpoints have never been
// called by this codebase. The consequence, stated plainly because it is easy
// to forget once the tests go green: the wiremock tests around this prove that
// our paging and reconcile logic is correct *against the shape we assumed* —
// they cannot prove Feishu actually sends that shape. Only one run against a
// real tenant can, and until that happens this is unverified.
//
// The structs are kept deliberately small and every field optional so a wrong
// guess degrades to a missing value rather than a failed parse, and so that
// correcting the shape after a real run is a local edit.

/// One page of a Feishu list endpoint.
///
/// The explicit `bound` is load-bearing: `#[serde(default)]` on a field whose
/// type mentions `T` makes serde's derive infer `T: Default` as well, which the
/// item types have no reason to satisfy.
#[derive(Debug, Clone, Deserialize)]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
struct FeishuPage<T> {
    #[serde(default)]
    items: Option<Vec<T>>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    has_more: Option<bool>,
}

// Hand-written so the item type does not have to be `Default` — `derive` would
// add that bound and force it onto every caller for no reason. An empty page is
// the right fallback when `data` is absent or unparseable.
impl<T> Default for FeishuPage<T> {
    fn default() -> Self {
        Self {
            items: None,
            page_token: None,
            has_more: None,
        }
    }
}

/// A department as returned by the children listing.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct FeishuListedDepartment {
    open_department_id: Option<String>,
    department_id: Option<String>,
    parent_department_id: Option<String>,
    name: Option<String>,
}

/// A person as returned by the per-department user listing.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct FeishuListedUser {
    open_id: Option<String>,
    union_id: Option<String>,
    name: Option<String>,
    job_title: Option<String>,
    department_ids: Option<Vec<String>>,
    /// Feishu marks leavers here rather than removing them from the directory.
    /// Treated as authoritative when present — see `DirectoryPerson::active`.
    status: Option<FeishuUserStatus>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct FeishuUserStatus {
    /// The only status we act on. `is_activated` is deliberately NOT read: a
    /// not-yet-activated account is a new hire who has not signed in, and
    /// treating that as a departure would offboard people on their first day.
    is_resigned: Option<bool>,
}

/// One department in the company tree, provider-neutral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryDepartment {
    pub external_id: String,
    /// `None` for a top-level department.
    pub parent_external_id: Option<String>,
    pub name: String,
}

/// One person in the company directory, provider-neutral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPerson {
    /// The id this person is matched on. Same field the SSO identity binds by,
    /// so a directory row can be tied back to a local account.
    pub external_id: String,
    pub name: Option<String>,
    pub job_title: Option<String>,
    pub department_external_ids: Vec<String>,
    /// `false` when the IdP says they have left. A resigned person is still
    /// *in* the directory, so absence is not the only departure signal.
    pub active: bool,
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
            // Feishu tenant_key is the company identifier — used to bind/auto-join
            // the SSO enterprise tenant (not a department; org_unit_path comes
            // from fetch_org_profile).
            org_external_id: info
                .tenant_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
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

    /// Mint an app-level token. `pub(crate)` so the directory sync can hold one
    /// for a whole run instead of minting per call, which is what the
    /// single-user path does (fine for one login, wasteful across hundreds of
    /// paged requests).
    pub(crate) async fn tenant_access_token(config: &FeishuProviderConfig) -> Result<String, SsoError> {
        Self::fetch_tenant_access_token(config.base(), &config.app_id, &config.app_secret).await
    }

    /// GET one page of a Feishu list endpoint and unwrap the `{code,msg,data}`
    /// envelope. Factored out because the three single-entity fetches above
    /// each hand-roll this and a fourth copy is where they start to drift.
    async fn get_page<T: serde::de::DeserializeOwned>(
        url: &str,
        tenant_token: &str,
        query: &[(&str, &str)],
        what: &str,
    ) -> Result<FeishuPage<T>, SsoError> {
        let client = reqwest::Client::builder()
            .timeout(FEISHU_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))?;
        let resp = client.get(url).query(query).bearer_auth(tenant_token).send().await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("Feishu {what}: HTTP {status}")));
        }
        let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = json.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(SsoError::Internal(format!("Feishu {what} request failed: {msg}")));
        }
        Ok(json
            .get("data")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default())
    }

    /// Walk every page of one list endpoint, collecting items.
    ///
    /// Any page failing aborts the whole walk with an error rather than
    /// returning what it got so far. That is the important half: a caller that
    /// received a partial directory and treated it as complete would conclude
    /// that everyone on the missing pages had left the company.
    async fn collect_pages<T: serde::de::DeserializeOwned>(
        url: &str,
        tenant_token: &str,
        base_query: &[(&str, &str)],
        what: &str,
    ) -> Result<Vec<T>, SsoError> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, &str)> = base_query.to_vec();
            query.push(("page_size", CONTACT_PAGE_SIZE));
            if let Some(token) = page_token.as_deref() {
                query.push(("page_token", token));
            }
            let page: FeishuPage<T> = Self::get_page(url, tenant_token, &query, what).await?;
            out.extend(page.items.unwrap_or_default());

            // Feishu signals "more" two ways; require both to agree before
            // asking again, so a stale token with has_more=false stops.
            match (page.has_more.unwrap_or(false), page.page_token) {
                (true, Some(token)) if !token.is_empty() => page_token = Some(token),
                _ => return Ok(out),
            }
        }
        Err(SsoError::Internal(format!(
            "Feishu {what}: pagination did not terminate after {MAX_PAGES} pages"
        )))
    }

    /// Every department in the company, flattened.
    ///
    /// Asks for the root's descendants recursively rather than walking the tree
    /// ourselves: one paged call instead of one call per node, which for a
    /// thousand-department tenant is the difference between a sync that
    /// finishes and one that gets rate-limited.
    pub async fn fetch_all_departments(
        config: &FeishuProviderConfig,
        tenant_token: &str,
    ) -> Result<Vec<DirectoryDepartment>, SsoError> {
        let url = format!(
            "{}{CONTACT_DEPARTMENT_PATH}/{FEISHU_ROOT_DEPARTMENT_ID}/{CONTACT_DEPARTMENT_CHILDREN_SEGMENT}",
            config.base()
        );
        let raw: Vec<FeishuListedDepartment> = Self::collect_pages(
            &url,
            tenant_token,
            &[("department_id_type", "open_department_id"), ("fetch_child", "true")],
            "department children",
        )
        .await?;

        Ok(raw
            .into_iter()
            .filter_map(|d| {
                // Prefer the open id: it is what the single-user path already
                // stores, so both halves agree on identity.
                let external_id = d.open_department_id.or(d.department_id)?;
                let external_id = external_id.trim().to_owned();
                if external_id.is_empty() {
                    return None;
                }
                let parent = d
                    .parent_department_id
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty() && p != FEISHU_ROOT_DEPARTMENT_ID);
                Some(DirectoryDepartment {
                    external_id,
                    parent_external_id: parent,
                    name: d.name.unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Every person in one department.
    ///
    /// `id_type` must match what SSO logins bind by (`external_id_field`), or
    /// directory rows and local accounts will never line up.
    pub async fn fetch_department_members(
        config: &FeishuProviderConfig,
        tenant_token: &str,
        department_id: &str,
        id_type: &str,
    ) -> Result<Vec<DirectoryPerson>, SsoError> {
        let url = format!(
            "{}{CONTACT_USER_PATH}/{CONTACT_USERS_BY_DEPARTMENT_SEGMENT}",
            config.base()
        );
        let raw: Vec<FeishuListedUser> = Self::collect_pages(
            &url,
            tenant_token,
            &[
                ("department_id", department_id),
                ("user_id_type", id_type),
                ("department_id_type", "open_department_id"),
            ],
            "department members",
        )
        .await?;

        Ok(raw
            .into_iter()
            .filter_map(|u| {
                let external_id = if id_type == "open_id" {
                    u.open_id.clone()
                } else {
                    u.union_id.clone()
                }?;
                let external_id = external_id.trim().to_owned();
                if external_id.is_empty() {
                    return None;
                }
                // Absent status = present and working. Only an explicit
                // `is_resigned` marks someone as gone; `is_activated == false`
                // is a not-yet-onboarded account, not a departure.
                let active = !u.status.as_ref().and_then(|s| s.is_resigned).unwrap_or(false);
                Some(DirectoryPerson {
                    external_id,
                    name: u.name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
                    job_title: u.job_title.map(|j| j.trim().to_owned()).filter(|j| !j.is_empty()),
                    department_external_ids: u.department_ids.unwrap_or_default(),
                    active,
                })
            })
            .collect())
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
        // It is the company id though — that's what binds/auto-joins the
        // enterprise tenant.
        assert_eq!(p.org_external_id.as_deref(), Some("tenant_should_not_leak"));
    }

    #[test]
    fn to_provider_user_info_captures_tenant_key_as_the_company_id() {
        let info = FeishuUserInfo {
            name: Some("赵高".into()),
            en_name: None,
            open_id: Some("ou_abc".into()),
            union_id: None,
            tenant_key: Some("tenant_huanle".into()),
            avatar_url: None,
        };
        let p = FeishuProvider::to_provider_user_info(&info, "ou_abc");
        assert_eq!(p.org_external_id.as_deref(), Some("tenant_huanle"));
    }

    #[test]
    fn to_provider_user_info_leaves_company_id_none_without_tenant_key() {
        // Blank/absent tenant_key must not produce an empty-string company id —
        // that would match nothing and could bind an enterprise to "".
        for tenant_key in [None, Some("   ".to_string())] {
            let info = FeishuUserInfo {
                name: Some("赵高".into()),
                en_name: None,
                open_id: Some("ou_abc".into()),
                union_id: None,
                tenant_key,
                avatar_url: None,
            };
            let p = FeishuProvider::to_provider_user_info(&info, "ou_abc");
            assert_eq!(p.org_external_id, None);
        }
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

    // ── Bulk directory pull (T6) ────────────────────────────────────────────
    //
    // ⚠️ These stub the shape we *believe* Feishu returns (see the note above
    // `FeishuPage`). They prove our paging, id selection and error propagation
    // are right given that shape; they cannot prove the shape. Read them as
    // "our half is correct", not "the integration works".

    const DEPT_CHILDREN_PATH: &str = "/open-apis/contact/v3/departments/0/children";
    const USERS_BY_DEPT_PATH: &str = "/open-apis/contact/v3/users/find_by_department";

    async fn mount_tenant_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok", "tenant_access_token": "t-tenant"
            })))
            .mount(server)
            .await;
    }

    /// Every page must be walked. A sync that stopped at page one would report
    /// everyone after it as having left the company.
    #[tokio::test]
    async fn fetch_all_departments_follows_every_page() {
        let server = MockServer::start().await;
        mount_tenant_token(&server).await;

        // wiremock matches most-recently-mounted first, so mount the
        // second page (guarded by its token) before the unguarded first.
        Mock::given(method("GET"))
            .and(path(DEPT_CHILDREN_PATH))
            .and(wiremock::matchers::query_param("page_token", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "has_more": false, "items": [
                    { "open_department_id": "od_2", "parent_department_id": "od_1", "name": "后端组" }
                ]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(DEPT_CHILDREN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "has_more": true, "page_token": "p2", "items": [
                    { "open_department_id": "od_1", "parent_department_id": "0", "name": "研发中心" }
                ]}
            })))
            .mount(&server)
            .await;

        let cfg = feishu_config_with_base(&server.uri());
        let departments = FeishuProvider::fetch_all_departments(&cfg, "t-tenant").await.unwrap();

        assert_eq!(departments.len(), 2, "both pages must be collected");
        assert_eq!(departments[0].external_id, "od_1");
        // The root id is not a real parent — a top-level department must come
        // back parentless or the tree cannot be assembled.
        assert_eq!(departments[0].parent_external_id, None);
        assert_eq!(departments[1].parent_external_id.as_deref(), Some("od_1"));
    }

    /// A failed page must abort the walk. Returning what we got so far is the
    /// dangerous alternative: downstream reads absence as departure.
    #[tokio::test]
    async fn a_failing_page_aborts_rather_than_returning_a_partial_list() {
        let server = MockServer::start().await;
        mount_tenant_token(&server).await;

        Mock::given(method("GET"))
            .and(path(DEPT_CHILDREN_PATH))
            .and(wiremock::matchers::query_param("page_token", "p2"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(DEPT_CHILDREN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "has_more": true, "page_token": "p2", "items": [
                    { "open_department_id": "od_1", "name": "研发中心" }
                ]}
            })))
            .mount(&server)
            .await;

        let cfg = feishu_config_with_base(&server.uri());
        let result = FeishuProvider::fetch_all_departments(&cfg, "t-tenant").await;
        assert!(result.is_err(), "a partial directory must not be returned as success");
    }

    /// A non-zero `code` in a 200 body is Feishu's real failure channel.
    #[tokio::test]
    async fn a_non_zero_code_is_an_error_even_with_http_200() {
        let server = MockServer::start().await;
        mount_tenant_token(&server).await;
        Mock::given(method("GET"))
            .and(path(DEPT_CHILDREN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 99991663, "msg": "app permission denied"
            })))
            .mount(&server)
            .await;

        let cfg = feishu_config_with_base(&server.uri());
        let err = FeishuProvider::fetch_all_departments(&cfg, "t-tenant")
            .await
            .expect_err("code != 0 must be an error");
        assert!(format!("{err}").contains("app permission denied"));
    }

    /// The id we key on must match what SSO logins bind by, or directory rows
    /// and local accounts never line up.
    #[tokio::test]
    async fn members_are_keyed_by_the_configured_id_field() {
        let server = MockServer::start().await;
        mount_tenant_token(&server).await;
        Mock::given(method("GET"))
            .and(path(USERS_BY_DEPT_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "has_more": false, "items": [
                    { "open_id": "ou_1", "union_id": "on_1", "name": "张三", "job_title": "工程师",
                      "department_ids": ["od_1"] }
                ]}
            })))
            .mount(&server)
            .await;

        let cfg = feishu_config_with_base(&server.uri());

        let by_open = FeishuProvider::fetch_department_members(&cfg, "t-tenant", "od_1", "open_id")
            .await
            .unwrap();
        assert_eq!(by_open[0].external_id, "ou_1");
        assert_eq!(by_open[0].name.as_deref(), Some("张三"));
        assert!(by_open[0].active, "no status block means present, not gone");

        let by_union = FeishuProvider::fetch_department_members(&cfg, "t-tenant", "od_1", "union_id")
            .await
            .unwrap();
        assert_eq!(by_union[0].external_id, "on_1");
    }

    /// Feishu keeps leavers in the directory and flags them, so absence is not
    /// the only departure signal — `is_resigned` has to be read.
    #[tokio::test]
    async fn a_resigned_person_comes_back_inactive() {
        let server = MockServer::start().await;
        mount_tenant_token(&server).await;
        Mock::given(method("GET"))
            .and(path(USERS_BY_DEPT_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "msg": "ok",
                "data": { "has_more": false, "items": [
                    { "open_id": "ou_gone", "name": "李四", "status": { "is_resigned": true } },
                    // Not yet activated is NOT a departure — they just haven't
                    // onboarded. Treating it as one would remove new hires.
                    { "open_id": "ou_new", "name": "王五", "status": { "is_activated": false } }
                ]}
            })))
            .mount(&server)
            .await;

        let cfg = feishu_config_with_base(&server.uri());
        let people = FeishuProvider::fetch_department_members(&cfg, "t-tenant", "od_1", "open_id")
            .await
            .unwrap();

        assert!(!people[0].active, "is_resigned must mark them gone");
        assert!(people[1].active, "not-yet-activated is not a departure");
    }
}
