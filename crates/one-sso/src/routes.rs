//! `/api/one/sso/*` routes.
//!
//! Mount behind the upstream auth middleware for admin routes; authorize
//! + callback are public (OAuth can't run with a session cookie yet).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::error::SsoError;
use crate::models::{SsoProviderKind, SsoProviderStatusDto, UpdateProviderBody};
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

    let (goto, state_token) = build_authorize_goto(provider, &row, &state.service, redirect_target, desktop).await?;

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
) -> Result<(String, String), SsoError> {
    use crate::providers::{dingtalk::DingtalkProvider, feishu::FeishuProvider, wecom::WecomProvider};
    use crate::service::parse_feishu_config;
    use crate::providers::dingtalk::DingtalkProviderConfig;
    use crate::providers::wecom::WecomProviderConfig;

    let state_token = service.state_store().issue(provider, redirect_target, desktop).await;
    let state_for_goto = state_token.clone();
    let goto = match provider {
        SsoProviderKind::Feishu => {
            let cfg = parse_feishu_config(row)
                .ok_or_else(|| SsoError::ProviderNotConfigured("feishu".into()))?;
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
    let (user_id, username, _created) = state
        .service
        .resolve_or_provision_user(provider, profile)
        .await?;
    let session = state.service.issue_session(
        &user_id,
        &username,
        entry.redirect_target.clone(),
        entry.desktop,
    )?;

    if entry.desktop {
        // Desktop deep-link: pass token via OS protocol handler.
        // No Set-Cookie — the browser cookie jar isn't shared with the
        // desktop renderer (cross-origin cookie restrictions would block it).
        let params = format!(
            "token={}&userId={}&username={}",
            urlencode(&session.token),
            urlencode(&session.user_id),
            urlencode(&session.username),
        );
        Ok(Redirect::to(&format!("aionui://sso-callback?{params}")).into_response())
    } else {
        // Browser: Set-Cookie + redirect to the SPA.
        let target = session
            .redirect_target
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("/guid");
        let location = format!("/#{target}");
        Ok((
            [(header::SET_COOKIE, session.cookie)],
            Redirect::to(&location),
        )
            .into_response())
    }
}

/// Run the provider-specific OAuth exchange + return normalized user info.
async fn run_provider_oauth(
    provider: SsoProviderKind,
    service: &Arc<crate::service::SsoService>,
    code: &str,
) -> Result<crate::providers::ProviderUserInfo, SsoError> {
    use crate::providers::{dingtalk::DingtalkProvider, feishu::FeishuProvider, wecom::WecomProvider};
    use crate::service::parse_feishu_config;
    use crate::providers::dingtalk::DingtalkProviderConfig;
    use crate::providers::wecom::WecomProviderConfig;

    let row = service
        .get_provider_row(provider)
        .await?
        .ok_or_else(|| SsoError::ProviderNotConfigured(provider.as_str().into()))?;

    match provider {
        SsoProviderKind::Feishu => {
            let cfg = parse_feishu_config(&row)
                .ok_or_else(|| SsoError::ProviderNotConfigured("feishu".into()))?;
            let token = FeishuProvider::exchange_code(&cfg, code).await?;
            let info = FeishuProvider::fetch_user_info(&token).await?;
            let external_id = FeishuProvider::resolve_external_id(&info, &cfg.external_id_field)
                .ok_or(SsoError::IdentityMissing)?;
            Ok(FeishuProvider::to_provider_user_info(&info, &external_id))
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
    let cfg: crate::providers::ldap::LdapProviderConfig = serde_json::from_str(&row.config)
        .map_err(|e| SsoError::Internal(format!("parse ldap config: {e}")))?;

    let auth = crate::providers::LdapProvider::authenticate(&cfg, &body.username, &body.password).await?;
    let profile = crate::providers::ProviderUserInfo {
        external_id: auth.external_id,
        preferred_username: body.username.trim().to_owned(),
        org_unit_path: auth.org_unit_path,
    };
    let (user_id, username, _created) = state.service.resolve_or_provision_user(provider, profile).await?;

    let redirect_target = body
        .redirect
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let session = state.service.issue_session(&user_id, &username, redirect_target, false)?;

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

async fn upsert_provider(
    State(state): State<OneSsoRouterState>,
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
