//! Top-level router assembly: middleware stack + module route merges.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use tower_http::cors::{Any, CorsLayer};

use aionui_ai_agent::{agent_routes, remote_agent_routes};
use aionui_api_types::ErrorResponse;
use aionui_assets::{AssetRouterState, asset_routes};
use aionui_assistant::assistant_routes;
use aionui_auth::{
    AuthRouterState, AuthState, auth_middleware, auth_routes, csrf_middleware, security_headers_middleware,
};
use aionui_channel::channel_routes;
#[cfg(feature = "weixin")]
use aionui_channel::weixin_login_route;
use aionui_claude_bridge::claude_bridge_config_routes;
use aionui_codex_bridge::{codex_bridge_config_routes, codex_bridge_public_routes};
use aionui_common::ApiErrorLogContext;
use aionui_conversation::{conversation_ops_routes, conversation_routes};
use aionui_cron::cron_routes;
use aionui_extension::{extension_routes, hub_routes, skill_routes};
use aionui_file::file_routes;
use aionui_mcp::mcp_routes;
use aionui_office::{office_proxy_routes, office_routes};
use aionui_realtime::{WsHandlerState, ws_upgrade_handler};
use aionui_shell::shell_routes;
use aionui_system::{ClientPrefService, connection_test_routes, system_routes};
use aionui_team::{TeamSessionService, team_routes};

use crate::services::AppServices;

/// Adapts one-org's `OrgService::tenant_of` to the `one_employee::TenantResolver`
/// trait, so one-employee / one-devops can resolve a caller's tenant (for
/// team-shared employees, A1 L3) without depending on one-org. Resolution
/// errors fall back to the personal `default` tenant.
struct OrgTenantResolver(std::sync::Arc<one_org::OrgService>);

#[async_trait::async_trait]
impl one_employee::TenantResolver for OrgTenantResolver {
    async fn tenant_of(&self, user_id: &str) -> String {
        self.0
            .tenant_of(user_id)
            .await
            .unwrap_or_else(|_| one_employee::DEFAULT_TENANT.to_owned())
    }
}

/// Adapts one-org's `OrgService::auto_join_by_email` to the
/// `one_sso::OrgAutoJoin` trait (P2-4 onboarding: domain-based project-group
/// auto-join). Errors are logged and swallowed — never blocks a valid login.
struct OrgAutoJoinAdapter(std::sync::Arc<one_org::OrgService>);

#[async_trait::async_trait]
impl one_sso::OrgAutoJoin for OrgAutoJoinAdapter {
    async fn auto_join_by_email(&self, user_id: &str, email: &str) {
        match self.0.auto_join_by_email(user_id, email).await {
            Ok(Some(tenant_id)) => {
                tracing::info!(
                    user_id,
                    tenant_id,
                    "SSO login: auto-joined project group by email domain"
                );
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, user_id, "onboarding domain auto-join failed; login continues");
            }
        }
    }
}

/// Adapts one-enterprise's `EnterpriseService::sync_member` to the
/// `one_sso::EnterpriseSync` trait, so an SSO login can sync the caller's
/// company + membership into the enterprise-org domain without one-sso
/// depending on one-enterprise. Best-effort by construction (see the service
/// method); errors are logged and swallowed so a failed sync can never block a
/// valid login.
/// Lets one-org revoke a departing member's company model channel tokens
/// without depending on one-devops (same layer). Those tokens deliberately
/// outlive JWT rotation, so removing a member has to close them explicitly —
/// otherwise the leaver keeps a working key to the company's models, which is
/// the whole thing channel provisioning exists to prevent.
struct ModelChannelRevoker(std::sync::Arc<one_devops::DevopsService>);

#[async_trait::async_trait]
impl one_org::CredentialRevoker for ModelChannelRevoker {
    async fn revoke_for_user(&self, user_id: &str) {
        match self.0.revoke_channel_tokens_for_user(user_id).await {
            Ok(0) => {}
            Ok(revoked) => tracing::info!(user_id, revoked, "revoked model channel tokens"),
            // Never block the removal: a member who could not be fully
            // de-provisioned must still be removed. Logged loudly because it
            // leaves a live credential behind and needs following up.
            Err(error) => tracing::error!(%error, user_id, "failed to revoke model channel tokens on removal"),
        }
    }
}

/// Lets a company removal cut off the leaver's access without one-enterprise
/// depending on one-org (same layer). one-org owns the per-user JWT secret and,
/// via its own `CredentialRevoker`, the company model channel tokens — so both
/// tiers of removal end up rotating exactly the same set of credentials.
struct OrgSessionRevoker(std::sync::Arc<one_org::OrgService>);

#[async_trait::async_trait]
impl one_enterprise::SessionRevoker for OrgSessionRevoker {
    async fn revoke_sessions(&self, user_id: &str) {
        // Never block the removal: a member who could not be fully
        // de-provisioned must still lose their seat. Logged at error because it
        // leaves a live session behind and needs following up.
        if let Err(error) = self.0.invalidate_user_tokens(user_id).await {
            tracing::error!(%error, user_id, "failed to revoke sessions on company removal");
        }
    }
}

/// Lets one-org read the company directory mirror to map a subtree into a
/// project group's department tree (T6 stage 3), without depending on
/// one-enterprise (same layer). No company, or no directory sync ever run →
/// empty, which the caller treats as "nothing to map" rather than an error.
struct DirectoryTreeSourceAdapter(std::sync::Arc<one_enterprise::EnterpriseService>);

#[async_trait::async_trait]
impl one_org::DirectoryTreeSource for DirectoryTreeSourceAdapter {
    async fn directory_departments(&self) -> Vec<one_org::DirectoryDepartmentRef> {
        let Some(enterprise_id) = self.0.deployment_company_id().await.ok().flatten() else {
            return Vec::new();
        };
        self.0
            .list_directory_departments(&enterprise_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|d| one_org::DirectoryDepartmentRef {
                external_id: d.external_id,
                parent_external_id: d.parent_external_id,
                name: d.name,
            })
            .collect()
    }
}

/// Stores a completed directory pull (T6). one-sso knows how to talk to Feishu,
/// one-enterprise owns the company's tables, and they are the same layer — so
/// they meet here.
struct DirectorySinkAdapter(std::sync::Arc<one_enterprise::EnterpriseService>);

#[async_trait::async_trait]
impl one_sso::DirectorySink for DirectorySinkAdapter {
    async fn enterprise_id(&self) -> Option<String> {
        // No company set up → nothing to attribute a directory to, and the
        // caller treats that as "skip", not "fail".
        self.0.deployment_company_id().await.ok().flatten()
    }

    async fn apply_snapshot(&self, enterprise_id: &str, snapshot: one_sso::DirectorySnapshotPayload) {
        let input = one_enterprise::directory::DirectorySyncInput {
            provider: snapshot.provider,
            external_id_field: snapshot.external_id_field,
            departments: snapshot
                .departments
                .into_iter()
                .map(|d| one_enterprise::directory::DirectoryDepartmentInput {
                    external_id: d.external_id,
                    parent_external_id: d.parent_external_id,
                    name: d.name,
                })
                .collect(),
            people: snapshot
                .people
                .into_iter()
                .map(|p| one_enterprise::directory::DirectoryPersonInput {
                    external_id: p.external_id,
                    name: p.name,
                    job_title: p.job_title,
                    department_external_id: p.department_external_id,
                    active: p.active,
                })
                .collect(),
            // Carried through verbatim. This is the flag that decides whether
            // absence means "left the company" — quietly defaulting it either
            // way would be the worst bug this feature could have.
            complete: snapshot.complete,
            error: snapshot.error,
        };

        match self.0.apply_directory_snapshot(enterprise_id, &input).await {
            Ok(report) => tracing::info!(
                enterprise_id,
                departments = report.departments,
                people = report.people,
                newly_missing = report.newly_missing,
                returned = report.returned,
                complete = report.complete,
                "directory sync applied"
            ),
            // Swallowed so a storage failure cannot unwind the background loop.
            // Logged at error because a directory that silently stops updating
            // is exactly how offboarding suggestions go stale.
            Err(error) => tracing::error!(%error, enterprise_id, "directory sync could not be stored"),
        }
    }
}

struct EnterpriseSyncAdapter(std::sync::Arc<one_enterprise::EnterpriseService>);

#[async_trait::async_trait]
impl one_sso::EnterpriseSync for EnterpriseSyncAdapter {
    async fn sync_member(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
    ) {
        if let Err(error) = self
            .0
            .sync_member(user_id, provider, external_id, display_name, department, job_title)
            .await
        {
            tracing::warn!(%error, user_id, provider, "enterprise-org sync failed; login continues");
        }
    }
}

/// Adapts one-enterprise's `EnterpriseService::is_company_admin_of` to the
/// `one_org::CompanyAdminResolver` trait (Direction B), so a company admin can
/// create/list the project groups their company owns without one-org depending
/// on one-enterprise. Resolution errors deny (fail closed).
struct CompanyAdminResolverAdapter(std::sync::Arc<one_enterprise::EnterpriseService>);

#[async_trait::async_trait]
impl one_org::CompanyAdminResolver for CompanyAdminResolverAdapter {
    async fn is_company_admin(&self, user_id: &str, enterprise_id: &str) -> bool {
        self.0
            .is_company_admin_of(user_id, enterprise_id)
            .await
            .unwrap_or(false)
    }
}

/// Adapts one-enterprise's `EnterpriseService::is_company_admin` to the
/// `one_sso::CompanyAdminCheck` trait, so a company admin may manage the
/// company-level SSO config (企业认证). Errors deny (fail closed).
struct CompanyAdminCheckAdapter(std::sync::Arc<one_enterprise::EnterpriseService>);

#[async_trait::async_trait]
impl one_sso::CompanyAdminCheck for CompanyAdminCheckAdapter {
    async fn is_company_admin(&self, user_id: &str) -> bool {
        self.0.is_company_admin(user_id).await.unwrap_or(false)
    }
}

/// Adapts one-billing's `BillingService::record_turn` to the conversation
/// crate's `UsageRecorder` trait (P0-3). Fire-and-forget: spawns the async
/// insert so metering never blocks or fails the send path.
struct BillingUsageRecorder(std::sync::Arc<one_billing::BillingService>);

impl aionui_conversation::UsageRecorder for BillingUsageRecorder {
    fn record_turn(&self, user_id: String, conversation_id: String) {
        let service = self.0.clone();
        tokio::spawn(async move {
            if let Err(e) = service
                .record_turn(&user_id, Some(&conversation_id), None, None, None)
                .await
            {
                tracing::debug!(error = %e, "usage record_turn failed (non-fatal)");
            }
        });
    }
}

/// Adapts one-billing's `check_send_allowed` to the conversation crate's
/// `SendGate` trait (P1-2 model control). Blocks a send when the team is over
/// its spend budget / off-allowlist; personal users always pass.
struct BillingSendGate(std::sync::Arc<one_billing::BillingService>);

/// Map a billing refusal to something the member can act on.
///
/// ⚠️ Only the two *policy* variants become visible text. Everything else is an
/// internal failure (a DB error, say) — the gate still fails closed, but its
/// `to_string()` must not reach the user, and there is nothing actionable in it
/// for them anyway. This mattered the moment these messages stopped being
/// redacted: before, `ApiError::Forbidden` hid every one of them equally.
fn billing_denial(error: one_billing::BillingError) -> aionui_conversation::PolicyDenial {
    use one_billing::BillingError;
    match error {
        BillingError::ModelNotAllowed(model) => aionui_conversation::PolicyDenial::new(
            "MODEL_NOT_ALLOWED",
            format!("Model '{model}' is not allowed by the team's policy"),
        )
        .with_details(serde_json::json!({ "model": model })),
        BillingError::BudgetExceeded => aionui_conversation::PolicyDenial::new(
            "BUDGET_EXCEEDED",
            "The team's usage budget for this period has been reached",
        ),
        // T6-4: arrived after the plan's seat cap filled. Distinct from every
        // other denial here — it isn't a rule the admin configured, it's the
        // absence of a seat, and the fix is "wait or ask for more seats" rather
        // than "wait for the budget window" or "ask for a different model".
        BillingError::SeatLimitExceeded => aionui_conversation::PolicyDenial::new(
            "SEAT_LIMIT_EXCEEDED",
            "Your organization's seat limit has been reached; an administrator needs to free a seat or upgrade the plan before you can send messages",
        ),
        other => {
            tracing::error!(error = %other, "billing gate failed; blocking the send");
            aionui_conversation::PolicyDenial::new("POLICY_CHECK_FAILED", "Company policy could not be checked.")
        }
    }
}

#[async_trait::async_trait]
impl aionui_conversation::SendGate for BillingSendGate {
    async fn check_send(&self, user_id: &str, model: Option<&str>) -> Result<(), aionui_conversation::PolicyDenial> {
        self.0.check_send_allowed(user_id, model).await.map_err(billing_denial)
    }

    async fn check_model(&self, user_id: &str, model: &str) -> Result<(), aionui_conversation::PolicyDenial> {
        self.0.check_model_allowed(user_id, model).await.map_err(billing_denial)
    }
}

/// Adapts aionui-system's local content inspector to the conversation crate's
/// `ContentInspector` trait (T4). Personal builds have no rules distributed, so
/// this costs a read lock and a length check per send.
struct LocalContentInspector(std::sync::Arc<aionui_system::ContentInspectionService>);

impl aionui_conversation::ContentInspector for LocalContentInspector {
    fn inspect(&self, conversation_id: &str, text: &str) -> Option<aionui_conversation::PolicyDenial> {
        // The model is not known at this point in the send path (the billing
        // gate has the same limitation), so findings are attributed by
        // conversation, which is what a reviewer follows back anyway.
        self.0.inspect(Some(conversation_id), None, text).blocked.map(|block| {
            // The rule name travels as a parameter, not baked into the
            // sentence: it is admin-authored data, while the sentence around
            // it is product copy the client translates.
            aionui_conversation::PolicyDenial::new("CONTENT_BLOCKED", block.reason)
                .with_details(serde_json::json!({ "ruleName": block.rule_name }))
        })
    }
}

use super::health::health_check;
use super::runtime_team_tools::{RuntimeTeamToolsState, runtime_team_tools_routes};
use super::state::{ModuleStates, RouterBuildError, build_module_states, build_ws_state};
use super::trace::with_access_log;

pub struct RouterRuntime {
    pub client_pref_service: ClientPrefService,
    pub team_service: Arc<TeamSessionService>,
    /// The two halves of T6 directory sync, handed back so `cmd_server` can
    /// drive them on a timer. Built here because this is where every service is
    /// already assembled; whether they ever do anything is decided at run time
    /// by whether this machine holds the company's SSO config.
    pub sso_service: Arc<one_sso::SsoService>,
    pub directory_sink: Arc<dyn one_sso::DirectorySink>,
}

/// Create the application router with all routes and global middleware.
///
/// Middleware stack (outermost → innermost):
/// 1. Security response headers (X-Frame-Options, etc.)
/// 2. CSRF protection (Double Submit Cookie)
/// 3. Route handlers (auth routes + system routes + conversation routes + file routes + health check)
pub async fn create_router(services: &AppServices) -> Result<Router, RouterBuildError> {
    let (router, _runtime) = create_router_with_runtime(services).await?;
    Ok(router)
}

/// Create the application router and return runtime handles needed by
/// background services started outside the router tree.
pub async fn create_router_with_runtime(services: &AppServices) -> Result<(Router, RouterRuntime), RouterBuildError> {
    let boot = Instant::now();
    tracing::info!("startup: router assembly started");

    // Bridge event bus → WebSocket manager: forward all broadcast events
    // to connected WebSocket clients.
    let mut event_rx = services.event_bus.subscribe();
    let ws_manager = services.ws_manager.clone();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            ws_manager.broadcast_all(event);
        }
    });

    let (states, channel_components) = build_module_states(services).await?;
    let client_pref_service = states.system.client_pref_service.clone();
    let team_service = states.team.service.clone();
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: module states built");

    // one-org keeps its own migration ledger (`_one_migrations`), fully
    // decoupled from the upstream sqlx migrator — see crates/one-org.
    one_org::run_one_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new("router.one_org.migrate", "failed to run one-org migrations").with_source(e)
        })?;
    one_employee::run_one_employee_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new("router.one_employee.migrate", "failed to run one-employee migrations").with_source(e)
        })?;
    one_sso::run_one_sso_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new("router.one_sso.migrate", "failed to run one-sso migrations").with_source(e)
        })?;
    one_devops::run_one_devops_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new("router.one_devops.migrate", "failed to run one-devops migrations").with_source(e)
        })?;
    one_enterprise::run_one_enterprise_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new(
                "router.one_enterprise.migrate",
                "failed to run one-enterprise migrations",
            )
            .with_source(e)
        })?;
    // MUST run after one-enterprise: billing_001_init grandfathers existing
    // one_enterprises rows to the top tier.
    one_billing::run_one_billing_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new("router.one_billing.migrate", "failed to run one-billing migrations").with_source(e)
        })?;
    // one-platform: deployment infra config (P1-3 container + P2-2 collab).
    one_platform::run_one_platform_migrations(services.database.pool())
        .await
        .map_err(|e| {
            RouterBuildError::new("router.one_platform.migrate", "failed to run one-platform migrations").with_source(e)
        })?;

    // Start channel orchestrator (message loop)
    tokio::spawn(
        channel_components
            .orchestrator
            .run(channel_components.message_rx, channel_components.confirm_rx),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel orchestrator spawned"
    );

    // Restore enabled channel plugins (starts receiving IM messages)
    let chan_mgr = channel_components.manager;
    let chan_factory = channel_components.plugin_factory;
    tokio::spawn(async move {
        if let Err(e) = chan_mgr.restore_plugins(&chan_factory).await {
            tracing::warn!(
                code = "BOOTSTRAP_DEGRADED_CHANNEL_RESTORE",
                stage = "channel.restore",
                error = %e,
                "failed to restore channel plugins"
            );
        }
    });
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel plugin restore scheduled"
    );

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route tree build started"
    );
    let router = create_router_with_states(services, states);
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: router assembly completed"
    );

    // T6 directory sync's two halves, for cmd_server's scheduler. Built here
    // rather than threaded out of the route builder because that function is
    // also a public test entry point returning only a Router. A second
    // `SsoService` over the same pool is harmless: the sync path touches only
    // the provider config table, never the in-memory OAuth state this instance
    // also carries.
    let directory_sso_service = Arc::new(one_sso::SsoService::new(
        services.database.pool().clone(),
        services.user_repo.clone(),
        services.jwt_service.clone(),
        services.cookie_config.clone(),
    ));
    let directory_sink: Arc<dyn one_sso::DirectorySink> = Arc::new(DirectorySinkAdapter(Arc::new(
        one_enterprise::EnterpriseService::new(services.database.pool().clone()),
    )));
    Ok((
        router,
        RouterRuntime {
            client_pref_service,
            team_service,
            sso_service: directory_sso_service,
            directory_sink,
        },
    ))
}

/// Create the application router with custom module states.
///
/// Used for testing when specific service overrides are needed
/// (e.g. injecting a mock HTTP server URL for version check).
pub fn create_router_with_states(services: &AppServices, states: ModuleStates) -> Router {
    let ws_state = build_ws_state(services);
    create_router_with_all_state(services, states, ws_state)
}

/// Create the application router with custom module states and WebSocket state.
///
/// Full-control variant used by tests that need to override
/// module services and WebSocket behaviour.
pub fn create_router_with_all_state(services: &AppServices, states: ModuleStates, ws_state: WsHandlerState) -> Router {
    let boot = Instant::now();
    tracing::info!("startup: route tree build with states started");

    let auth_state = AuthRouterState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        cookie_config: services.cookie_config.clone(),
        qr_token_store: services.qr_token_store.clone(),
        local: services.local,
    };

    let auth_mw_state = AuthState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        local: services.local,
    };

    // System routes protected by auth middleware
    let system_authenticated =
        system_routes(states.system).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-billing service (subscription tier / seats / usage). Built here — it
    // is dependency-free (pool + manual provider) — so its usage recorder can be
    // injected into the conversation routes below and its routes mounted later.
    let one_billing_service = std::sync::Arc::new(one_billing::BillingService::new(
        services.database.pool().clone(),
        std::sync::Arc::new(one_billing::ManualBillingProvider),
    ));

    // Conversation routes protected by auth middleware. Metering each accepted
    // send as one usage turn (P0-3).
    let conversation_authenticated = conversation_routes(
        states
            .conversation
            .clone()
            .with_usage_recorder(std::sync::Arc::new(BillingUsageRecorder(one_billing_service.clone())))
            .with_send_gate(std::sync::Arc::new(BillingSendGate(one_billing_service.clone())))
            .with_content_inspector(std::sync::Arc::new(LocalContentInspector(
                services.content_inspection.clone(),
            ))),
    )
    .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // The ops router hosts set-config-option (model switch) — gate it too so
    // the P1-2 model allowlist is enforced at model selection.
    let conversation_ops_authenticated = conversation_ops_routes(
        states
            .conversation
            .with_send_gate(std::sync::Arc::new(BillingSendGate(one_billing_service.clone()))),
    )
    .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Remote agent routes protected by auth middleware
    let remote_agent_authenticated = remote_agent_routes(states.remote_agent)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Unified agent listing/refresh/test routes protected by auth middleware
    let agent_authenticated =
        agent_routes(states.agent).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Connection test routes (Bedrock, Gemini) protected by auth middleware
    let connection_test_authenticated = connection_test_routes(states.connection_test)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // File routes protected by auth middleware
    let file_authenticated =
        file_routes(states.file).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // MCP routes protected by auth middleware
    let mcp_authenticated =
        mcp_routes(states.mcp).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Extension routes protected by auth middleware
    let extension_authenticated =
        extension_routes(states.extension).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Hub routes protected by auth middleware
    let hub_authenticated =
        hub_routes(states.hub).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Skill routes protected by auth middleware
    let skill_authenticated =
        skill_routes(states.skill).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Channel routes protected by auth middleware
    #[cfg(feature = "weixin")]
    let weixin_login_authenticated = weixin_login_route(states.channel.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
    let channel_authenticated =
        channel_routes(states.channel).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Team routes protected by auth middleware. Clone the team session
    // service out before moving the state into team_routes — one-employee
    // needs it for /run-team.
    let team_session_service = states.team.service.clone();
    let team_authenticated =
        team_routes(states.team.clone()).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Cron routes protected by auth middleware
    let cron_authenticated =
        cron_routes(states.cron).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Office routes protected by auth middleware
    let office_authenticated =
        office_routes(states.office.clone()).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Shell + STT routes protected by auth middleware
    let shell_authenticated =
        shell_routes(states.shell).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Assistant routes protected by auth middleware (T1a skeleton: all
    // handlers return 500 "not implemented"; T1b wires real service)
    let assistant_authenticated =
        assistant_routes(states.assistant).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Codex-bridge *settings* routes (which saved provider/model it forwards
    // to) are app-facing config, protected like any other authenticated
    // route. The bridge's own `/v1/responses` surface is registered
    // separately below, unauthenticated at the session-cookie layer — Codex
    // is an external process with no browser session, and gates itself with
    // its own bearer token instead (see `aionui-codex-bridge::routes`).
    let codex_bridge_config_authenticated = codex_bridge_config_routes(states.codex_bridge.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Claude bridge settings: app-facing config only, no public/unauthenticated
    // surface — unlike Codex, the resolved provider is injected directly as
    // launch-time env vars (no local HTTP proxy for Claude Code to call).
    let claude_bridge_config_authenticated = claude_bridge_config_routes(states.claude_bridge.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Built here, ahead of one-org, only because removing a member has to be
    // able to revoke that member's model channel tokens (see
    // `ModelChannelRevoker`). Its routes are still assembled further down with
    // the rest of one-devops.
    //
    // The data key is for company model channels: their credential is stored
    // encrypted and decrypted only inside the model proxy, so it never reaches
    // a member's machine.
    let one_devops_service = std::sync::Arc::new(
        one_devops::DevopsService::new(services.database.pool().clone())
            .with_encryption_key(crate::config::derive_encryption_key(&services.data_secret_raw)),
    );

    // one-org enterprise routes (/api/one/*) — RBAC extractors depend on the
    // upstream auth middleware injecting CurrentUser.
    let one_org_service = std::sync::Arc::new(
        one_org::OrgService::new(
            services.database.pool().clone(),
            services.user_repo.clone(),
            services.data_dir.clone(),
            crate::config::derive_encryption_key(&services.data_secret_raw),
        )
        .with_credential_revoker(std::sync::Arc::new(ModelChannelRevoker(one_devops_service.clone()))),
    );
    // one-enterprise service (真实企业 / company tier) — constructed here so its
    // company-admin bridges can be wired into one-org and one-sso below, and so
    // it can borrow one-org's credential revocation (built just above).
    let one_enterprise_service = std::sync::Arc::new(
        one_enterprise::EnterpriseService::new(services.database.pool().clone())
            .with_session_revoker(std::sync::Arc::new(OrgSessionRevoker(one_org_service.clone()))),
    );
    // Tenant resolver shared by one-employee + one-devops for team-shared
    // employees (A1 L3).
    let tenant_resolver: std::sync::Arc<dyn one_employee::TenantResolver> =
        std::sync::Arc::new(OrgTenantResolver(one_org_service.clone()));
    // Direction B: let a company admin create/list the project groups their
    // company owns (system_admin still governs everything as before). T6
    // stage 3: also let a project-group admin map a company directory
    // subtree into their own department tree.
    let one_org_state = one_org::OneOrgRouterState::new(one_org_service.clone())
        .with_company_admin_resolver(std::sync::Arc::new(CompanyAdminResolverAdapter(
            one_enterprise_service.clone(),
        )))
        .with_directory_source(std::sync::Arc::new(DirectoryTreeSourceAdapter(
            one_enterprise_service.clone(),
        )));
    let one_org_authenticated =
        one_org::one_org_routes(one_org_state).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-employee digital employee routes (/api/one/employee/*).
    // Wire the team session service so /run-team can drive existing team
    // slots; spawn the 30s schedule scanner for cron-driven runs.
    let one_employee_service = std::sync::Arc::new(
        one_employee::EmployeeService::new(
            services.database.pool().clone(),
            std::sync::Arc::new(services.conversation_service.clone()),
            std::sync::Arc::new(aionui_db::SqliteConversationRepository::new(
                services.database.pool().clone(),
            )),
            services.agent_registry.clone(),
            services.work_dir.clone(),
        )
        .with_team_session(team_session_service)
        // Lets an employee's aionrs model be validated against enabled
        // providers at save time instead of failing the run.
        .with_provider_repo(std::sync::Arc::new(aionui_db::SqliteProviderRepository::new(
            services.database.pool().clone(),
        ))),
    );
    one_employee_service.spawn_scheduler();
    let one_employee_state = one_employee::OneEmployeeRouterState::new(one_employee_service.clone())
        .with_tenant_resolver(tenant_resolver.clone());
    let one_employee_authenticated = one_employee::one_employee_routes(one_employee_state)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-enterprise routes (/api/one/enterprise/*) — the SSO-company
    // "enterprise org" dimension + the company tier (Direction B). The service
    // was constructed above so the company-admin bridges could be wired.
    let one_enterprise_state = one_enterprise::OneEnterpriseRouterState::new(one_enterprise_service.clone());
    let one_enterprise_authenticated = one_enterprise::one_enterprise_routes(one_enterprise_state)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-billing routes (/api/one/billing/*) — subscription tier, seats, and
    // usage dashboard. No payment provider wired: manual provisioning via
    // `PUT /tier`. The service was built above (before the conversation routes)
    // so its usage recorder could be injected there.
    let one_billing_state = one_billing::OneBillingRouterState::new(one_billing_service.clone());
    let one_billing_authenticated = one_billing::one_billing_routes(one_billing_state)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-platform routes (/api/one/admin/platform/*) — deployment infra config
    // (P1-3 container runtime + P2-2 realtime collaboration). Reserved adapters:
    // the Noop defaults report "not configured" until a real runtime/provider
    // is wired here via `with_container_runtime` / `with_collaboration_provider`.
    let one_platform_service = std::sync::Arc::new(one_platform::PlatformService::new(
        services.database.pool().clone(),
        crate::config::derive_encryption_key(&services.data_secret_raw),
    ));
    let one_platform_state = one_platform::OnePlatformRouterState::new(one_platform_service);
    let one_platform_authenticated = one_platform::one_platform_routes(one_platform_state)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-sso routes. Public half (providers/authorize/callback) is
    // unauthenticated so OAuth can run before the user has a session;
    // admin half (upsert provider) sits behind the auth middleware.
    let one_sso_state = one_sso::OneSsoRouterState::new(std::sync::Arc::new(one_sso::SsoService::new(
        services.database.pool().clone(),
        services.user_repo.clone(),
        services.jwt_service.clone(),
        services.cookie_config.clone(),
    )))
    // Enterprise-org sync: a successful SSO login upserts the caller's company
    // + membership into one-enterprise. No-op (aside from the upsert) for
    // personal edition / WebUI-only builds since it never touches
    // `one_tenants` / project-group membership.
    .with_enterprise_sync(std::sync::Arc::new(EnterpriseSyncAdapter(
        one_enterprise_service.clone(),
    )))
    // Direction B: SSO config (企业认证) is a company-level policy, so a company
    // admin may manage it. Falls back to the project-group admin when unset.
    .with_company_admin_check(std::sync::Arc::new(CompanyAdminCheckAdapter(
        one_enterprise_service.clone(),
    )))
    // P2-4 onboarding: auto-join a project group by email-domain policy. No-op
    // for logins whose IdP profile isn't email-shaped, or when no tenant has
    // `allowed_email_domains` set (the default).
    .with_org_auto_join(std::sync::Arc::new(OrgAutoJoinAdapter(one_org_service.clone())))
    // T6 directory sync: where a completed Feishu directory pull is stored.
    // Wiring it does not start anything — a pull only happens when an admin
    // asks or the scheduler fires, and both find nothing to do unless this
    // machine actually holds the company's SSO config.
    .with_directory_sink(std::sync::Arc::new(DirectorySinkAdapter(
        one_enterprise_service.clone(),
    )));
    let one_sso_public = one_sso::one_sso_public_routes(one_sso_state.clone());
    let one_sso_admin = one_sso::one_sso_admin_routes(one_sso_state)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-devops routes (/api/one/devops/*) — requirements board +
    // collaboration registries, member-writable behind auth.
    // `one_devops_service` was built earlier (see the comment there).
    // Installs whose knowledge base predates hybrid retrieval have chunks in
    // SQLite but no lexical index yet. The rebuild reads only text already
    // stored, so it costs no embedding-API calls, and it self-skips once
    // populated — a no-op on every subsequent boot and on personal installs
    // with no knowledge base at all.
    {
        let service = one_devops_service.clone();
        tokio::spawn(async move {
            match service.rebuild_lexical_index().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(chunks = n, "team knowledge lexical index built"),
                Err(e) => {
                    tracing::warn!(error = %e, "lexical index build failed; retrieval stays vector-only")
                }
            }
        });
    }
    let one_devops_state = one_devops::OneDevopsRouterState::new(one_devops_service)
        .with_employee(one_employee_service.clone())
        .with_tenant_resolver(tenant_resolver.clone());
    let one_devops_authenticated = one_devops::one_devops_routes(one_devops_state.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
    // Not session-authenticated, for the same reason the Codex bridge is not:
    // the caller is an agent process presenting a channel token, not a browser
    // with a session. See `one_devops::model_proxy`.
    let model_proxy_public = one_devops::model_proxy_routes(one_devops_state);

    // Office proxy routes — exempt from auth (serve iframe content)
    let office_proxy = office_proxy_routes(states.office);
    let public_assets = asset_routes(AssetRouterState::default());
    // Not session-authenticated: Codex CLI is an external process with no
    // browser session. Gated by its own per-installation bearer token
    // instead (checked inside the handler; see `aionui-codex-bridge`).
    let codex_bridge_public = codex_bridge_public_routes(states.codex_bridge);

    // WebSocket upgrade route — exempt from CSRF (no cookie-based
    // double-submit) but still gets security response headers.
    let ws_routes = Router::new().route("/ws", get(ws_upgrade_handler)).with_state(ws_state);
    let runtime_team_tools = runtime_team_tools_routes(RuntimeTeamToolsState {
        team_service: states.team.service.clone(),
        runtime_token_service: services.runtime_token_service.clone(),
    });
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: route groups built");

    let router = Router::new()
        .route("/health", get(health_check))
        .merge(auth_routes(auth_state))
        .merge(system_authenticated)
        .merge(conversation_authenticated)
        .merge(conversation_ops_authenticated)
        .merge(remote_agent_authenticated)
        .merge(agent_authenticated)
        .merge(connection_test_authenticated)
        .merge(file_authenticated)
        .merge(mcp_authenticated)
        .merge(extension_authenticated)
        .merge(hub_authenticated)
        .merge(skill_authenticated)
        .merge(channel_authenticated)
        .merge(team_authenticated)
        .merge(cron_authenticated)
        .merge(office_authenticated)
        .merge(shell_authenticated)
        .merge(assistant_authenticated)
        .merge(codex_bridge_config_authenticated)
        .merge(claude_bridge_config_authenticated)
        .merge(one_org_authenticated)
        .merge(one_employee_authenticated)
        .merge(one_devops_authenticated)
        .merge(one_enterprise_authenticated)
        .merge(one_billing_authenticated)
        .merge(one_platform_authenticated)
        .merge(one_sso_public)
        .merge(one_sso_admin);

    // Conditionally merge WeChat login SSE route (feature-gated)
    #[cfg(feature = "weixin")]
    let router = router.merge(weixin_login_authenticated);

    let router = if services.local {
        router
    } else {
        router.layer(middleware::from_fn_with_state(
            services.cookie_config.clone(),
            csrf_middleware,
        ))
    }
    .merge(ws_routes)
    .merge(runtime_team_tools)
    .merge(office_proxy)
    .merge(public_assets)
    .merge(codex_bridge_public)
    .merge(model_proxy_public)
    .layer(middleware::from_fn(security_headers_middleware));

    // Raise the default request body limit from axum's 2MB default to
    // `BODY_LIMIT` (10MB). Routes that need a larger cap (e.g. `/api/fs/upload`)
    // disable this default and install their own `RequestBodyLimitLayer`.
    let router = router.layer(DefaultBodyLimit::max(aionui_common::constants::BODY_LIMIT));
    let router = router.layer(middleware::from_fn(normalize_boundary_error_response));

    let router = with_access_log(router);
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route tree build with states completed"
    );

    // CORS applies in both modes. Wildcard origin without allow_credentials
    // means cross-origin requests never carry the session cookie, so the
    // cookie path stays CSRF-protected; cross-origin callers must present a
    // Bearer token explicitly. This is what lets the desktop client (file://
    // or localhost origin) talk to a remote enterprise server.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(Any);
    router.layer(cors)
}

async fn normalize_boundary_error_response(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if response.status().is_success() || response_has_json_content_type(&response) {
        return response;
    }

    let status = response.status();
    let Some((error, code)) = boundary_error_for_status(status) else {
        return response;
    };

    let original_headers = response.headers().clone();
    let mut normalized = (status, Json(ErrorResponse::new(error, code))).into_response();
    normalized.extensions_mut().insert(ApiErrorLogContext {
        code,
        message: error.to_owned(),
    });
    for (name, value) in original_headers.iter() {
        if *name != header::CONTENT_TYPE && *name != header::CONTENT_LENGTH {
            normalized.headers_mut().insert(name, value.clone());
        }
    }
    normalized
}

fn response_has_json_content_type(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("application/json"))
}

fn boundary_error_for_status(status: StatusCode) -> Option<(&'static str, &'static str)> {
    match status {
        StatusCode::BAD_REQUEST => Some(("Bad request.", "BAD_REQUEST")),
        StatusCode::UNAUTHORIZED => Some(("Unauthorized.", "UNAUTHORIZED")),
        StatusCode::FORBIDDEN => Some(("Forbidden.", "FORBIDDEN")),
        StatusCode::NOT_FOUND => Some(("Route not found.", "NOT_FOUND")),
        StatusCode::METHOD_NOT_ALLOWED => Some(("Method not allowed.", "METHOD_NOT_ALLOWED")),
        StatusCode::CONFLICT => Some(("Conflict.", "CONFLICT")),
        StatusCode::GONE => Some(("Gone.", "GONE")),
        StatusCode::PAYLOAD_TOO_LARGE => Some(("Request body is too large.", "PAYLOAD_TOO_LARGE")),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => Some(("Unsupported media type.", "UNSUPPORTED_MEDIA_TYPE")),
        StatusCode::UNPROCESSABLE_ENTITY => Some(("Unprocessable entity.", "UNPROCESSABLE_ENTITY")),
        StatusCode::TOO_MANY_REQUESTS => Some(("Rate limited", "RATE_LIMITED")),
        StatusCode::INTERNAL_SERVER_ERROR => Some(("Internal server error.", "INTERNAL_ERROR")),
        StatusCode::BAD_GATEWAY => Some(("Upstream service unavailable.", "BAD_GATEWAY")),
        StatusCode::GATEWAY_TIMEOUT => Some(("Request timed out.", "GATEWAY_TIMEOUT")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::{billing_denial, boundary_error_for_status, create_router_with_runtime};
    use crate::config::AppConfig;
    use crate::services::AppServices;

    /// The two policy refusals are written for the member and must survive with
    /// their parameters, so a client can say them in the reader's language.
    #[test]
    fn billing_policy_denials_keep_their_message_and_parameters() {
        let model = billing_denial(one_billing::BillingError::ModelNotAllowed("gpt-9".into()));
        assert_eq!(model.code, "MODEL_NOT_ALLOWED");
        assert!(model.message.contains("gpt-9"));
        assert_eq!(
            model.details.expect("model id travels as a parameter")["model"],
            "gpt-9"
        );

        let budget = billing_denial(one_billing::BillingError::BudgetExceeded);
        assert_eq!(budget.code, "BUDGET_EXCEEDED");
        assert!(budget.message.contains("budget"));
    }

    /// ⚠️ An internal failure must NOT become user-visible text.
    ///
    /// While these messages were redacted to "Forbidden." this could not bite;
    /// the moment they started reaching the user, `e.to_string()` on any error
    /// would have shipped internals (SQL, paths) straight to the UI. The gate
    /// still fails closed — it just refuses without narrating why.
    #[test]
    fn billing_internal_failure_does_not_leak_its_message() {
        let leaky = "no such table: one_licenses; DB at C:\\Users\\someone\\secret.db";
        let denial = billing_denial(one_billing::BillingError::Internal(leaky.into()));

        assert_eq!(denial.code, "POLICY_CHECK_FAILED");
        assert!(!denial.message.contains("no such table"));
        assert!(!denial.message.contains("secret.db"));
        assert!(denial.details.is_none());
    }

    #[test]
    fn boundary_error_for_status_covers_common_fallback_statuses() {
        let cases = [
            (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            (StatusCode::FORBIDDEN, "FORBIDDEN"),
            (StatusCode::NOT_FOUND, "NOT_FOUND"),
            (StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED"),
            (StatusCode::CONFLICT, "CONFLICT"),
            (StatusCode::GONE, "GONE"),
            (StatusCode::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE"),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "UNSUPPORTED_MEDIA_TYPE"),
            (StatusCode::UNPROCESSABLE_ENTITY, "UNPROCESSABLE_ENTITY"),
            (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),
            (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
            (StatusCode::BAD_GATEWAY, "BAD_GATEWAY"),
            (StatusCode::GATEWAY_TIMEOUT, "GATEWAY_TIMEOUT"),
        ];

        for (status, code) in cases {
            let (_, actual_code) = boundary_error_for_status(status).expect("status should be normalized");
            assert_eq!(actual_code, code);
        }
    }

    #[tokio::test]
    async fn create_router_with_runtime_exposes_team_service_for_background_coordinators() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();

        let (_router, _runtime) = create_router_with_runtime(&services)
            .await
            .expect("router runtime should build");
    }
}
