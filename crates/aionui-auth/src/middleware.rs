#![allow(clippy::disallowed_types)]

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_db::{IUserRepository, UserStatus, UserType};

use crate::JwtService;
use crate::extract::extract_token_from_headers;

/// Header the WebUI reverse proxy stamps on every request it forwards.
///
/// The desktop's co-located backend runs with `--local`, which historically
/// meant "nobody has to log in". But the SAME backend is what the WebUI serves
/// to browsers, and with "允许远程访问" on, that listener is bound to `0.0.0.0`.
/// So `--local` was silently granting the operator's identity to anyone on the
/// network — no credential at all.
///
/// The peer address cannot answer "was this remote?" here: the proxy splices
/// over loopback, so by the time a request reaches this process every peer
/// looks local. The proxy is the only layer that still knows, so it tells us.
///
/// Trust model: the backend listener is bound to loopback, so forging this
/// header requires already running code on the machine — and the header can
/// only ever make the check *stricter*, never weaker. The proxy sets it
/// unconditionally (overwriting any client-supplied copy), so a remote client
/// cannot strip it.
pub const WEBUI_PROXY_HEADER: &str = "x-aionui-forwarded-origin";

/// Value paired with [`WEBUI_PROXY_HEADER`].
pub const WEBUI_PROXY_VALUE: &str = "webui";

/// Whether this request arrived through the WebUI reverse proxy.
///
/// When true, "local mode" must not be treated as "trusted operator": the
/// caller is a browser that may be on another machine entirely.
pub fn is_webui_proxied(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(WEBUI_PROXY_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(WEBUI_PROXY_VALUE))
}

/// Header the WebUI reverse proxy stamps with the real remote-peer IP of the
/// TCP connection that reached it.
///
/// Companion to [`WEBUI_PROXY_HEADER`], same trust model: only meaningful
/// (and only trusted) when [`is_webui_proxied`] is true. Necessary for the
/// same reason — the proxy splices every request over loopback, so
/// `ConnectInfo` always shows 127.0.0.1 for a proxied request regardless of
/// who actually connected. For a non-proxied request (standalone server with
/// no reverse proxy in front, or the desktop's direct-to-backend traffic),
/// `ConnectInfo` is the real peer and this header is not consulted.
pub const CLIENT_IP_HEADER: &str = "x-aionui-client-ip";

/// Resolve the caller's real IP for IP-allowlist enforcement.
///
/// Trusts [`CLIENT_IP_HEADER`] only when the request arrived through the
/// WebUI proxy (which overwrites any client-supplied copy — see
/// [`CLIENT_IP_HEADER`]); otherwise falls back to the direct TCP peer via
/// `ConnectInfo`, which is only present when the server was started with
/// `into_make_service_with_connect_info` (the production `axum::serve` call
/// in `cmd_server.rs`). Returns `None` when neither source is available —
/// callers must treat that as "cannot verify", not "allowed".
fn resolve_caller_ip(request: &Request) -> Option<IpAddr> {
    if is_webui_proxied(request.headers()) {
        return request
            .headers()
            .get(CLIENT_IP_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<IpAddr>().ok());
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

/// Per-project-group IP allowlist check, bridged from `one-platform` (same
/// layer as `aionui-auth`, so the dependency runs through this trait rather
/// than a direct crate dependency — same arrangement as
/// `one_org::CredentialRevoker` / `one_enterprise::CompanyDisbandCascade`).
///
/// Implementations resolve the caller's active project group from `user_id`
/// and check `ip` against that group's configured allowlist. `ip` is `None`
/// when the caller's real IP could not be resolved (see
/// [`resolve_caller_ip`]) — implementations own the fail-open/fail-closed
/// decision for that case, and must get it right per caller:
///
/// - A caller with nothing to check (no project group, or the group has no
///   allowlist enabled) must return `Ok(true)` regardless of `ip` — the vast
///   majority of callers, including every personal-edition install and every
///   test harness that drives the router directly without a real `ConnectInfo`.
/// - A caller whose group DOES have enforcement on must return `Ok(false)`
///   when `ip` is `None` — "cannot verify" must not be treated as "allowed"
///   once enforcement is actually active.
#[async_trait]
pub trait IpAllowlistGate: Send + Sync {
    async fn is_allowed(&self, user_id: &str, ip: Option<IpAddr>) -> Result<bool, String>;
}

/// Default when nothing is wired: every caller is allowed. Used by
/// standalone tests and any build that never mounts one-platform.
pub struct NoopIpAllowlistGate;

#[async_trait]
impl IpAllowlistGate for NoopIpAllowlistGate {
    async fn is_allowed(&self, _user_id: &str, _ip: Option<IpAddr>) -> Result<bool, String> {
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIdentityMode {
    Local,
    UserSession,
    AionPro,
}

/// Header carrying the conversation-runtime helper token minted by the backend
/// and injected into agent subprocess environments as `AIONUI_RUNTIME_TOKEN`.
pub const RUNTIME_TOKEN_HEADER: &str = "x-aionui-runtime-token";
/// Header carrying the acting user id asserted by the helper CLI.
pub const RUNTIME_USER_ID_HEADER: &str = "x-aionui-user-id";
/// Header carrying the conversation id the helper CLI runs inside.
pub const RUNTIME_CONVERSATION_ID_HEADER: &str = "x-aionui-conversation-id";

/// Port for validating conversation-runtime helper tokens.
///
/// Implemented in the composition layer over the agent runtime's token
/// service; `aionui-auth` must not depend on `aionui-ai-agent` directly.
/// A verifier must confirm the token is a live, conversation-helper-scoped
/// token bound to exactly this (user_id, conversation_id) pair.
pub trait IRuntimeTokenVerifier: Send + Sync {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool;
}

/// Authenticated user injected into request extensions by the auth middleware.
///
/// Route handlers extract this from `request.extensions()` to identify
/// the current user.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// User ID from the database.
    pub id: String,
    /// Username.
    pub username: String,
    /// Internal identity source for the current user.
    pub user_type: UserType,
    /// Current account status. Authenticated requests only receive active users.
    pub status: UserStatus,
}

impl CurrentUser {
    pub fn local_default() -> Self {
        Self {
            id: "system_default_user".to_string(),
            username: "system_default_user".to_string(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }
}

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    /// `Local` replaces the former `local: bool` — it skips JWT verification
    /// and injects a fixed default user, same as `local: true` did.
    pub identity_mode: AuthIdentityMode,
    /// Optional second credential channel for agent-subprocess helper CLIs
    /// (`aioncore config` / `diagnose`), which cannot carry a JWT or cookies.
    /// `None` disables the channel (requests without a JWT are rejected).
    pub runtime_token_verifier: Option<Arc<dyn IRuntimeTokenVerifier>>,
    /// IP-allowlist check, `None` to skip enforcement entirely (the default
    /// for every test/call site that does not explicitly wire one — this
    /// keeps the feature strictly additive rather than a behavior change for
    /// anything that hasn't opted in).
    pub ip_allowlist: Option<Arc<dyn IpAllowlistGate>>,
}

/// Authentication middleware that verifies JWT tokens and injects `CurrentUser`.
///
/// Flow:
/// 1. Extract bearer token from `Authorization` header or `aionui-session` cookie
/// 2. Verify JWT signature, expiration, and blacklist
/// 3. Look up user in the database to ensure they still exist
/// 4. Insert [`CurrentUser`] into request extensions
///
/// Returns HTTP 401 for authentication failures.
///
/// Use with `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // Local mode is the desktop's co-located backend: the no-login operator is
    // resolved to `system_default_user`. But that SAME backend is what a
    // "本机作为服务器" deployment exposes to remote clients (web-host proxies
    // `/api/*` from `0.0.0.0` to this `--local` backend). Those clients present
    // their SSO-issued JWT and MUST resolve to their real identity — otherwise
    // every remote member collapses to `system_default_user` (seeing that
    // operator's tenant/members, and silently gaining its admin role). So in
    // local mode we still honor a *valid* bearer token when one is present, and
    // only fall back to the operator when there is no token (the local desktop
    // never sends one) or it fails to resolve.
    //
    // The operator fallback is what makes the desktop login-free, so it must
    // apply ONLY to the desktop. A request carrying [`WEBUI_PROXY_HEADER`]
    // reached us through the WebUI listener — which is bound to `0.0.0.0`
    // whenever "允许远程访问" is on — so it is never "the operator at the
    // keyboard" no matter what `--local` says. Those fall through to the strict
    // path below and get 401 without a real session.
    if state.identity_mode == AuthIdentityMode::Local && !is_webui_proxied(request.headers()) {
        if let Some(token) = extract_token_from_headers(request.headers())
            && let Ok(payload) = state.jwt_service.verify(&token)
            && let Ok(Some(user)) = state.user_repo.find_by_id(&payload.user_id).await
        {
            request.extensions_mut().insert(CurrentUser {
                id: user.id,
                username: user.username.unwrap_or_else(|| "external_user".to_string()),
                user_type: user.user_type,
                status: user.status,
            });
            return Ok(next.run(request).await);
        }
        request.extensions_mut().insert(CurrentUser::local_default());
        return Ok(next.run(request).await);
    }

    let Some(token) = extract_token_from_headers(request.headers()) else {
        // No JWT/cookie: fall back to the conversation-helper runtime-token
        // channel used by agent subprocess CLIs.
        return runtime_token_channel(&state, request, next).await;
    };

    let payload = state.jwt_service.verify(&token).map_err(|e| {
        tracing::debug!("Token verification failed: {e}");
        ApiError::Unauthorized("Invalid or expired token".into())
    })?;

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "auth middleware user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    // IP-allowlist enforcement. Only reachable here — the operator-fallback
    // branches above return early and never cross this point — which is
    // exactly right: this path is the one taken by real network traffic
    // (standalone server, or the WebUI proxy's remote clients), while the
    // branches above are the desktop's own same-machine backend, which has
    // no "remote IP" to restrict.
    //
    // The gate, not this middleware, decides whether an unresolvable IP is
    // fatal: most callers (personal edition, no project group, allowlist
    // disabled) have nothing to check and must pass regardless of whether
    // `ConnectInfo` happened to be available — including every test harness
    // that drives the router directly via `.oneshot()`. Only a caller whose
    // group has enforcement actually turned on should be denied here.
    if let Some(gate) = &state.ip_allowlist {
        let ip = resolve_caller_ip(&request);
        match gate.is_allowed(&user.id, ip).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(user_id = %user.id, ?ip, "auth middleware: request blocked by IP allowlist");
                return Err(ApiError::Forbidden("IP address not permitted".into()));
            }
            Err(error) => {
                tracing::error!(user_id = %user.id, %error, "auth middleware: IP allowlist check failed");
                return Err(ApiError::Internal("IP allowlist check failed".into()));
            }
        }
    }

    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    });

    Ok(next.run(request).await)
}

/// Authenticate a JWT-less request via the conversation-helper runtime token.
///
/// The helper CLI sends the token the backend minted for its conversation
/// runtime plus the (user, conversation) pair the token was bound to. The
/// verifier enforces that binding, so a forged user or conversation header
/// fails closed. On success the token's user is loaded and injected as
/// [`CurrentUser`], making ordinary user-scoped handlers work unchanged.
async fn runtime_token_channel(state: &AuthState, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let headers = request.headers();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(verifier), Some(token), Some(user_id), Some(conversation_id)) = (
        state.runtime_token_verifier.as_ref(),
        header(RUNTIME_TOKEN_HEADER),
        header(RUNTIME_USER_ID_HEADER),
        header(RUNTIME_CONVERSATION_ID_HEADER),
    ) else {
        return Err(ApiError::Unauthorized("Authentication required".into()));
    };

    if !verifier.verify_conversation_helper(&token, &user_id, &conversation_id) {
        return Err(ApiError::Unauthorized("Invalid runtime token".into()));
    }

    let user = state
        .user_repo
        .find_active_by_id(&user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "runtime token channel user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    });

    Ok(next.run(request).await)
}

/// Local-mode authentication middleware that skips JWT verification.
///
/// Injects a fixed `CurrentUser` with id and username `system_default_user`.
/// Used when the server runs as an embedded subprocess inside Electron.
pub async fn local_auth_middleware(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(CurrentUser::local_default());
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    async fn echo_user(request: Request<Body>) -> String {
        let user = request.extensions().get::<CurrentUser>().unwrap();
        format!("{}:{}", user.id, user.username)
    }

    #[tokio::test]
    async fn test_local_auth_middleware_injects_default_user() {
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn(local_auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "system_default_user:system_default_user"
        );
    }

    async fn local_auth_app(user_repo: Arc<dyn IUserRepository>, jwt_service: Arc<JwtService>) -> Router {
        let state = AuthState {
            jwt_service,
            user_repo,
            local: true,
            ip_allowlist: None,
        };
        Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    async fn body_string(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// A "本机作为服务器" deployment proxies remote clients to the SAME
    /// `--local` backend. A client presenting a valid SSO-issued JWT must
    /// resolve to their real identity, not collapse to the operator.
    #[tokio::test]
    async fn local_mode_honors_a_valid_bearer_token() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let app = local_auth_app(user_repo, jwt).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, format!("{}:{}", user.id, user.username));
    }

    /// The desktop operator (no token) still resolves to `system_default_user`
    /// in local mode — the no-login convenience is preserved.
    #[tokio::test]
    async fn local_mode_without_a_token_falls_back_to_default_user() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));

        let app = local_auth_app(user_repo, jwt).await;
        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "system_default_user:system_default_user");
    }

    /// A request forwarded by the WebUI proxy is NOT the desktop operator, even
    /// though the process runs with `--local`. Without a session it must 401 —
    /// this is the whole point of the header: with "允许远程访问" on, the WebUI
    /// listener is bound to 0.0.0.0, so this path was handing `system_default_user`
    /// (and its admin role) to anyone on the network, with no credential at all.
    #[tokio::test]
    async fn local_mode_rejects_a_proxied_request_that_carries_no_session() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));

        let app = local_auth_app(user_repo, jwt).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header(WEBUI_PROXY_HEADER, WEBUI_PROXY_VALUE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// ...but a proxied request WITH a valid session resolves to that real
    /// user. Closing the hole must not break the logged-in WebUI.
    #[tokio::test]
    async fn local_mode_honors_a_valid_session_on_a_proxied_request() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let app = local_auth_app(user_repo, jwt).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header(WEBUI_PROXY_HEADER, WEBUI_PROXY_VALUE)
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, format!("{}:{}", user.id, user.username));
    }

    /// A forged/expired token on a proxied request must 401 rather than fall
    /// back to the operator — the fallback is exactly what we are removing.
    #[tokio::test]
    async fn local_mode_does_not_fall_back_to_the_operator_on_a_proxied_request() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));

        let app = local_auth_app(user_repo, jwt).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header(WEBUI_PROXY_HEADER, WEBUI_PROXY_VALUE)
                    .header("Authorization", "Bearer not-a-real-jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// An invalid/forged token in local mode does not 401 — it falls back to
    /// the operator, so a malformed client request never hard-fails the
    /// desktop's own no-auth path.
    #[tokio::test]
    async fn local_mode_with_an_invalid_token_falls_back_to_default_user() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));

        let app = local_auth_app(user_repo, jwt).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", "Bearer not-a-real-jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "system_default_user:system_default_user");
    }

    // --- IP allowlist enforcement ---

    use std::net::Ipv4Addr;

    /// Always allows — models a caller with nothing to check (no project
    /// group, or allowlist disabled), the realistic default and the case
    /// every unrelated e2e test in the app implicitly relies on.
    struct AllowGate;
    #[async_trait]
    impl IpAllowlistGate for AllowGate {
        async fn is_allowed(&self, _user_id: &str, _ip: Option<IpAddr>) -> Result<bool, String> {
            Ok(true)
        }
    }

    /// Always denies — models a caller whose group has enforcement on and
    /// whose IP simply isn't on the list.
    struct DenyGate;
    #[async_trait]
    impl IpAllowlistGate for DenyGate {
        async fn is_allowed(&self, _user_id: &str, _ip: Option<IpAddr>) -> Result<bool, String> {
            Ok(false)
        }
    }

    /// Models a caller whose group HAS enforcement on: allows a resolved IP,
    /// denies when it cannot verify one at all — mirrors
    /// `PlatformIpAllowlistGate`'s real fail-closed behavior once enforcement
    /// is actually active (as opposed to `AllowGate`, which models "nothing
    /// to check" and never denies for this reason).
    struct EnforcingGate;
    #[async_trait]
    impl IpAllowlistGate for EnforcingGate {
        async fn is_allowed(&self, _user_id: &str, ip: Option<IpAddr>) -> Result<bool, String> {
            Ok(ip.is_some())
        }
    }

    #[derive(Default, Clone)]
    struct RecordingGate {
        seen: Arc<std::sync::Mutex<Vec<Option<IpAddr>>>>,
    }
    #[async_trait]
    impl IpAllowlistGate for RecordingGate {
        async fn is_allowed(&self, _user_id: &str, ip: Option<IpAddr>) -> Result<bool, String> {
            self.seen.lock().unwrap().push(ip);
            Ok(true)
        }
    }

    fn standalone_app(
        user_repo: Arc<dyn IUserRepository>,
        jwt_service: Arc<JwtService>,
        ip_allowlist: Option<Arc<dyn IpAllowlistGate>>,
    ) -> Router {
        let state = AuthState {
            jwt_service,
            user_repo,
            local: false,
            ip_allowlist,
        };
        Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn connect_info_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 54321)
    }

    /// The strict path (standalone server, no reverse proxy) is exactly where
    /// this feature is meant to bite: a real remote caller, resolvable via
    /// `ConnectInfo` since nothing splices the connection over loopback here.
    #[tokio::test]
    async fn strict_path_allows_when_gate_permits_the_connect_info_ip() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let app = standalone_app(user_repo, jwt, Some(Arc::new(AllowGate)));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .extension(ConnectInfo(connect_info_addr()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn strict_path_rejects_when_gate_denies_the_connect_info_ip() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let app = standalone_app(user_repo, jwt, Some(Arc::new(DenyGate)));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .extension(ConnectInfo(connect_info_addr()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// A caller with nothing to check (`AllowGate` — the realistic default:
    /// personal edition, or a group with no allowlist) must pass even when
    /// the real IP cannot be resolved (no `ConnectInfo`) — this is exactly
    /// the shape of every unrelated e2e test in `aionui-app` that drives the
    /// router via `.oneshot()` without a real `ConnectInfo`, so getting this
    /// wrong 403s the entire existing test suite, not just this feature.
    #[tokio::test]
    async fn strict_path_allows_when_gate_has_nothing_to_check_even_without_connect_info() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let app = standalone_app(user_repo, jwt, Some(Arc::new(AllowGate)));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A caller whose group DOES have enforcement on (`EnforcingGate`) must
    /// fail closed when the real IP cannot be resolved — "cannot verify"
    /// must not be treated as "allowed" once enforcement is actually active.
    #[tokio::test]
    async fn strict_path_rejects_an_enforcing_gate_when_the_real_ip_cannot_be_resolved() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let app = standalone_app(user_repo, jwt, Some(Arc::new(EnforcingGate)));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// A proxied request trusts the forwarded-IP header, not `ConnectInfo` —
    /// `ConnectInfo` would show 127.0.0.1 for every proxied caller regardless
    /// of who actually connected (the proxy splices over loopback).
    #[tokio::test]
    async fn proxied_request_uses_the_forwarded_ip_header_not_connect_info() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let gate = RecordingGate::default();
        let app = standalone_app(user_repo, jwt, Some(Arc::new(gate.clone())));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .header(WEBUI_PROXY_HEADER, WEBUI_PROXY_VALUE)
                    .header(CLIENT_IP_HEADER, "198.51.100.9")
                    // A loopback ConnectInfo is what a proxy-spliced connection
                    // would actually show — the header must win over it.
                    .extension(ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            gate.seen.lock().unwrap().as_slice(),
            &[Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)))]
        );
    }

    /// A client-supplied client-ip header on a NON-proxied request must be
    /// ignored — only a request that passes `is_webui_proxied` trusts it.
    #[tokio::test]
    async fn non_proxied_request_ignores_a_client_supplied_ip_header() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let gate = RecordingGate::default();
        let app = standalone_app(user_repo, jwt, Some(Arc::new(gate.clone())));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .header(CLIENT_IP_HEADER, "198.51.100.9")
                    .extension(ConnectInfo(connect_info_addr()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(gate.seen.lock().unwrap().as_slice(), &[Some(connect_info_addr().ip())]);
    }

    /// The desktop operator (no proxy, no token — the fallback branch) must
    /// never be IP-gated: it is inherently the same machine, and enforcing it
    /// here would risk locking the operator out of their own local app.
    #[tokio::test]
    async fn operator_fallback_is_never_ip_gated() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));

        let state = AuthState {
            jwt_service: jwt,
            user_repo,
            local: true,
            ip_allowlist: Some(Arc::new(DenyGate)),
        };
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Same for "valid bearer token in local mode, not proxied" — still the
    /// desktop's own direct traffic, not a network boundary.
    #[tokio::test]
    async fn local_mode_valid_token_not_proxied_is_never_ip_gated() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        let user = user_repo.create_user("zhaogao", "pw").await.unwrap();
        let jwt = Arc::new(JwtService::new("test-secret".to_string()));
        let token = jwt.sign(&user.id, &user.username).unwrap();

        let state = AuthState {
            jwt_service: jwt,
            user_repo,
            local: true,
            ip_allowlist: Some(Arc::new(DenyGate)),
        };
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
