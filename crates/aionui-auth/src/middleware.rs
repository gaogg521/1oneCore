#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_db::IUserRepository;

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
}

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    /// When `true`, skip JWT verification and inject a fixed default user.
    pub local: bool,
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
    if state.local && !is_webui_proxied(request.headers()) {
        if let Some(token) = extract_token_from_headers(request.headers())
            && let Ok(payload) = state.jwt_service.verify(&token)
            && let Ok(Some(user)) = state.user_repo.find_by_id(&payload.user_id).await
        {
            request.extensions_mut().insert(CurrentUser {
                id: user.id,
                username: user.username,
            });
            return Ok(next.run(request).await);
        }
        request.extensions_mut().insert(CurrentUser {
            id: "system_default_user".to_string(),
            username: "system_default_user".to_string(),
        });
        return Ok(next.run(request).await);
    }

    let token = extract_token_from_headers(request.headers())
        .ok_or_else(|| ApiError::Unauthorized("Authentication required".into()))?;

    let payload = state.jwt_service.verify(&token).map_err(|e| {
        tracing::debug!("Token verification failed: {e}");
        ApiError::Unauthorized("Invalid or expired token".into())
    })?;

    let user = state
        .user_repo
        .find_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "auth middleware user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username,
    });

    Ok(next.run(request).await)
}

/// Local-mode authentication middleware that skips JWT verification.
///
/// Injects a fixed `CurrentUser` with id and username `system_default_user`.
/// Used when the server runs as an embedded subprocess inside Electron.
pub async fn local_auth_middleware(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(CurrentUser {
        id: "system_default_user".to_string(),
        username: "system_default_user".to_string(),
    });
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
}
