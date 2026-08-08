//! The model proxy: the only place a company channel's credential is used.
//!
//! A member's desktop gets a normal-looking provider whose `base_url` points
//! here and whose `api_key` is their revocable channel token. Everything
//! downstream — aionrs chat, the ACP bridges, all three media adapter forms —
//! then works unchanged, because they only ever read a base URL and a key.
//!
//! # Why path-preserving rather than protocol-aware
//!
//! `aionui-codex-bridge` decodes requests into `LlmEvent`s and re-encodes them.
//! That is right for translating between two protocols, and wrong here: media
//! alone speaks three mutually incompatible shapes (`/v1/images/generations`,
//! chat completions, and the DashScope/Ark async task APIs), so a
//! protocol-aware proxy would need a branch per shape and would break the day a
//! vendor adds a fourth.
//!
//! Instead the whole path after the channel id is forwarded verbatim:
//!
//! ```text
//! ANY /api/one/model-proxy/{channel_id}/{*path}
//!   → {channel.upstream_base_url}/{path}   + the real Authorization
//! ```
//!
//! so `/v1/chat/completions`, `/v1/images/generations` and
//! `/api/v1/services/aigc/...` all arrive intact with no per-protocol code.
//!
//! # Streaming
//!
//! Request and response bodies are streamed, never buffered. Buffering would
//! turn every streamed chat reply into a long silence followed by the whole
//! answer at once, and would hold multi-megabyte image payloads in memory on
//! the server for no reason.
//!
//! # Not session-authenticated
//!
//! The caller is an agent process, not a browser: it presents a channel token,
//! not a session cookie. Same arrangement as the Codex bridge's public route,
//! and the same reason it must sit outside the CSRF layer.

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::TryStreamExt;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::warn;

use crate::state::OneDevopsRouterState;

/// Image-to-image sends reference images inline as data URIs, which routinely
/// exceeds the app-wide 10 MB default. This is a pass-through — the body is
/// never held whole — so the cap only exists to bound a hostile request.
const PROXY_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Headers that describe *this* hop and must not be copied to the next one.
/// `authorization` is replaced with the company credential; `host` must follow
/// the upstream URL; the rest are connection-level.
const HOP_BY_HOP: &[&str] = &[
    "authorization",
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

pub fn model_proxy_routes(state: OneDevopsRouterState) -> Router {
    Router::new()
        .route("/api/one/model-proxy/{channel_id}/{*path}", any(handle_proxy))
        .with_state(state)
        // Replace the app-wide default rather than layering under it: a
        // pass-through has different constraints from a JSON endpoint.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(PROXY_BODY_LIMIT))
}

#[derive(serde::Deserialize)]
struct ProxyPath {
    channel_id: String,
    path: String,
}

/// Bearer token from the request, if it looks like one.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// One flat error shape for every rejection.
///
/// Deliberately indistinguishable across "no token", "revoked token", "token
/// for another channel" and "no such channel": telling them apart would let
/// anyone holding one token enumerate the company's channels.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": {
                "message": "not authorized for this model channel",
                "type": "invalid_request_error"
            }
        })),
    )
        .into_response()
}

async fn handle_proxy(
    State(state): State<OneDevopsRouterState>,
    Path(params): Path<ProxyPath>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(token) = bearer_token(&headers) else {
        return unauthorized();
    };

    let channel = match state
        .service
        .resolve_channel_for_token(&params.channel_id, &token)
        .await
    {
        Ok(Some(channel)) => channel,
        Ok(None) => return unauthorized(),
        Err(err) => {
            // A misconfigured channel (no credential, unreadable key) is the
            // operator's problem, not the caller's — say so rather than
            // pretending they are unauthorized, which would send them hunting
            // through their own settings.
            warn!(
                channel_id = %params.channel_id,
                error = %err,
                "model_proxy: channel could not be resolved"
            );
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": { "message": err.to_string(), "type": "server_error" }
                })),
            )
                .into_response();
        }
    };

    let url = format!("{}/{}", channel.upstream_base_url, params.path.trim_start_matches('/'));

    let client = reqwest::Client::new();
    let mut request = client.request(method, &url);
    for (name, value) in headers.iter() {
        if !is_hop_by_hop(name.as_str()) {
            request = request.header(name.as_str(), value.as_bytes());
        }
    }
    request = request.header("Authorization", format!("Bearer {}", channel.api_key));
    // Stream the request body straight through: an image-to-image payload can
    // be tens of megabytes and there is no reason for it to land in memory.
    request = request.body(reqwest::Body::wrap_stream(
        body.into_data_stream().map_err(std::io::Error::other),
    ));

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            // Never log the URL with credentials or the body; the channel id
            // and the user are enough to find the misconfiguration.
            warn!(
                channel_id = %channel.id,
                user_id = %channel.user_id,
                error = %err,
                "model_proxy: upstream request failed"
            );
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": { "message": format!("upstream request failed: {err}"), "type": "server_error" }
                })),
            )
                .into_response();
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_ref()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }

    // Stream the response too, so SSE arrives token by token instead of as one
    // long pause followed by the whole answer.
    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_bearer_token_is_read_in_either_case() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer onech-abc"));
        assert_eq!(bearer_token(&headers).as_deref(), Some("onech-abc"));

        headers.insert("authorization", HeaderValue::from_static("bearer onech-abc"));
        assert_eq!(bearer_token(&headers).as_deref(), Some("onech-abc"));
    }

    #[test]
    fn anything_that_is_not_a_bearer_token_is_no_token() {
        let mut headers = HeaderMap::new();
        assert!(bearer_token(&headers).is_none());
        headers.insert("authorization", HeaderValue::from_static("Basic abc"));
        assert!(bearer_token(&headers).is_none());
        headers.insert("authorization", HeaderValue::from_static("Bearer   "));
        assert!(bearer_token(&headers).is_none());
    }

    /// The caller's own Authorization must never reach upstream — it carries
    /// the channel token, and the upstream gets the company credential instead.
    #[test]
    fn the_callers_credentials_are_not_forwarded() {
        assert!(is_hop_by_hop("Authorization"));
        assert!(is_hop_by_hop("authorization"));
        assert!(is_hop_by_hop("Host"));
        assert!(is_hop_by_hop("Content-Length"));
        // …while everything the model API actually needs passes through.
        assert!(!is_hop_by_hop("content-type"));
        assert!(!is_hop_by_hop("accept"));
        assert!(!is_hop_by_hop("x-dashscope-async"));
    }
}
