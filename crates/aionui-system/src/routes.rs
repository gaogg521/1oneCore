#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};

use aionui_api_types::{
    ApiResponse, ClientPreferencesResponse, CreateProviderRequest, DetectProtocolRequest, EnsureNodeRuntimeRequest,
    EnsureNodeRuntimeResponse, FeedbackDiagnosticsQuery, FeedbackDiagnosticsResponse, FetchModelsAnonymousRequest,
    FetchModelsRequest, FetchModelsResponse, ProtocolDetectionResponse, ProviderResponse, SystemInfoResponse,
    SystemSettingsResponse, UpdateCheckRequest, UpdateCheckResult, UpdateClientPreferencesRequest,
    UpdateProviderRequest, UpdateSettingsRequest,
};
use aionui_auth::{CurrentUser, is_webui_proxied};
use aionui_common::ApiError;

use crate::client_pref::ClientPrefService;
use crate::diagnostics::FeedbackDiagnosticsService;
use crate::error::SystemError;
use crate::model_fetcher::ModelFetchService;
use crate::protocol::ProtocolDetectionService;
use crate::provider::ProviderService;
use crate::runtime_prepare::RuntimePrepareService;
use crate::settings::SettingsService;
use crate::version::VersionCheckService;

/// Shared state for system route handlers.
#[derive(Clone)]
pub struct SystemRouterState {
    pub settings_service: SettingsService,
    pub client_pref_service: ClientPrefService,
    pub provider_service: ProviderService,
    pub model_fetch_service: ModelFetchService,
    pub protocol_detection_service: ProtocolDetectionService,
    pub version_check_service: VersionCheckService,
    pub runtime_prepare_service: RuntimePrepareService,
    pub feedback_diagnostics_service: FeedbackDiagnosticsService,
    /// Materializes company model channels as local providers. `None` on
    /// deployments that never wire it — the endpoint then reports plainly
    /// instead of silently doing nothing.
    pub managed_provider_sync: Option<std::sync::Arc<crate::managed_provider::ManagedProviderSync>>,
    /// Holds the company's distributed content-inspection rules and the
    /// findings they produce on this machine (T4). Always present — with no
    /// rules distributed it costs a read lock and a length check per send.
    pub content_inspection: std::sync::Arc<crate::content_inspection::ContentInspectionService>,
}

impl From<SystemError> for ApiError {
    fn from(error: SystemError) -> Self {
        match error {
            SystemError::NotFound(reason) => ApiError::NotFound(reason),
            SystemError::BadRequest(reason) => ApiError::BadRequest(reason),
            SystemError::Conflict(reason) => ApiError::Conflict(reason),
            SystemError::Internal(reason) => ApiError::Internal(reason),
            SystemError::BadGateway(reason) => ApiError::BadGateway(reason),
            SystemError::Timeout(reason) => ApiError::Timeout(reason),
            SystemError::UnprocessableEntity(reason) => ApiError::UnprocessableEntity(reason),
        }
    }
}

/// Build the system router (settings + client prefs + providers + system).
///
/// All routes require authentication (applied by the caller).
///
/// Endpoints:
/// - `GET  /api/settings`                    — get all backend settings
/// - `PATCH /api/settings`                   — partial update backend settings
/// - `GET  /api/settings/client`             — get client preferences
/// - `PUT  /api/settings/client`             — batch update client preferences
/// - `GET  /api/providers`                   — list all providers
/// - `POST /api/providers`                   — create a provider
/// - `PUT  /api/providers/:id`               — update a provider
/// - `DELETE /api/providers/:id`             — delete a provider
/// - `POST /api/providers/:id/models`        — fetch models from remote API
/// - `POST /api/providers/fetch-models`      — fetch models anonymously (pre-create preview)
/// - `POST /api/providers/detect-protocol`   — detect API protocol
/// - `GET  /api/system/info`                 — system directory & platform info
/// - `POST /api/system/check-update`         — check GitHub for new versions
/// - `POST /api/system/ensure-node-runtime`  — prepare managed Node runtime
/// - `GET  /api/system/diagnostics/feedback-report` — collect sanitized feedback diagnostics
pub fn system_routes(state: SystemRouterState) -> Router {
    Router::new()
        .route("/api/settings", get(get_settings).patch(update_settings))
        .route(
            "/api/settings/client",
            get(get_client_preferences).put(update_client_preferences),
        )
        .route("/api/providers", get(list_providers).post(create_provider))
        // Literal-segment routes must register BEFORE the `/{id}` routes so
        // axum matches the literals instead of treating "detect-protocol" /
        // "fetch-models" as a provider id.
        .route("/api/providers/detect-protocol", post(detect_protocol))
        .route("/api/providers/fetch-models", post(fetch_models_anonymous))
        .route("/api/providers/{id}", delete(delete_provider).put(update_provider))
        .route("/api/providers/{id}/models", post(fetch_models))
        .route("/api/providers/sync-model-channels", post(sync_model_channels))
        // Content inspection (T4). Rules come down from the governance backend
        // via the renderer; findings go back up the same way.
        .route("/api/content-inspection/rules", post(set_inspection_rules))
        .route("/api/content-inspection/findings", post(drain_inspection_findings))
        .route("/api/system/info", get(get_system_info))
        .route("/api/system/check-update", post(check_update))
        .route("/api/system/ensure-node-runtime", post(ensure_node_runtime))
        .route("/api/system/diagnostics/feedback-report", get(get_feedback_diagnostics))
        .with_state(state)
}

/// Backwards-compatible alias — delegates to `system_routes`.
pub fn settings_routes(state: SystemRouterState) -> Router {
    system_routes(state)
}

// ===========================================================================
// Settings handlers
// ===========================================================================

async fn get_settings(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<SystemSettingsResponse>>, ApiError> {
    let settings = state
        .settings_service
        .get_settings(&user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn get_feedback_diagnostics(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<FeedbackDiagnosticsQuery>,
) -> Result<Json<ApiResponse<FeedbackDiagnosticsResponse>>, ApiError> {
    let diagnostics = state
        .feedback_diagnostics_service
        .collect(&user.id, query)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(diagnostics)))
}

async fn update_settings(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<UpdateSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SystemSettingsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let settings = state
        .settings_service
        .update_settings(&user.id, req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(settings)))
}

// ===========================================================================
// Client preferences handlers
// ===========================================================================

#[derive(Debug, serde::Deserialize, Default)]
struct ClientPrefQuery {
    keys: Option<String>,
}

async fn get_client_preferences(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ClientPrefQuery>,
) -> Result<Json<ApiResponse<ClientPreferencesResponse>>, ApiError> {
    let keys_filter: Option<Vec<String>> = query.keys.map(|k| {
        k.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let key_refs: Option<Vec<&str>> = keys_filter.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect());

    let prefs = state
        .client_pref_service
        .get_preferences(&user.id, key_refs.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(prefs)))
}

async fn update_client_preferences(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<UpdateClientPreferencesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .client_pref_service
        .update_preferences(&user.id, req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

// ===========================================================================
// Provider handlers
// ===========================================================================

/// The account that owns this deployment's provider credentials.
///
/// ⚠️ Fork decision, deliberately diverging from upstream. `7f8ed6c5` gave
/// `providers` a `user_id` and scoped every read to the calling account. That
/// is right for a personal multi-account install and wrong for how this fork is
/// deployed: one machine runs the backend, the operator configures the
/// company's keys once, and everyone else reaching it is a member of their org
/// who is meant to USE those models, not to own a separate set. Per-user
/// scoping would have shown every existing member an empty model list on
/// upgrade.
///
/// So the column exists and the plumbing is upstream's, but every provider
/// handler pins the scope here — the table stays deployment-global, as it was
/// before the sync. Members share the operator's providers; what they cannot do
/// is read the key out (see `may_see_provider_secrets`).
///
/// Per-member credentials are a different feature and already have their own
/// path: enterprise model channels materialize a `managed_by='enterprise'` row
/// per member with a revocable channel token (migration 041).
const PROVIDER_CREDENTIAL_OWNER: &str = "system_default_user";

/// Whether this caller may see provider API keys in plaintext.
///
/// Two ways to qualify, and both mean "this is the operator":
/// - the request did not come through the WebUI proxy, i.e. it is the desktop
///   app talking to its own co-located backend from this machine;
/// - or it did, but the session resolves to the operator's own account — the
///   operator using the WebUI from a browser is still the operator.
///
/// An org member authenticated over the WebUI is neither, and gets the key
/// masked. They can still see that a key is configured, pick models, and use
/// the provider; what they cannot do is walk away with the operator's
/// credential — which is billable and reusable anywhere.
///
/// `user` is optional because these routes are mounted with the auth middleware
/// by `aionui-app` but exercised without it in this crate's own tests. A
/// proxied request that somehow arrives with no resolved identity is treated as
/// "not the operator" rather than trusted — the only way to reach the plaintext
/// branch without an identity is to not be coming through the proxy at all,
/// which means the desktop app on this machine.
fn may_see_provider_secrets(headers: &HeaderMap, user: Option<&CurrentUser>) -> bool {
    if !is_webui_proxied(headers) {
        return true;
    }
    user.is_some_and(|current| current.id == PROVIDER_CREDENTIAL_OWNER)
}

/// Replace the key with a fixed marker.
///
/// Deliberately not a prefix/suffix hint: a masked-but-recognisable key still
/// leaks which credential is in use across a whole org, and nothing in the UI
/// needs to tell two keys apart — the provider already has a name.
fn redact_provider_secret(mut provider: ProviderResponse) -> ProviderResponse {
    if !provider.api_key.is_empty() {
        provider.api_key = "***".to_string();
    }
    provider
}

/// `user` stays OPTIONAL: a request that reached us through the WebUI proxy
/// without a resolvable identity must still get the list — with every key
/// redacted — rather than a hard rejection. Making the extractor mandatory
/// turns that case into an empty 500 and silently drops the redaction path
/// this endpoint exists to exercise. Falls back to the local default user for
/// the scope query, which is who an unauthenticated desktop request is.
async fn list_providers(
    State(state): State<SystemRouterState>,
    user: Option<Extension<CurrentUser>>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<Vec<ProviderResponse>>>, ApiError> {
    // The caller's own id. It records WHO on create and is otherwise inert:
    // scoping is enforced once, in the repository — see
    // `IProviderRepository::list` for why `providers` stays deployment-global.
    let scope_user_id = user.as_deref().map_or(PROVIDER_CREDENTIAL_OWNER, |u| u.id.as_str());
    let providers = state
        .provider_service
        .list(scope_user_id)
        .await
        .map_err(ApiError::from)?;
    if may_see_provider_secrets(&headers, user.as_deref()) {
        return Ok(Json(ApiResponse::ok(providers)));
    }
    Ok(Json(ApiResponse::ok(
        providers.into_iter().map(redact_provider_secret).collect(),
    )))
}

async fn create_provider(
    State(state): State<SystemRouterState>,
    user: Option<Extension<CurrentUser>>,
    headers: HeaderMap,
    body: Result<Json<CreateProviderRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ProviderResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    // The caller's own id. It records WHO on create and is otherwise inert:
    // scoping is enforced once, in the repository — see
    // `IProviderRepository::list` for why `providers` stays deployment-global.
    let scope_user_id = user.as_deref().map_or(PROVIDER_CREDENTIAL_OWNER, |u| u.id.as_str());
    let provider = state
        .provider_service
        .create(scope_user_id, req)
        .await
        .map_err(ApiError::from)?;
    // Echoing the key back would hand it to a caller the list endpoint would
    // have redacted for.
    let provider = if may_see_provider_secrets(&headers, user.as_deref()) {
        provider
    } else {
        redact_provider_secret(provider)
    };
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(provider))))
}

async fn update_provider(
    State(state): State<SystemRouterState>,
    user: Option<Extension<CurrentUser>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<UpdateProviderRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProviderResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    // The caller's own id. It records WHO on create and is otherwise inert:
    // scoping is enforced once, in the repository — see
    // `IProviderRepository::list` for why `providers` stays deployment-global.
    let scope_user_id = user.as_deref().map_or(PROVIDER_CREDENTIAL_OWNER, |u| u.id.as_str());
    let provider = state
        .provider_service
        .update(scope_user_id, &id, req)
        .await
        .map_err(ApiError::from)?;
    let provider = if may_see_provider_secrets(&headers, user.as_deref()) {
        provider
    } else {
        redact_provider_secret(provider)
    };
    Ok(Json(ApiResponse::ok(provider)))
}

async fn delete_provider(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .provider_service
        .delete(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncModelChannelsBody {
    #[serde(default)]
    channels: Vec<crate::managed_provider::ManagedChannelPayload>,
    /// True only when the caller's fetch from the company server succeeded, so
    /// this really is the complete set. False leaves existing managed rows
    /// alone — a server that could not be reached must not wipe a member's
    /// working setup.
    #[serde(default)]
    authoritative: bool,
}

/// Materialize the company's model channels as local providers.
///
/// Local-only by design: the channel list and the member's tokens are fetched
/// by the renderer (which knows the company server's address and holds the
/// session), and handed here to be written to this machine's provider table.
async fn sync_model_channels(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<SyncModelChannelsBody>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::managed_provider::ManagedChannelSyncReport>>, ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    let Some(sync) = state.managed_provider_sync.as_ref() else {
        return Err(ApiError::BadRequest(
            "model channel sync is not available on this deployment".into(),
        ));
    };
    let report = sync
        .sync(&user.id, &body.channels, body.authoritative)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(report)))
}

#[derive(serde::Deserialize)]
struct SetInspectionRulesBody {
    #[serde(default)]
    rules: Vec<aionui_common::dlp::DlpRule>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetInspectionRulesReport {
    active_rules: usize,
}

/// Replace the locally-enforced rule set.
///
/// Authoritative on purpose: an admin deleting a rule has to actually stop it
/// being enforced, and a merge would keep deleted rules alive on every machine
/// that ever saw them.
async fn set_inspection_rules(
    State(state): State<SystemRouterState>,
    body: Result<Json<SetInspectionRulesBody>, JsonRejection>,
) -> Result<Json<ApiResponse<SetInspectionRulesReport>>, ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    let active_rules = state.content_inspection.set_rules(body.rules);
    Ok(Json(ApiResponse::ok(SetInspectionRulesReport { active_rules })))
}

/// Hand over the findings buffered since the last call, and forget them.
///
/// POST rather than GET because it mutates: the caller takes ownership of
/// delivering them upstream.
async fn drain_inspection_findings(
    State(state): State<SystemRouterState>,
) -> Result<Json<ApiResponse<Vec<crate::content_inspection::PendingFinding>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.content_inspection.drain_findings())))
}

async fn fetch_models(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<FetchModelsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FetchModelsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .model_fetch_service
        .fetch_models(&user.id, &id, &req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn fetch_models_anonymous(
    State(state): State<SystemRouterState>,
    body: Result<Json<FetchModelsAnonymousRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FetchModelsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .model_fetch_service
        .fetch_models_anonymous(&req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn detect_protocol(
    State(state): State<SystemRouterState>,
    body: Result<Json<DetectProtocolRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProtocolDetectionResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .protocol_detection_service
        .detect_protocol(&req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

// ===========================================================================
// System info & version check handlers
// ===========================================================================

async fn get_system_info() -> Json<ApiResponse<SystemInfoResponse>> {
    let info = crate::sysinfo::get_system_info();
    Json(ApiResponse::ok(info))
}

async fn check_update(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateCheckRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<UpdateCheckResult>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .version_check_service
        .check_update(&req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn ensure_node_runtime(
    State(state): State<SystemRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<EnsureNodeRuntimeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<EnsureNodeRuntimeResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .runtime_prepare_service
        .ensure_node_runtime_for_user(&user.id, req.scope)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}
