//! End-to-end checks on the model proxy, against a real upstream server.
//!
//! The unit tests in `model_proxy.rs` cover the helpers. These cover the bet
//! the whole design rests on: that forwarding the path verbatim is enough for
//! every protocol the product speaks, and that the caller's token is swapped
//! for the company credential on the way out. Both are things only a real
//! request through the real router can show.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::any;
use one_devops::{DevopsService, OneDevopsRouterState, model_proxy_routes, run_one_devops_migrations};
use tower::ServiceExt;

const KEY: [u8; 32] = [9u8; 32];
const COMPANY_SECRET: &str = "sk-company-credential";

/// What the upstream saw, so a test can assert on it after the fact.
#[derive(Default, Clone)]
struct Seen {
    path: String,
    authorization: String,
    body: String,
    custom_header: String,
}

/// A stand-in for a model vendor: records the request and echoes something back.
async fn spawn_upstream(seen: Arc<Mutex<Seen>>) -> SocketAddr {
    async fn record(
        State(seen): State<Arc<Mutex<Seen>>>,
        Path(path): Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> String {
        let mut slot = seen.lock().unwrap();
        slot.path = path;
        slot.authorization = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        slot.custom_header = headers
            .get("x-dashscope-async")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        slot.body = body;
        "{\"ok\":true}".to_owned()
    }

    let app = Router::new().route("/{*path}", any(record)).with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

async fn service() -> DevopsService {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_one_devops_migrations(&pool).await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id));
         CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE one_tenants (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT 0);",
    )
    .execute(&pool)
    .await
    .unwrap();
    DevopsService::new(pool).with_encryption_key(KEY)
}

struct Fixture {
    app: Router,
    seen: Arc<Mutex<Seen>>,
    svc: Arc<DevopsService>,
    channel_id: String,
    token: String,
}

async fn fixture() -> Fixture {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let addr = spawn_upstream(seen.clone()).await;
    let svc = service().await;
    let channel = svc
        .upsert_provider_channel(
            None,
            "corp-gateway",
            "openai",
            &format!("http://{addr}"),
            Some(COMPANY_SECRET),
            r#"["gpt-image-2"]"#,
            None,
            true,
            "org",
            None,
            "all",
            "admin1",
        )
        .await
        .unwrap();
    let token = svc.issue_channel_token("admin1", &channel.id).await.unwrap().token;
    let svc = Arc::new(svc);
    let app = model_proxy_routes(OneDevopsRouterState::new(svc.clone()));
    Fixture {
        app,
        seen,
        svc,
        channel_id: channel.id,
        token,
    }
}

/// The core bet: whatever path the caller appends to the proxy base URL arrives
/// at the vendor unchanged. These are the three shapes the product actually
/// produces — OpenAI chat, the images API, and a DashScope async task — and a
/// protocol-aware proxy would need a branch for each.
#[tokio::test]
async fn every_protocol_shape_reaches_the_vendor_with_its_path_intact() {
    let f = fixture().await;

    for path in [
        "v1/chat/completions",
        "v1/images/generations",
        "api/v1/services/aigc/text2image/image-synthesis",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/one/model-proxy/{}/{path}", f.channel_id))
            .header("authorization", format!("Bearer {}", f.token))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"gpt-image-2"}"#))
            .unwrap();

        let response = f.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "path {path} should have forwarded");
        assert_eq!(f.seen.lock().unwrap().path, path, "path was rewritten");
    }
}

/// The credential swap, in both directions: the member's channel token must not
/// escape to the vendor, and the company key must not be needed by the member.
#[tokio::test]
async fn the_company_credential_is_substituted_for_the_members_token() {
    let f = fixture().await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/one/model-proxy/{}/v1/chat/completions", f.channel_id))
        .header("authorization", format!("Bearer {}", f.token))
        .header("content-type", "application/json")
        .header("x-dashscope-async", "enable")
        .body(Body::from(r#"{"model":"x"}"#))
        .unwrap();
    f.app.clone().oneshot(request).await.unwrap();

    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.authorization, format!("Bearer {COMPANY_SECRET}"));
    assert!(
        !seen.authorization.contains(&f.token),
        "the member's channel token leaked upstream"
    );
    // Everything the vendor genuinely needs still gets through.
    assert_eq!(seen.custom_header, "enable");
    assert_eq!(seen.body, r#"{"model":"x"}"#);
}

#[tokio::test]
async fn a_request_without_a_usable_token_never_reaches_the_vendor() {
    let f = fixture().await;

    for auth in [None, Some("Bearer onech-not-a-real-token"), Some("Basic whatever")] {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("/api/one/model-proxy/{}/v1/chat/completions", f.channel_id));
        if let Some(value) = auth {
            builder = builder.header("authorization", value);
        }
        let response = f
            .app
            .clone()
            .oneshot(builder.body(Body::from("{}")).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "auth {auth:?} was accepted"
        );
        assert!(
            f.seen.lock().unwrap().path.is_empty(),
            "an unauthorized request was forwarded upstream"
        );
    }
}

/// Revocation has to bite at the proxy, not merely hide the channel from a
/// listing — that is the whole point of the token being separate from the
/// session. This is the offboarding path: `one-org` removing a member calls
/// exactly this method through `CredentialRevoker`.
#[tokio::test]
async fn a_revoked_token_stops_at_the_proxy() {
    let f = fixture().await;

    let call = || {
        Request::builder()
            .method("POST")
            .uri(format!("/api/one/model-proxy/{}/v1/chat/completions", f.channel_id))
            .header("authorization", format!("Bearer {}", f.token))
            .body(Body::from("{}"))
            .unwrap()
    };

    // Prove it works first, so a failure below cannot be a setup mistake.
    assert_eq!(f.app.clone().oneshot(call()).await.unwrap().status(), StatusCode::OK);

    assert_eq!(f.svc.revoke_channel_tokens_for_user("admin1").await.unwrap(), 1);
    f.seen.lock().unwrap().path.clear();

    let response = f.app.clone().oneshot(call()).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        f.seen.lock().unwrap().path.is_empty(),
        "a revoked member still reached the vendor"
    );
}
