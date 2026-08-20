//! Top-level router assembly: middleware stack + module route merges.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::http::{HeaderName, Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use aionui_ai_agent::{
    RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION, agent_routes, remote_agent_routes,
};
use aionui_api_types::{ErrorResponse, WebSocketMessage};
use aionui_assets::{AssetRouterState, asset_routes};
use aionui_assistant::assistant_routes;
use aionui_auth::{
    AuthIdentityMode, AuthRouterState, AuthState, IRuntimeTokenVerifier, SystemDefaultFilesystemAdopter,
    auth_middleware, auth_routes, csrf_middleware, security_headers_middleware,
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
use aionui_project::project_routes;
use aionui_realtime::{NoopMessageRouter, WebSocketManager, WsHandlerState, ws_upgrade_handler};
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

/// Lets disbanding a company delete what it owns in one-org (every project
/// group) and one-billing (every usage/license record) without
/// one-enterprise depending on either (same layer). Best-effort per side, by
/// the trait's own contract — a company the operator asked to disband must
/// actually go away even if one side's cleanup hits an error.
struct CompanyDisbandCascadeImpl {
    org: std::sync::Arc<one_org::OrgService>,
    billing: std::sync::Arc<one_billing::BillingService>,
}

#[async_trait::async_trait]
impl one_enterprise::CompanyDisbandCascade for CompanyDisbandCascadeImpl {
    async fn disband(&self, enterprise_id: &str) -> Vec<String> {
        if let Err(error) = self.billing.delete_enterprise_billing_data(enterprise_id).await {
            tracing::error!(%error, enterprise_id, "failed to delete enterprise billing data on disband");
        }
        match self.org.disband_tenants_for_enterprise(enterprise_id).await {
            Ok(deleted) => deleted,
            Err(error) => {
                tracing::error!(%error, enterprise_id, "failed to disband project groups on company disband");
                Vec::new()
            }
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
        personal_external_id: &str,
        display_name: Option<&str>,
        department: Option<&str>,
        job_title: Option<&str>,
    ) {
        if let Err(error) = self
            .0
            .sync_member(
                user_id,
                provider,
                external_id,
                personal_external_id,
                display_name,
                department,
                job_title,
            )
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

/// Adapts one-enterprise's `EnterpriseService::ensure_member` to the
/// `one_org::CompanySeatSync` trait, so joining a project group that belongs
/// to a company also registers the joiner as a company member (seat-capped,
/// same rule as SSO auto-provisioning) — see `one_org::enterprise_hooks`
/// module docs for why this exists.
struct CompanySeatSyncAdapter(std::sync::Arc<one_enterprise::EnterpriseService>);

#[async_trait::async_trait]
impl one_org::CompanySeatSync for CompanySeatSyncAdapter {
    async fn ensure_company_member(&self, user_id: &str, enterprise_id: &str, display_name: Option<&str>) {
        if let Err(error) = self.0.ensure_member(user_id, enterprise_id, display_name).await {
            tracing::warn!(%error, user_id, enterprise_id, "company seat sync failed; project-group join continues");
        }
    }

    async fn release_company_member(&self, user_id: &str, enterprise_id: &str) {
        if let Err(error) = self.0.leave_company(enterprise_id, user_id).await {
            // MemberNotFound is expected and not worth a warning: not every
            // project-group joiner ever synced into the company (the hook
            // this mirrors is best-effort too), and LastCompanyAdmin is a
            // real, intentional refusal — the seat is deliberately kept
            // occupied rather than leaving the company with no admin.
            if !matches!(
                error,
                one_enterprise::EnterpriseError::MemberNotFound | one_enterprise::EnterpriseError::LastCompanyAdmin
            ) {
                tracing::warn!(%error, user_id, enterprise_id, "company seat release failed; project-group leave continues");
            }
        }
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

    async fn company_exists(&self) -> bool {
        self.0.company_exists().await.unwrap_or(false)
    }
}

/// Adapts one-platform's per-project-group IP allowlist to the auth
/// middleware's `IpAllowlistGate` trait — same-layer bridge, same
/// arrangement as every other adapter in this file.
///
/// A caller with nothing to check (no resolvable project group — personal
/// edition, or an enterprise account with no membership — or a group whose
/// allowlist is disabled, the reserved default) is always allowed regardless
/// of whether `ip` resolved. Only once a group's allowlist is actually
/// enabled does an unresolvable `ip` become a denial: see the trait's own
/// doc comment for why getting this ordering backwards would 403 every test
/// (and every personal-edition install) that never wired a real
/// `ConnectInfo`.
struct PlatformIpAllowlistGate(std::sync::Arc<one_platform::PlatformService>);

#[async_trait::async_trait]
impl aionui_auth::IpAllowlistGate for PlatformIpAllowlistGate {
    async fn is_allowed(&self, user_id: &str, ip: Option<std::net::IpAddr>) -> Result<bool, String> {
        let actor = self.0.resolve_actor(user_id).await.map_err(|e| e.to_string())?;
        let Some(actor) = actor else {
            return Ok(true);
        };
        match ip {
            Some(ip) => self
                .0
                .is_ip_allowed(&actor.tenant_id, &ip.to_string())
                .await
                .map_err(|e| e.to_string()),
            None => {
                let cfg = self
                    .0
                    .get_ip_allowlist(&actor.tenant_id)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(!cfg.enabled)
            }
        }
    }
}

/// Adapts one-billing's `BillingService::record_turn` to the conversation
/// crate's `UsageRecorder` trait (P0-3). Fire-and-forget: spawns the async
/// insert so metering never blocks or fails the send path.
///
/// `model`/tokens flow through from `ConversationTurnOrchestrator`, which
/// only calls this once a turn has actually completed — real cost is known
/// only then, never at accept time (see the trait's own doc comment for why
/// that used to make every chat turn cost $0 regardless of the real bill).
struct BillingUsageRecorder(std::sync::Arc<one_billing::BillingService>);

impl aionui_conversation::UsageRecorder for BillingUsageRecorder {
    fn record_turn(
        &self,
        user_id: String,
        conversation_id: String,
        model: Option<String>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    ) {
        let service = self.0.clone();
        tokio::spawn(async move {
            if let Err(e) = service
                .record_turn(
                    &user_id,
                    Some(&conversation_id),
                    model.as_deref(),
                    input_tokens,
                    output_tokens,
                )
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
        // T7: a tighter cap layered under the company-wide budget, scoped to
        // this member's department. Distinct code so the UI can point at the
        // department's own budget panel rather than the company one.
        BillingError::DepartmentBudgetExceeded => aionui_conversation::PolicyDenial::new(
            "DEPARTMENT_BUDGET_EXCEEDED",
            "This department's usage budget for this period has been reached",
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

/// Adapts one-billing's `check_model_allowed` to the agent factory's
/// `ModelAllowlistGate` (P0, vision delegate).
///
/// Allowlist-only on purpose. The delegate is resolved once when the agent is
/// built and then reused for the whole session, so a budget check here would
/// answer a question about one instant and cache it for hours — and the send
/// path already checks the budget per turn. The delegate's *cost* is handled
/// separately, by metering the call so the cap can see it at all.
///
/// `pub(crate)` because `services.rs` builds the factory long before any router
/// exists; this is the same adapter, wired earlier.
pub(crate) struct BillingModelAllowlistGate(pub(crate) std::sync::Arc<one_billing::BillingService>);

#[async_trait::async_trait]
impl aionui_ai_agent::ModelAllowlistGate for BillingModelAllowlistGate {
    async fn is_model_allowed(&self, user_id: &str, model: &str) -> Result<bool, String> {
        match self.0.check_model_allowed(user_id, model).await {
            Ok(()) => Ok(true),
            // The two policy refusals: the admin's allowlist, and a member who
            // arrived after the seat cap filled (T6-4 — governed by nothing, so
            // denied outright rather than falling through an empty allowlist).
            Err(one_billing::BillingError::ModelNotAllowed(_) | one_billing::BillingError::SeatLimitExceeded) => {
                Ok(false)
            }
            // Anything else is the check failing, not the policy passing. The
            // caller fails closed on `Err`; same posture as `BillingSendGate`.
            Err(other) => Err(other.to_string()),
        }
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
use super::fs_monitor::spawn_fs_monitor;
use super::health::health_check;
use super::runtime_team_tools::{RuntimeTeamToolsState, runtime_team_tools_routes};
use super::scm_monitor::{CompositeMessageRouter, spawn_scm_monitor};
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

async fn forward_event_bus_to_websocket(
    mut event_rx: tokio::sync::broadcast::Receiver<WebSocketMessage<serde_json::Value>>,
    ws_manager: Arc<WebSocketManager>,
) {
    loop {
        let event = match event_rx.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "websocket event bus bridge lagged; skipped stale events and will continue"
                );
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        if let Some(user_id) = event
            .data
            .get("user_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
        {
            ws_manager.broadcast_to_user(&user_id, event);
        } else if is_global_websocket_event(&event.name) {
            ws_manager.broadcast_all(event);
        } else {
            tracing::warn!(
                event_name = %event.name,
                "dropping websocket event without user_id; add user_id to payload or whitelist explicit global event"
            );
        }
    }
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
    let event_rx = services.event_bus.subscribe();
    let ws_manager = services.ws_manager.clone();
    tokio::spawn(forward_event_bus_to_websocket(event_rx, ws_manager));

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
    let chan_owner_user_id = channel_components.owner_user_id;
    tokio::spawn(async move {
        if let Some(chan_owner_user_id) = chan_owner_user_id {
            if let Err(e) = chan_mgr.restore_plugins(&chan_owner_user_id, &chan_factory).await {
                tracing::warn!(
                    code = "BOOTSTRAP_DEGRADED_CHANNEL_RESTORE",
                    stage = "channel.restore",
                    owner_user_id = %chan_owner_user_id,
                    error = %e,
                    "failed to restore channel plugins"
                );
            }
        } else {
            tracing::info!(
                stage = "channel.restore",
                "skipping channel plugin restore until an owner user is available"
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
    // Spawn the Project Explorer filesystem monitor and install its inbound
    // router (fs/* frames). Built here — inside the runtime — because the actor
    // runs as a background task. The sync test-only assembly path keeps a no-op.
    let fs_router = spawn_fs_monitor(Arc::new(services.project_service.clone()), services.ws_manager.clone());
    // Source control shares the connection but owns its own envelope name, so the
    // two inbound routers are composed behind the realtime layer's single slot.
    let scm_router = spawn_scm_monitor(Arc::new(services.project_service.clone()), services.ws_manager.clone());
    let inbound_router: Arc<dyn aionui_realtime::MessageRouter> = match scm_router {
        Some(scm) => Arc::new(CompositeMessageRouter::new(vec![fs_router, scm])),
        None => fs_router,
    };
    let ws_state = build_ws_state(services, inbound_router);
    let router = create_router_with_all_state(services, states, ws_state);
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
    // No-op inbound router: this sync assembly path is for HTTP-focused tests and
    // does not spawn the fs monitor (which requires a runtime task).
    let ws_state = build_ws_state(services, Arc::new(NoopMessageRouter));
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
        fs_adopter: Some(Arc::new(SkillFilesystemAdopter {
            skill_paths: services.skill_paths.clone(),
            skill_repo: services.skill_repo.clone(),
        })),
        cookie_config: services.cookie_config.clone(),
        qr_token_store: services.qr_token_store.clone(),
        identity_mode: auth_identity_mode(services.identity_mode),
        bootstrap_secret: services.bootstrap_secret.clone(),
        session_revoked_hook: {
            let ws_manager = services.ws_manager.clone();
            let conversation_service = states.conversation.service.clone();
            let team_service = states.team.service.clone();
            let channel_manager = states.channel.manager.clone();
            let channel_session_manager = states.channel.session_manager.clone();
            let file_watch_service = states.file.watch_service.clone();
            let office_watch_manager = states.office.watch_manager.clone();
            Some(Arc::new(move |user_id: &str| {
                ws_manager.disconnect_user(user_id, "session revoked");
                let stopped_team_sessions = team_service.stop_sessions_for_user(user_id);
                if stopped_team_sessions > 0 {
                    tracing::info!(
                        user_id = %user_id,
                        stopped_team_sessions,
                        "stopped team sessions after session revocation"
                    );
                }
                let user_id = user_id.to_owned();
                let conversation_service = conversation_service.clone();
                let channel_manager = channel_manager.clone();
                let channel_session_manager = channel_session_manager.clone();
                let file_watch_service = file_watch_service.clone();
                let office_watch_manager = office_watch_manager.clone();
                tokio::spawn(async move {
                    channel_manager.shutdown_for_user(&user_id).await;
                    office_watch_manager.stop_all_for_user(&user_id);
                    if let Err(err) = channel_session_manager.clear_all_sessions(&user_id).await {
                        tracing::warn!(
                            user_id = %user_id,
                            error = %err,
                            "failed to clear channel sessions after session revocation"
                        );
                    }
                    if let Err(err) = conversation_service.terminate_runtime_for_user(&user_id).await {
                        tracing::warn!(
                            user_id = %user_id,
                            error = %err,
                            "failed to terminate runtimes after session revocation"
                        );
                    }
                    if let Err(err) = file_watch_service.stop_all_watches_for_user(&user_id).await {
                        tracing::warn!(
                            user_id = %user_id,
                            error = %err,
                            "failed to stop file watches after session revocation"
                        );
                    }
                    if let Err(err) = file_watch_service.stop_all_office_watches_for_user(&user_id).await {
                        tracing::warn!(
                            user_id = %user_id,
                            error = %err,
                            "failed to stop office file watches after session revocation"
                        );
                    }
                });
            }))
        },
        local: services.local,
        aionpro_mode: services.identity_mode == crate::config::IdentityMode::AionPro,
    };

    // one-platform service (IP allowlist among other deployment-infra config)
    // — built here, ahead of its own route mount further down, purely so the
    // auth middleware can wire the allowlist check into every route group
    // through a single shared `AuthState`. Cheap to construct this early:
    // only needs the pool + encryption key, both already on `services`.
    let one_platform_service = std::sync::Arc::new(one_platform::PlatformService::new(
        services.database.pool().clone(),
        crate::config::derive_encryption_key(&services.data_secret_raw),
    ));

    let auth_mw_state = AuthState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        identity_mode: auth_identity_mode(services.identity_mode),
        runtime_token_verifier: Some(Arc::new(ConversationHelperTokenVerifier {
            runtime_token_service: services.runtime_token_service.clone(),
        })),
        ip_allowlist: Some(std::sync::Arc::new(PlatformIpAllowlistGate(
            one_platform_service.clone(),
        ))),
    };

    // System routes protected by auth middleware
    let system_authenticated =
        system_routes(states.system).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // one-billing service (subscription tier / seats / usage). Built in
    // `AppServices` rather than here: the agent factory is assembled before any
    // router exists and needs the same instance for the vision-delegate model
    // allowlist (`BillingModelAllowlistGate`). It stays dependency-free (pool +
    // manual provider), so nothing about the construction order changes.
    let one_billing_service = services.billing.clone();

    // P0-3 usage metering: wired onto the shared `ConversationService`
    // (interior-mutability setter, like `with_project_service` below) rather
    // than the per-router-mount `ConversationRouterState` builder chain —
    // `ConversationTurnOrchestrator` fires this from inside the service
    // itself once a turn actually completes, so every clone of the service
    // (HTTP routes, cron, team) needs to see the same wiring, not just
    // whichever local variable this chain happens to run through.
    states
        .conversation
        .service
        .with_usage_recorder(std::sync::Arc::new(BillingUsageRecorder(one_billing_service.clone())));

    // Conversation routes protected by auth middleware.
    let conversation_authenticated = conversation_routes(
        states
            .conversation
            .clone()
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

    // Project control-plane routes protected by auth middleware
    let project_authenticated =
        project_routes(states.project).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

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
            .with_session_revoker(std::sync::Arc::new(OrgSessionRevoker(one_org_service.clone())))
            .with_disband_cascade(std::sync::Arc::new(CompanyDisbandCascadeImpl {
                org: one_org_service.clone(),
                billing: one_billing_service.clone(),
            })),
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
        )))
        // Direction B: a project-group join whose tenant belongs to a company
        // also registers the joiner as a company member (see
        // `CompanySeatSyncAdapter` / `one_org::enterprise_hooks` docs).
        .with_company_seat_sync(std::sync::Arc::new(CompanySeatSyncAdapter(
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
    // Service itself was built above, alongside `auth_mw_state`, so its IP
    // allowlist could be wired into the auth middleware.
    let one_platform_state = one_platform::OnePlatformRouterState::new(one_platform_service.clone());
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
    // Office proxy routes serve iframe content but still require auth so
    // preview ports remain scoped to the active Core user.
    let office_proxy =
        office_proxy_routes(states.office).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
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

    // Antigravity permission hook callback. Deliberately NOT behind
    // auth_middleware: the hook is a local process with no user session and
    // presents a per-conversation token instead (checked in the handler).
    let antigravity_hook = crate::router::antigravity_hook::antigravity_hook_routes(
        crate::router::antigravity_hook::AntigravityHookRouterState {
            task_manager: services.worker_task_manager.clone(),
            tokens: services.antigravity_hook_tokens.clone(),
        },
    );

    let router = Router::new()
        .merge(antigravity_hook)
        .route("/health", get(health_check))
        .merge(auth_routes(auth_state))
        .merge(system_authenticated)
        .merge(conversation_authenticated)
        .merge(conversation_ops_authenticated)
        .merge(remote_agent_authenticated)
        .merge(agent_authenticated)
        .merge(connection_test_authenticated)
        .merge(file_authenticated)
        .merge(project_authenticated)
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

    let router = if services.identity_mode.is_local() {
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

    // Local mode keeps the wildcard origin: without allow_credentials a
    // cross-origin request never carries the session cookie, so the cookie
    // path stays CSRF-protected and cross-origin callers must present a
    // Bearer token explicitly. That is what lets the desktop client (file://
    // or localhost origin) talk to a remote enterprise server.
    if services.identity_mode.is_local() {
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
    } else {
        // Non-local (external identity) mode: the desktop renderer is a
        // cross-origin browser context (localhost:5173 in dev, file:// when
        // packaged) authenticating with the session cookie, so responses must
        // opt in to credentialed CORS. Credentialed mode forbids wildcards:
        // reflect the request origin and enumerate headers explicitly
        // (x-csrf-token is required by the CSRF double-submit middleware).
        let cors = CorsLayer::new()
            .allow_origin(AllowOrigin::mirror_request())
            .allow_credentials(true)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
                HeaderName::from_static("x-csrf-token"),
            ]);
        router.layer(cors)
    }
}

/// Adapter running the on-disk side of AionUi → AionPro adoption over the
/// skill filesystem (auth crate cannot depend on the extension/filesystem).
struct SkillFilesystemAdopter {
    skill_paths: Arc<aionui_extension::SkillPaths>,
    skill_repo: Arc<dyn aionui_db::ISkillRepository>,
}

#[async_trait::async_trait]
impl SystemDefaultFilesystemAdopter for SkillFilesystemAdopter {
    async fn adopt_filesystem(&self, adopter_user_id: &str) {
        aionui_extension::fs_adopt::adopt_user_filesystem(
            self.skill_paths.as_ref(),
            self.skill_repo.as_ref(),
            adopter_user_id,
        )
        .await;
    }
}

/// Adapter exposing the agent runtime's token service to the auth middleware
/// as the conversation-helper credential channel (aionui-auth cannot depend on
/// aionui-ai-agent, so the binding happens here in the composition layer).
struct ConversationHelperTokenVerifier {
    runtime_token_service: Arc<RuntimeTokenService>,
}

impl IRuntimeTokenVerifier for ConversationHelperTokenVerifier {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool {
        self.runtime_token_service
            .validate(
                Some(token),
                user_id,
                conversation_id,
                RuntimeTokenScope::ConversationHelper,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .is_ok()
    }
}

fn auth_identity_mode(identity_mode: crate::config::IdentityMode) -> AuthIdentityMode {
    match identity_mode {
        crate::config::IdentityMode::Local => AuthIdentityMode::Local,
        crate::config::IdentityMode::WebUi => AuthIdentityMode::UserSession,
        crate::config::IdentityMode::AionPro => AuthIdentityMode::AionPro,
    }
}

fn is_global_websocket_event(event_name: &str) -> bool {
    matches!(
        event_name,
        "runtime.statusChanged" | "extensions.lifecycle" | "hub.state-changed"
    )
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
    use std::sync::Arc;

    use aionui_api_types::WebSocketMessage;
    use aionui_realtime::{BroadcastEventBus, EventBroadcaster, WebSocketManager, WsOutbound};
    use axum::http::StatusCode;
    use serde_json::json;

    use super::{
        billing_denial, boundary_error_for_status, create_router_with_runtime, forward_event_bus_to_websocket,
        is_global_websocket_event,
    };
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

    #[test]
    fn extension_enablement_events_are_user_scoped() {
        assert!(!is_global_websocket_event("extensions.state-changed"));
        assert!(is_global_websocket_event("extensions.lifecycle"));
    }

    #[tokio::test]
    async fn create_router_with_runtime_exposes_team_service_for_background_coordinators() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();

        let (_router, _runtime) = create_router_with_runtime(&services)
            .await
            .expect("router runtime should build");
    }

    #[tokio::test]
    async fn websocket_event_bridge_continues_after_receiver_lag() {
        let event_bus = BroadcastEventBus::new(2);
        let event_rx = event_bus.subscribe();
        let ws_manager = Arc::new(WebSocketManager::new());
        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel(8);
        ws_manager.add_client_for_user("user-a".into(), "token".into(), outbound_tx);

        for sequence in 1..=3 {
            event_bus.broadcast(WebSocketMessage::new(
                "test.beforeLag",
                json!({"user_id": "user-a", "sequence": sequence}),
            ));
        }

        let bridge = tokio::spawn(forward_event_bus_to_websocket(event_rx, ws_manager));
        event_bus.broadcast(WebSocketMessage::new(
            "test.afterLag",
            json!({"user_id": "user-a", "sequence": 4}),
        ));

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                match outbound_rx.recv().await {
                    Some(WsOutbound::Text(text)) => {
                        let event: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if event["name"] == "test.afterLag" {
                            break event;
                        }
                    }
                    other => panic!("expected a text websocket event, got {other:?}"),
                }
            }
        })
        .await
        .expect("the bridge should deliver events after recovering from lag");

        assert_eq!(delivered["data"]["sequence"], 4);
        bridge.abort();
    }
}
