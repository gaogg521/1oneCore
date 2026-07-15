//! `/api/one/sso/*` routes.
//!
//! Mount behind the upstream auth middleware for admin routes; authorize
//! + callback are public (OAuth can't run with a session cookie yet).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::error::SsoError;
use crate::models::{SsoProviderConfigDto, SsoProviderKind, SsoProviderStatusDto, UpdateProviderBody};
use crate::rbac::RequireSsoAdmin;
use crate::state::OneSsoRouterState;

pub fn one_sso_public_routes(state: OneSsoRouterState) -> Router {
    Router::new()
        .route("/api/one/sso/providers", get(list_providers))
        .route("/api/one/sso/{provider}/authorize", get(authorize))
        .route("/api/one/sso/{provider}/callback", get(callback))
        .route("/api/one/sso/ldap/login", post(ldap_login))
        .with_state(state)
}

pub fn one_sso_admin_routes(state: OneSsoRouterState) -> Router {
    Router::new()
        .route("/api/one/admin/sso/providers", get(list_provider_configs))
        .route("/api/one/admin/sso/{provider}", put(upsert_provider))
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeQuery {
    #[serde(default)]
    redirect: Option<String>,
    #[serde(default)]
    desktop: Option<String>,
    #[serde(default)]
    format: Option<String>,
    /// Which OS protocol scheme the caller's desktop deep-link handler is
    /// registered under (dev and packaged builds claim different schemes so
    /// they don't fight over the same OS-level registration — see
    /// `sanitize_deep_link_scheme`). Only read when `desktop=1`.
    #[serde(default)]
    scheme: Option<String>,
}

/// Restrict the client-supplied `scheme` query param to a closed allowlist
/// before it is interpolated into the callback HTML page (as both a raw JS
/// string literal and an href attribute — see `desktop_callback_page`).
/// Unlike the rest of the deep-link params, this one is NOT run through
/// `urlencode` first, so it must never pass through anything other than one
/// of these two known-safe literals (e.g. a `javascript:` or quote-breaking
/// value). Anything unrecognized silently falls back to the production
/// scheme, matching the pre-existing hardcoded behavior for older clients
/// that don't send `scheme` at all.
fn sanitize_deep_link_scheme(raw: Option<&str>) -> &'static str {
    match raw {
        Some("aionui-dev") => "aionui-dev",
        _ => "aionui",
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeRedirectDto {
    goto: String,
    state: String,
}

async fn list_providers(
    State(state): State<OneSsoRouterState>,
) -> Result<Json<ApiResponse<Vec<SsoProviderStatusDto>>>, SsoError> {
    let rows = state.service.list_provider_status().await?;
    let dtos = rows
        .into_iter()
        .map(|(provider, enabled, configured)| SsoProviderStatusDto {
            provider,
            enabled,
            configured,
        })
        .collect();
    Ok(Json(ApiResponse::ok(dtos)))
}

async fn authorize(
    State(state): State<OneSsoRouterState>,
    Path(provider): Path<String>,
    Query(query): Query<AuthorizeQuery>,
) -> Result<Response, SsoError> {
    let provider = SsoProviderKind::parse(&provider)
        .ok_or_else(|| SsoError::BadRequest(format!("unknown provider: {provider}")))?;
    if provider == SsoProviderKind::Ldap {
        return Err(SsoError::BadRequest(
            "LDAP uses POST /api/one/sso/ldap/login, not OAuth authorize".into(),
        ));
    }

    let row = state
        .service
        .get_provider_row(provider)
        .await?
        .ok_or_else(|| SsoError::ProviderNotConfigured(provider.as_str().into()))?;
    if !row.enabled {
        return Err(SsoError::ProviderDisabled(provider.as_str().into()));
    }

    let redirect_target = query
        .redirect
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let desktop = matches!(query.desktop.as_deref(), Some("1") | Some("true"));
    let want_json = matches!(query.format.as_deref(), Some("json"));
    let deep_link_scheme = sanitize_deep_link_scheme(query.scheme.as_deref());

    let (goto, state_token) = build_authorize_goto(
        provider,
        &row,
        &state.service,
        redirect_target,
        desktop,
        deep_link_scheme,
    )
    .await?;

    if want_json {
        return Ok(Json(ApiResponse::ok(AuthorizeRedirectDto {
            goto,
            state: state_token,
        }))
        .into_response());
    }
    Ok(Redirect::to(&goto).into_response())
}

/// Build the provider-specific OAuth authorize URL + issue an OAuth state
/// token. Returns `(goto_url, state)`.
async fn build_authorize_goto(
    provider: SsoProviderKind,
    row: &crate::models::SsoProviderRow,
    service: &Arc<crate::service::SsoService>,
    redirect_target: Option<String>,
    desktop: bool,
    deep_link_scheme: &'static str,
) -> Result<(String, String), SsoError> {
    use crate::providers::dingtalk::DingtalkProviderConfig;
    use crate::providers::wecom::WecomProviderConfig;
    use crate::providers::{dingtalk::DingtalkProvider, feishu::FeishuProvider, wecom::WecomProvider};
    use crate::service::parse_feishu_config;

    let state_token = service
        .state_store()
        .issue(provider, redirect_target, desktop, deep_link_scheme)
        .await;
    let state_for_goto = state_token.clone();
    let goto = match provider {
        SsoProviderKind::Feishu => {
            let cfg = parse_feishu_config(row).ok_or_else(|| SsoError::ProviderNotConfigured("feishu".into()))?;
            FeishuProvider::build_authorize_url(&cfg, &state_for_goto)
        }
        SsoProviderKind::Dingtalk => {
            let cfg: DingtalkProviderConfig = serde_json::from_str(&row.config)
                .map_err(|e| SsoError::Internal(format!("parse dingtalk config: {e}")))?;
            DingtalkProvider::build_authorize_url(&cfg, &state_for_goto)
        }
        SsoProviderKind::Wecom => {
            let cfg: WecomProviderConfig = serde_json::from_str(&row.config)
                .map_err(|e| SsoError::Internal(format!("parse wecom config: {e}")))?;
            WecomProvider::build_authorize_url(&cfg, &state_for_goto)
        }
        SsoProviderKind::Ldap => return Err(SsoError::BadRequest("LDAP has no OAuth".into())),
    };
    Ok((goto, state_token))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

async fn callback(
    State(state): State<OneSsoRouterState>,
    Path(provider): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, SsoError> {
    let provider = SsoProviderKind::parse(&provider)
        .ok_or_else(|| SsoError::BadRequest(format!("unknown provider: {provider}")))?;
    let code = query
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(SsoError::MissingCode)?;
    let state_token = query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(SsoError::InvalidState)?;

    let entry = state
        .service
        .state_store()
        .consume(state_token)
        .await
        .ok_or(SsoError::InvalidState)?;
    if entry.provider != provider {
        return Err(SsoError::InvalidState);
    }

    let profile = run_provider_oauth(provider, &state.service, code).await?;
    let (user_id, username, _created) = state.service.resolve_or_provision_user(provider, profile).await?;
    let session = state
        .service
        .issue_session(&user_id, &username, entry.redirect_target.clone(), entry.desktop)?;

    if entry.desktop {
        // Desktop deep-link: pass token via OS protocol handler.
        // No Set-Cookie — the browser cookie jar isn't shared with the
        // desktop renderer (cross-origin cookie restrictions would block it).
        //
        // A raw redirect here left the system-browser tab stuck forever: a
        // 3xx Location to a non-http(s) scheme just makes the browser pop an
        // "open 1One Work?" prompt — it can't actually navigate the tab
        // anywhere, so whatever was on screen (often the OAuth consent page)
        // stays put with no way to tell the user it's safe to close it.
        // Render a small landing page instead: trigger the deep link via
        // script, say the login succeeded, and best-effort try to close the
        // tab (browsers may block `window.close()` on tabs they didn't open
        // via script — harmless no-op if so, the text still tells the user
        // what to do).
        let params = format!(
            "token={}&userId={}&username={}",
            urlencode(&session.token),
            urlencode(&session.user_id),
            urlencode(&session.username),
        );
        let deep_link = format!("{}://sso-callback?{params}", entry.deep_link_scheme);
        Ok(Html(desktop_callback_page(&deep_link)).into_response())
    } else {
        // Browser: Set-Cookie + redirect to the SPA.
        let target = session
            .redirect_target
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("/guid");
        let location = format!("/#{target}");
        Ok(([(header::SET_COOKIE, session.cookie)], Redirect::to(&location)).into_response())
    }
}

/// Run the provider-specific OAuth exchange + return normalized user info.
async fn run_provider_oauth(
    provider: SsoProviderKind,
    service: &Arc<crate::service::SsoService>,
    code: &str,
) -> Result<crate::providers::ProviderUserInfo, SsoError> {
    use crate::providers::dingtalk::DingtalkProviderConfig;
    use crate::providers::wecom::WecomProviderConfig;
    use crate::providers::{dingtalk::DingtalkProvider, feishu::FeishuProvider, wecom::WecomProvider};
    use crate::service::parse_feishu_config;

    let row = service
        .get_provider_row(provider)
        .await?
        .ok_or_else(|| SsoError::ProviderNotConfigured(provider.as_str().into()))?;

    match provider {
        SsoProviderKind::Feishu => {
            let cfg = parse_feishu_config(&row).ok_or_else(|| SsoError::ProviderNotConfigured("feishu".into()))?;
            let token = FeishuProvider::exchange_code(&cfg, code).await?;
            let info = FeishuProvider::fetch_user_info(&cfg, &token).await?;
            let external_id =
                FeishuProvider::resolve_external_id(&info, &cfg.external_id_field).ok_or(SsoError::IdentityMissing)?;
            let mut profile = FeishuProvider::to_provider_user_info(&info, &external_id);
            // Best-effort enrichment (job title + real department name) via
            // the Contact API — see FeishuProvider::fetch_org_profile's doc
            // comment for why this can never fail the login.
            let org_profile = FeishuProvider::fetch_org_profile(&cfg, &external_id, &cfg.external_id_field).await;
            profile.job_title = org_profile.job_title;
            profile.org_unit_path = org_profile.department_name;
            Ok(profile)
        }
        SsoProviderKind::Dingtalk => {
            let cfg: DingtalkProviderConfig = serde_json::from_str(&row.config)
                .map_err(|e| SsoError::Internal(format!("parse dingtalk config: {e}")))?;
            let token = DingtalkProvider::exchange_code(&cfg, code).await?;
            let info = DingtalkProvider::fetch_user_info(&token).await?;
            let external_id = DingtalkProvider::resolve_external_id(&info, &cfg.external_id_field)
                .ok_or(SsoError::IdentityMissing)?;
            Ok(DingtalkProvider::to_provider_user_info(&info, &external_id))
        }
        SsoProviderKind::Wecom => {
            let cfg: WecomProviderConfig = serde_json::from_str(&row.config)
                .map_err(|e| SsoError::Internal(format!("parse wecom config: {e}")))?;
            let corp_token = WecomProvider::fetch_corp_access_token(&cfg.corp_id, &cfg.secret).await?;
            let user_id = WecomProvider::fetch_user_id_by_code(&corp_token, code).await?;
            Ok(WecomProvider::to_provider_user_info(&user_id))
        }
        SsoProviderKind::Ldap => Err(SsoError::BadRequest("LDAP has no OAuth callback".into())),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LdapLoginBody {
    username: String,
    password: String,
    #[serde(default)]
    redirect: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LdapLoginDto {
    user_id: String,
    username: String,
    token: String,
}

/// LDAP is password-based (no OAuth dance): authenticate against the
/// directory, JIT-provision the local user, and answer like the upstream
/// `/login` handler — Set-Cookie for browsers plus the token in the body so
/// desktop remote-mode clients can go straight to Bearer auth.
async fn ldap_login(
    State(state): State<OneSsoRouterState>,
    Json(body): Json<LdapLoginBody>,
) -> Result<Response, SsoError> {
    let provider = SsoProviderKind::Ldap;
    let row = state
        .service
        .get_provider_row(provider)
        .await?
        .ok_or_else(|| SsoError::ProviderNotConfigured("ldap".into()))?;
    if !row.enabled {
        return Err(SsoError::ProviderDisabled("ldap".into()));
    }
    let cfg: crate::providers::ldap::LdapProviderConfig =
        serde_json::from_str(&row.config).map_err(|e| SsoError::Internal(format!("parse ldap config: {e}")))?;

    let auth = crate::providers::LdapProvider::authenticate(&cfg, &body.username, &body.password).await?;
    let profile = crate::providers::ProviderUserInfo {
        external_id: auth.external_id,
        preferred_username: body.username.trim().to_owned(),
        org_unit_path: auth.org_unit_path,
        job_title: None,
    };
    let (user_id, username, _created) = state.service.resolve_or_provision_user(provider, profile).await?;

    let redirect_target = body
        .redirect
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let session = state
        .service
        .issue_session(&user_id, &username, redirect_target, false)?;

    Ok((
        [(header::SET_COOKIE, session.cookie.clone())],
        Json(ApiResponse::ok(LdapLoginDto {
            user_id: session.user_id,
            username: session.username,
            token: session.token,
        })),
    )
        .into_response())
}

/// Admin-only: status + non-secret config values, so the settings form can
/// pre-fill fields the admin already saved instead of always starting blank.
async fn list_provider_configs(
    State(state): State<OneSsoRouterState>,
    _admin: RequireSsoAdmin,
) -> Result<Json<ApiResponse<Vec<SsoProviderConfigDto>>>, SsoError> {
    let dtos = state.service.list_provider_configs().await?;
    Ok(Json(ApiResponse::ok(dtos)))
}

async fn upsert_provider(
    State(state): State<OneSsoRouterState>,
    _admin: RequireSsoAdmin,
    Extension(user): Extension<CurrentUser>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateProviderBody>,
) -> Result<Json<ApiResponse<()>>, SsoError> {
    let provider = SsoProviderKind::parse(&provider)
        .ok_or_else(|| SsoError::BadRequest(format!("unknown provider: {provider}")))?;
    state
        .service
        .upsert_provider(provider, body.enabled, body.config, &user.id)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

/// Landing page shown in the system browser after a successful desktop SSO
/// login, right before handing off to the `aionui://` (or `aionui-dev://`,
/// see `sanitize_deep_link_scheme`) deep link. `deep_link` is built entirely
/// from `urlencode`'d segments plus an allowlisted scheme (see `callback`),
/// so it only ever contains `[A-Za-z0-9\-._~:/?=&]` — safe to interpolate
/// as-is into both the script string and (with `&` escaped) the href
/// attribute.
fn desktop_callback_page(deep_link: &str) -> String {
    let href = deep_link.replace('&', "&amp;");
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<title>1One Work</title>
</head>
<body style="font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#f5f5f7;color:#1d1d1f;">
<div style="text-align:center;">
<p style="font-size:16px;margin:0 0 8px;">登录成功，可以关闭此页面。<br>Login successful — you can close this tab.</p>
<p style="font-size:13px;color:#86868b;margin:0;">若应用没有自动打开，<a href="{href}">点击这里</a>。<br>If the app didn't open automatically, <a href="{href}">click here</a>.</p>
</div>
<script>
location.href = "{deep_link}";
setTimeout(function () {{ try {{ window.close(); }} catch (e) {{}} }}, 1200);
</script>
</body>
</html>"#
    )
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

// Touch HeaderMap so the import stays alive for future header work.
const _: fn() = || {
    let _ = std::marker::PhantomData::<HeaderMap>;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_callback_page_embeds_deep_link_for_script_redirect_and_manual_fallback() {
        let deep_link = "aionui://sso-callback?token=abc&userId=u1&username=sso_12345678";
        let page = desktop_callback_page(deep_link);

        // Script-driven redirect uses the raw deep link (safe: built entirely
        // from urlencode'd segments, no HTML/JS-sensitive characters).
        assert!(page.contains(&format!("location.href = \"{deep_link}\";")));
        // Manual fallback link HTML-escapes the query-string separators.
        assert!(page.contains("href=\"aionui://sso-callback?token=abc&amp;userId=u1&amp;username=sso_12345678\""));
        // Friendly copy in both languages so the user knows the login already
        // succeeded even if the deep link/auto-close don't fire.
        assert!(page.contains("登录成功"));
        assert!(page.contains("Login successful"));
        assert!(page.contains("window.close()"));
    }

    #[test]
    fn sanitize_deep_link_scheme_allows_only_the_known_schemes() {
        assert_eq!(sanitize_deep_link_scheme(Some("aionui-dev")), "aionui-dev");
        // Anything else — including no param at all — falls back to the
        // production scheme, matching the old hardcoded behavior.
        assert_eq!(sanitize_deep_link_scheme(Some("aionui")), "aionui");
        assert_eq!(sanitize_deep_link_scheme(None), "aionui");
        assert_eq!(sanitize_deep_link_scheme(Some("")), "aionui");
        // Injection attempts must not pass through: this value is NOT
        // urlencoded before being interpolated into the callback HTML as a
        // JS string literal and an href attribute.
        assert_eq!(sanitize_deep_link_scheme(Some("javascript")), "aionui");
        assert_eq!(sanitize_deep_link_scheme(Some("aionui\"; alert(1); //")), "aionui");
    }
}
