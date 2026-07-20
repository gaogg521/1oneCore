//! Provider health-check route auth and validation tests.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

#[tokio::test]
async fn provider_health_check_unauthenticated_is_rejected() {
    let (app, _services) = build_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/agents/provider-health-check")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"provider_id": "p1", "model": "gpt-4o"})).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected auth rejection, got {}",
        resp.status()
    );
}

// Bearer requests carry no ambient credential a cross-site form could ride
// on, so the CSRF middleware exempts them (remote-desktop clients depend on
// this — see M4d, crates/aionui-auth/src/csrf.rs). Cookie-authenticated
// requests still require the CSRF token pair.
#[tokio::test]
async fn provider_health_check_allows_bearer_without_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/agents/provider-health-check")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&json!({"provider_id": "p1", "model": "gpt-4o"})).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    // Not CSRF-rejected — request reaches the handler, which then reports the
    // unknown provider_id (a separate, expected validation error).
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "BAD_REQUEST");
    assert_eq!(body["error"], "Provider 'p1' not found");
}

#[tokio::test]
async fn provider_health_check_requires_csrf_for_cookie_auth() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/agents/provider-health-check")
        .header("content-type", "application/json")
        .header("cookie", format!("aionui-session={token}"))
        .body(Body::from(
            serde_json::to_vec(&json!({"provider_id": "p1", "model": "gpt-4o"})).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn provider_health_check_validates_required_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/agents/provider-health-check",
        json!({"provider_id": "", "model": "gpt-4o"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "BAD_REQUEST");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|message| message.contains("provider_id is required")),
        "expected provider_id validation error, got {json}"
    );
}
