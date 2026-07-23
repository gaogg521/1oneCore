use std::sync::Arc;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use crate::factory::acp_launch_policy::{AcpLaunchPolicyInput, apply_acp_launch_policy};
use crate::factory::context::FactoryContext;
use crate::manager::acp::{AcpAgentManager, CatalogForwarder};
use crate::session_context::AcpSessionBuildContext;
use agent_client_protocol::schema::{EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio};
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_common::CommandSpec;
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use aionui_runtime::{
    ManagedAcpToolId, ensure_managed_acp_tool_with_reporter, ensure_node_runtime_with_reporter, ensure_runtime_command,
    ensure_runtime_command_with_reporter, resolve_command_path,
};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::runtime_status::{conversation_acp_tool_runtime_reporter, conversation_runtime_reporter};

const CLAUDE_BRIDGE_BASE_URL_ENV_KEY: &str = "ANTHROPIC_BASE_URL";
const CLAUDE_BRIDGE_AUTH_TOKEN_ENV_KEY: &str = "ANTHROPIC_AUTH_TOKEN";
const CLAUDE_BRIDGE_MODEL_ENV_KEY: &str = "ANTHROPIC_MODEL";
/// The bundled `@agentclientprotocol/claude-agent-acp` wrapper hardcodes
/// `settingSources: ["user", "project", "local"]` when it constructs the
/// real Claude Agent SDK session (confirmed by reading its own shipped
/// `acp-agent.js`) — meaning it always loads the operator's real
/// `~/.claude/settings.json`. That file commonly carries its own
/// `env.ANTHROPIC_MODEL`/`model` (from the pre-existing `cc_switch`-style
/// manual setup most users already have) and — verified empirically by
/// injecting a deliberately-invalid bridge model and observing the
/// session's own reasoning name the real settings.json model instead —
/// **wins over** whatever we inject via `command_spec.env`. The SDK does
/// expose a documented `CLAUDE_CONFIG_DIR` env var to relocate where
/// `~/.claude` is read from; pointing it at an app-private, real-settings
/// free directory is the only way (short of patching the wrapper) to make
/// our injected env vars authoritative instead of silently overridden.
const CLAUDE_BRIDGE_CONFIG_DIR_ENV_KEY: &str = "CLAUDE_CONFIG_DIR";

/// `CLAUDE_CONFIG_DIR` only isolates the settings.json *file*. The real
/// Anthropic SDK also honors `ANTHROPIC_DEFAULT_HAIKU_MODEL` /
/// `_SONNET_MODEL` / `_OPUS_MODEL` / `ANTHROPIC_SMALL_FAST_MODEL` as plain
/// process environment variables (confirmed present in the shipped SDK's own
/// string table), independent of any settings file. On this machine these
/// happen to already be ambient in the backend process's own environment
/// (inherited from the operator's real Claude Code setup one layer up the
/// process tree) and `aionui_runtime::agent_process_env()` does not strip
/// them, so they flow straight into the spawned `claude-agent-acp` child on
/// top of our injected `ANTHROPIC_MODEL` — confirmed empirically: a bridge
/// session's own bootstrap `/model haiku` resolved to the operator's real
/// settings.json alias (`ANTHROPIC_DEFAULT_HAIKU_MODEL`) even with
/// `CLAUDE_CONFIG_DIR` isolation in place. Pin all of these to the bridge's
/// own configured model so no alias tier can resolve to anything else.
const CLAUDE_BRIDGE_MODEL_ALIAS_OVERRIDE_ENV_KEYS: [&str; 4] = [
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
];

/// Resolve the Claude Code custom-provider bridge into the env vars Claude
/// Code (and the real Anthropic SDK it embeds) reads directly —
/// `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL`, plus
/// `CLAUDE_CONFIG_DIR` pointed at an isolated directory so the operator's
/// real `~/.claude/settings.json` can't silently override them (see
/// `CLAUDE_BRIDGE_CONFIG_DIR_ENV_KEY`'s doc comment). Unlike the Codex
/// bridge this needs no local proxy: Claude Code already speaks the
/// Anthropic Messages protocol, so the saved provider's real base_url/API
/// key are used as-is. Returns `None` for non-Claude agents, when the
/// bridge is disabled/unconfigured, or when the saved provider can no
/// longer be resolved (caller falls back to the external cc-switch
/// integration in that case — see `acp_launch_policy::append_claude_provider_env`).
async fn resolve_claude_bridge_env(
    deps: &AgentFactoryDeps,
    meta: &aionui_api_types::AgentMetadata,
) -> Option<HashMap<String, String>> {
    if meta.backend.as_deref() != Some("claude") {
        return None;
    }
    let repo = deps.claude_bridge_config_repo.as_ref()?;
    let config = repo.get().await.unwrap_or_else(|error| {
        warn!(error = %error, "claude-bridge: config lookup failed; falling back to cc-switch");
        None
    })?;
    if !config.enabled {
        return None;
    }
    let (provider_id, model) = (config.provider_id.as_deref()?, config.model.as_deref()?);

    let row = match deps.provider_repo.find_by_id(provider_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            warn!(
                provider_id,
                "claude-bridge: saved provider not found; falling back to cc-switch"
            );
            return None;
        }
        Err(error) => {
            warn!(error = %error, provider_id, "claude-bridge: provider lookup failed; falling back to cc-switch");
            return None;
        }
    };
    let api_key = match aionui_common::decrypt_string(&row.api_key_encrypted, &deps.encryption_key) {
        Ok(key) => key,
        Err(error) => {
            warn!(error = %error, provider_id, "claude-bridge: failed to decrypt provider API key");
            return None;
        }
    };

    let config_dir = deps.data_dir.join("claude-bridge-isolated-home");
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        warn!(
            error = %error,
            path = %config_dir.display(),
            "claude-bridge: failed to create isolated CLAUDE_CONFIG_DIR; the operator's real \
             ~/.claude/settings.json may override this bridge's env vars"
        );
    }

    let base_url = row.base_url.trim_end_matches('/').to_owned();
    let mut env = HashMap::with_capacity(4 + CLAUDE_BRIDGE_MODEL_ALIAS_OVERRIDE_ENV_KEYS.len());
    env.insert(CLAUDE_BRIDGE_BASE_URL_ENV_KEY.to_owned(), base_url);
    env.insert(CLAUDE_BRIDGE_AUTH_TOKEN_ENV_KEY.to_owned(), api_key);
    env.insert(CLAUDE_BRIDGE_MODEL_ENV_KEY.to_owned(), model.to_owned());
    env.insert(
        CLAUDE_BRIDGE_CONFIG_DIR_ENV_KEY.to_owned(),
        config_dir.to_string_lossy().into_owned(),
    );
    for key in CLAUDE_BRIDGE_MODEL_ALIAS_OVERRIDE_ENV_KEYS {
        env.insert(key.to_owned(), model.to_owned());
    }
    info!(provider_id, model, "claude-bridge: provider env resolved");
    Some(env)
}

/// Resolve the context-window size (in tokens) of the provider/model backing
/// an enabled Codex bridge config, if the saved provider row has one on
/// file. Codex's own model catalog only knows built-in OpenAI-family models
/// — pointing it at an arbitrary custom model logs a "metadata not found,
/// defaulting to fallback" warning and manages the context/output token
/// budget conservatively. `config.toml`'s top-level `model_context_window`
/// override exists precisely for this (confirmed present as a real
/// `ConfigToml` struct field by inspecting the installed `codex.exe`
/// binary's own string table, since Codex ships no machine-readable schema
/// doc). Returns `None` (and Codex keeps using its fallback default) when
/// the bridge is disabled, unconfigured, or the saved provider has no
/// context length on file — this is a value-add, not a required field.
async fn resolve_codex_bridge_context_window(
    deps: &AgentFactoryDeps,
    codex_bridge_config: Option<&aionui_db::CodexBridgeConfig>,
) -> Option<i64> {
    let config = codex_bridge_config?;
    if !config.enabled {
        return None;
    }
    let provider_id = config.provider_id.as_deref()?;
    match deps.provider_repo.find_by_id(provider_id).await {
        Ok(Some(row)) => row.context_limit,
        Ok(None) => None,
        Err(error) => {
            warn!(error = %error, provider_id, "codex-bridge: provider lookup failed while resolving context window");
            None
        }
    }
}

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AcpSessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let mut config = build_context.config;

    // Resolve the catalog row — prefer explicit agent_id, fall
    // back to a vendor-label match for legacy payloads.
    let meta = if let Some(ref agent_id) = config.agent_id {
        deps.agent_registry.get(agent_id).await
    } else if let Some(ref vendor) = config.backend {
        deps.agent_registry.find_builtin_by_backend(vendor).await
    } else {
        None
    }
    .ok_or_else(|| AgentError::bad_request("ACP agent requires either agent_id or backend in extra"))?;

    // Trust the catalog row over the client-supplied `backend` when an
    // `agent_id` was provided. The frontend collapses row-scoped rows
    // (custom ACP / remote) to a shared `custom`/`remote` slot string,
    // which downstream consumers (MCP injection, preset-context
    // composition) would mis-interpret. When the caller only supplied a
    // vendor label (builtin path), we preserve it as-is.
    if config.agent_id.is_some() || config.backend.is_none() {
        config.backend.clone_from(&meta.backend);
    }

    let mut command_spec =
        resolve_agent_command_spec(&meta, &ctx.workspace, &ctx.conversation_id, deps.broadcaster.clone()).await?;
    let codex_bridge_config = match deps.codex_bridge_config_repo.as_ref() {
        Some(repo) => repo.get().await.unwrap_or_else(|error| {
            warn!(error = %error, "codex-bridge: config lookup failed; launching Codex without it");
            None
        }),
        None => None,
    };
    let claude_bridge_env = resolve_claude_bridge_env(&deps, &meta).await;
    let codex_bridge_context_window = resolve_codex_bridge_context_window(&deps, codex_bridge_config.as_ref()).await;
    apply_acp_launch_policy(
        &mut command_spec,
        AcpLaunchPolicyInput {
            metadata: &meta,
            config: &config,
            session_snapshot: build_context.session_snapshot.as_ref(),
            runtime_env: &ctx.runtime_env,
            codex_bridge_config: codex_bridge_config.as_ref(),
            local_base_url: &deps.local_base_url,
            claude_bridge_env: claude_bridge_env.as_ref(),
            codex_bridge_context_window,
        },
    );
    let session_snapshot = build_context.session_snapshot;

    // Load user-configured MCP servers from the DB so they reach
    // ACP `session/new` mcpServers payload. Without this the agent
    // starts with zero MCP tools even when the user configured them
    // via Settings → MCP (ELECTRON-1JG).
    let mcp_capabilities = meta
        .handshake
        .agent_capabilities
        .as_ref()
        .map(parse_acp_mcp_capabilities)
        .unwrap_or_default();

    let user_mcp_servers = match deps.mcp_server_repo.as_ref() {
        Some(repo) => {
            load_user_mcp_servers(
                repo.as_ref(),
                config.mcp_server_ids.as_deref(),
                &ctx.conversation_id,
                &mcp_capabilities,
            )
            .await
        }
        None => Vec::new(),
    };
    let mut session_mcp_servers = user_mcp_servers;
    for server in &config.session_mcp_servers {
        if !session_server_supported_by_capabilities(server, &mcp_capabilities) {
            warn!(
                ctx.conversation_id,
                server_id = %server.id,
                server_name = %server.name,
                "session_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match session_server_to_sdk_mcp_server(server).await {
            Ok(server) => session_mcp_servers.push(server),
            Err(err) => {
                warn!(
                    ctx.conversation_id,
                    server_id = %server.id,
                    server_name = %server.name,
                    error = %err,
                    "session_mcp: failed to convert session snapshot; skipping"
                );
            }
        }
    }

    let params = Arc::new(
        assemble_acp_params(
            ctx.conversation_id.clone(),
            WorkspaceInfo {
                path: ctx.workspace,
                is_custom: ctx.is_custom_workspace,
            },
            meta,
            command_spec,
            config,
            session_mcp_servers,
            session_snapshot,
            deps.data_dir.clone(),
            deps.dump_prompts,
        )
        .await,
    );

    let skill_mgr = deps.skill_manager.clone();
    let catalog_tx = deps.agent_registry.catalog_sender();

    let (agent, domain_rx, notification_rx) = AcpAgentManager::build(params, skill_mgr, &catalog_tx).await?;

    let arc = Arc::new(agent);
    arc.start_permission_handler();
    arc.start_session_event_tracker(notification_rx);
    CatalogForwarder::spawn(
        arc.agent_id().to_owned(),
        crate::IAgentTask::subscribe(arc.as_ref()),
        catalog_tx,
    );

    // Desired (mode/model/config) are seeded from `params.session_snapshot`
    // inside `AcpAgentManager::new`. The CLI-assigned session id is still
    // loaded here so the first turn after a task rebuild takes the resume
    // path.
    if let Some(sid) = build_context.session_id {
        arc.set_session_id(sid).await;
    }

    // Open the ACP session eagerly so runtime preparation returns only after
    // session/new (or claude-meta-resume / session/load) and the first
    // reconcile pass have completed. Matches aionrs factory behaviour:
    // the caller sees "warmed up" == "ready for PUT /mode | /model".
    arc.warmup_session().await?;

    let instance = AgentInstance::Acp(Arc::clone(&arc));

    // Hand the service the domain event receiver so it can
    // persist user intent changes without reverse-engineering
    // them from CLI observations.
    deps.acp_agent_service.attach(ctx.conversation_id, domain_rx).await;

    Ok(instance)
}

async fn resolve_agent_command_spec(
    meta: &aionui_api_types::AgentMetadata,
    workspace: &str,
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
) -> Result<CommandSpec, AgentError> {
    if meta.agent_source == aionui_api_types::AgentSource::Builtin
        && let Some(backend) = meta.backend.as_deref()
        && let Some(tool) = ManagedAcpToolId::from_backend(backend)
    {
        return resolve_builtin_managed_acp_command_spec(meta, workspace, conversation_id, broadcaster, tool).await;
    }

    let command = meta
        .command
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::bad_request(format!("Agent '{}' has no spawn command configured", meta.name)))?;
    let reporter = conversation_runtime_reporter(broadcaster, conversation_id.to_owned());
    let resolved = ensure_runtime_command_with_reporter(command, Some(reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let mut args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let launch_args = if meta.agent_source == aionui_api_types::AgentSource::Builtin
        && meta.agent_source_info.bridge_binary.as_deref() == Some("npx")
        && let Some(backend) = meta.backend.as_deref()
    {
        aionui_runtime::pin_registry_npx_args(backend, &meta.args)
            .map_err(|error| AgentError::bad_request(format!("Agent '{}' package lock invalid: {error}", meta.name)))?
    } else {
        meta.args.clone()
    };
    args.extend(launch_args);

    let mut env: Vec<aionui_common::EnvVar> = meta
        .env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    Ok(CommandSpec {
        command: resolved.program,
        args,
        env,
        cwd: Some(workspace.to_owned()),
    })
}

async fn resolve_builtin_managed_acp_command_spec(
    meta: &aionui_api_types::AgentMetadata,
    workspace: &str,
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
    tool: ManagedAcpToolId,
) -> Result<CommandSpec, AgentError> {
    if let Some(primary) = meta.agent_source_info.binary_name.as_deref()
        && resolve_command_path(primary).is_none()
    {
        return Err(AgentError::bad_request(format!(
            "Agent '{}' requires `{primary}` to be installed and available on PATH",
            meta.name
        )));
    }

    let node_reporter = conversation_runtime_reporter(broadcaster.clone(), conversation_id.to_owned());
    let node_runtime = ensure_node_runtime_with_reporter(Some(node_reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let tool_reporter = conversation_acp_tool_runtime_reporter(broadcaster, conversation_id.to_owned(), tool);
    let managed_tool = ensure_managed_acp_tool_with_reporter(tool, Some(tool_reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let resolved = managed_tool.command(&node_runtime);

    let args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let mut env: Vec<aionui_common::EnvVar> = meta
        .env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    Ok(CommandSpec {
        command: resolved.program,
        args,
        env,
        cwd: Some(workspace.to_owned()),
    })
}

/// Load the operator's enabled MCP servers from the DB, log+skip any rows
/// whose `transport_config` JSON fails to parse (better to start without one
/// MCP tool than fail the whole session), and return them in SDK shape ready
/// for `NewSessionRequest::mcp_servers`.
///
/// When `selected_ids` is present, those rows define the session snapshot and
/// are injected regardless of the current global `enabled` flag. Legacy
/// conversations without a snapshot still fall back to "all enabled rows".
/// Builtins are wired through other paths (e.g. team/guide MCP).
async fn load_user_mcp_servers(
    repo: &dyn IMcpServerRepository,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    capabilities: &AcpMcpCapabilities,
) -> Vec<McpServer> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(ids).await,
        None => repo.list().await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(
                conversation_id,
                error = %err,
                "user_mcp: list() failed; skipping injection"
            );
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for row in rows {
        let selected = selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled);
        if !selected || row.builtin {
            continue;
        }
        if !row_supported_by_capabilities(&row, capabilities) {
            warn!(
                conversation_id,
                server_id = %row.id,
                server_name = %row.name,
                transport_type = %row.transport_type,
                "user_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match row_to_sdk_mcp_server(&row).await {
            Ok(server) => servers.push(server),
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "user_mcp: failed to convert row; skipping"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "user_mcp: injected into session/new"
        );
    }
    servers
}

/// Convert an `McpServerRow` into the SDK `McpServer` shape used by
/// `NewSessionRequest::mcp_servers`. Returns an error string when
/// `transport_config` is malformed or required fields are missing.
async fn row_to_sdk_mcp_server(row: &McpServerRow) -> Result<McpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env_entries: Vec<(String, String)> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            env_entries.sort_by(|a, b| a.0.cmp(&b.0));
            let (resolved_command, args, env) = ensure_stdio_launch(command, &args, &env_entries).await?;

            let stdio = McpServerStdio::new(row.name.clone(), resolved_command)
                .args(args)
                .env(env);
            Ok(McpServer::Stdio(stdio))
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Http(
                McpServerHttp::new(row.name.clone(), url).headers(headers),
            ))
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Sse(
                McpServerSse::new(row.name.clone(), url).headers(headers),
            ))
        }
        other => Err(format!("unknown transport type: {other}")),
    }
}

fn parse_headers(value: Option<&serde_json::Value>) -> Vec<HttpHeader> {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect()
}

async fn session_server_to_sdk_mcp_server(server: &SessionMcpServer) -> Result<McpServer, String> {
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            if command.is_empty() {
                return Err("stdio: missing command".to_owned());
            }
            let mut entries: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let (command, args, env) = ensure_stdio_launch(command, args, &entries).await?;
            Ok(McpServer::Stdio(
                McpServerStdio::new(server.name.clone(), command).args(args).env(env),
            ))
        }
        SessionMcpTransport::Http { url, headers } | SessionMcpTransport::StreamableHttp { url, headers } => {
            if url.is_empty() {
                return Err("http: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect();
            Ok(McpServer::Http(
                McpServerHttp::new(server.name.clone(), url).headers(headers),
            ))
        }
        SessionMcpTransport::Sse { url, headers } => {
            if url.is_empty() {
                return Err("sse: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect();
            Ok(McpServer::Sse(
                McpServerSse::new(server.name.clone(), url).headers(headers),
            ))
        }
    }
}

async fn ensure_stdio_launch(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<(std::path::PathBuf, Vec<String>, Vec<EnvVariable>), String> {
    let resolved = ensure_runtime_command(command)
        .await
        .map_err(|error| error.to_string())?;

    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVariable> = env
        .iter()
        .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| {
        EnvVariable::new(
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }));

    Ok((resolved.program, final_args, final_env))
}

fn row_supported_by_capabilities(row: &McpServerRow, capabilities: &AcpMcpCapabilities) -> bool {
    match row.transport_type.as_str() {
        "stdio" => capabilities.stdio,
        "http" | "streamable_http" => capabilities.http,
        "sse" => capabilities.sse,
        _ => false,
    }
}

fn session_server_supported_by_capabilities(server: &SessionMcpServer, capabilities: &AcpMcpCapabilities) -> bool {
    match server.transport {
        SessionMcpTransport::Stdio { .. } => capabilities.stdio,
        SessionMcpTransport::Http { .. } | SessionMcpTransport::StreamableHttp { .. } => capabilities.http,
        SessionMcpTransport::Sse { .. } => capabilities.sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;
    use aionui_runtime::{ManagedResourcesMode, init as init_runtime, set_managed_resources_mode};
    use std::sync::OnceLock;
    use std::{
        mem,
        path::{Path, PathBuf},
    };

    fn make_row(
        name: &str,
        transport_type: &str,
        transport_config: &str,
        enabled: bool,
        builtin: bool,
    ) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn stdio_config_for_existing_command() -> String {
        let command = std::env::current_exe()
            .expect("current test executable")
            .to_string_lossy()
            .into_owned();
        serde_json::json!({
            "command": command,
            "args": [],
            "env": {},
        })
        .to_string()
    }

    fn path_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    fn is_npx_command_path(command: &str) -> bool {
        command == "npx" || command.ends_with("/npx") || command.ends_with("\\npx.cmd")
    }

    #[cfg(unix)]
    fn test_runtime_data_dir() -> &'static PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().to_path_buf();
            mem::forget(temp);
            init_runtime(&path);
            path
        })
    }

    #[cfg(unix)]
    fn install_fake_bundled_runtime() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime_root = tmp.path().join("node").join(current_node_runtime_directory_name());
        let bin = runtime_root.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");

        for tool in ["node", "npm", "npx"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\necho v24.11.0\n").expect("write tool");
            let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }

        tmp
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-darwin-arm64"
    }

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-darwin-x64"
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-linux-arm64"
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-linux-x64"
    }

    #[cfg(all(
        unix,
        not(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "macos", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        ))
    ))]
    fn current_node_runtime_directory_name() -> &'static str {
        panic!("unsupported managed Node runtime test platform")
    }

    #[cfg(unix)]
    struct BundledRuntimeModeGuard;

    #[cfg(unix)]
    impl BundledRuntimeModeGuard {
        fn install(root: &Path) -> Self {
            unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", root) };
            set_managed_resources_mode(ManagedResourcesMode::Bundled);
            Self
        }
    }

    #[cfg(unix)]
    impl Drop for BundledRuntimeModeGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
            set_managed_resources_mode(ManagedResourcesMode::Download);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn row_to_sdk_stdio_flattens_resolved_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        let _runtime_mode = BundledRuntimeModeGuard::install(runtime.path());

        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );

        let server = row_to_sdk_mcp_server(&row).await.expect("convert");
        match server {
            McpServer::Stdio(s) => {
                let command = s.command.to_string_lossy();
                assert_ne!(command, "npx");
                assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
                assert_eq!(s.args, vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]);
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_agent_command_spec_flattens_bare_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        let _runtime_mode = BundledRuntimeModeGuard::install(runtime.path());

        let mut meta = aionui_api_types::AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("custom".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Custom,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: Some("npx".into()),
            resolved_command: None,
            args: vec!["-y".into(), "@scope/test-agent".into()],
            env: vec![aionui_api_types::AgentEnvEntry {
                name: "K".into(),
                value: "V".into(),
                description: None,
            }],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: false,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: aionui_api_types::AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        };

        let spec = resolve_agent_command_spec(
            &meta,
            "/tmp/workspace",
            "conv-acp",
            Arc::new(BroadcastEventBus::new(16)),
        )
        .await
        .expect("resolved command spec");

        let command = spec.command.to_string_lossy();
        assert_ne!(command, "npx");
        assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
        assert_eq!(spec.args, vec!["-y".to_owned(), "@scope/test-agent".to_owned()]);
        assert!(spec.env.iter().any(|entry| entry.name == "K" && entry.value == "V"));
        assert_eq!(spec.cwd.as_deref(), Some("/tmp/workspace"));

        meta.name = "Pi".into();
        meta.backend = Some("pi".into());
        meta.agent_source = aionui_api_types::AgentSource::Builtin;
        meta.agent_source_info.bridge_binary = Some("npx".into());
        meta.args = vec!["-y".into(), "pi-acp".into()];
        let spec = resolve_agent_command_spec(
            &meta,
            "/tmp/workspace",
            "conv-acp",
            Arc::new(BroadcastEventBus::new(16)),
        )
        .await
        .expect("resolved release-pinned builtin command spec");
        assert_eq!(spec.args, vec!["-y", "pi-acp@0.0.31"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn row_to_sdk_stdio_roundtrip() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        let _runtime_mode = BundledRuntimeModeGuard::install(runtime.path());

        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row).await.expect("convert");
        match server {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "ctx7");
                let command = s.command.to_string_lossy();
                assert!(
                    is_npx_command_path(&command),
                    "unexpected stdio command path: {command}",
                );
                assert_eq!(s.args, vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]);
                assert!(
                    s.env.iter().any(|entry| entry.name == "K" && entry.value == "V"),
                    "missing user-provided env in stdio launch"
                );
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[tokio::test]
    async fn row_to_sdk_http_with_headers() {
        let row = make_row(
            "remote",
            "http",
            r#"{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer tok"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row).await.expect("convert");
        match server {
            McpServer::Http(h) => {
                assert_eq!(h.name, "remote");
                assert_eq!(h.url, "https://example.com/mcp");
                assert_eq!(h.headers.len(), 1);
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[tokio::test]
    async fn row_to_sdk_unknown_transport_type_errors() {
        let row = make_row("bad", "websocket", "{}", true, false);
        assert!(row_to_sdk_mcp_server(&row).await.is_err());
    }

    #[tokio::test]
    async fn row_to_sdk_invalid_json_errors() {
        let row = make_row("bad", "stdio", "not-json", true, false);
        assert!(row_to_sdk_mcp_server(&row).await.is_err());
    }

    #[tokio::test]
    async fn row_to_sdk_stdio_missing_command_errors() {
        let row = make_row("bad", "stdio", r#"{"args":[]}"#, true, false);
        assert!(row_to_sdk_mcp_server(&row).await.is_err());
    }

    // -- load_user_mcp_servers integration -----------------------------------

    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                Err(aionui_db::DbError::Init("simulated".into()))
            } else {
                Ok(self.rows.clone())
            }
        }
        async fn find_by_id(&self, _id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn find_by_name(&self, _name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn list_by_ids_any(&self, ids: &[String]) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                return Err(aionui_db::DbError::Init("simulated".into()));
            }
            Ok(ids
                .iter()
                .filter_map(|id| self.rows.iter().find(|row| row.id == *id).cloned())
                .collect())
        }
        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
        async fn batch_upsert(
            &self,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn update_status(
            &self,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
        async fn update_tools(&self, _id: &str, _tools: Option<&str>) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_disabled_and_builtin() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("user-enabled", "stdio", &stdio_config, true, false),
                make_row("user-disabled", "stdio", &stdio_config, false, false),
                make_row(
                    "builtin",
                    "stdio",
                    r#"{"command":"img-gen","args":[],"env":{}}"#,
                    true,
                    true,
                ),
            ],
            fail: false,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "user-enabled"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_returns_empty_on_repo_failure() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![],
            fail: true,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_malformed_rows_but_keeps_others() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("good", "stdio", &stdio_config, true, false),
                make_row("bad", "stdio", "not-json", true, false),
            ],
            fail: false,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "good"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_uses_selected_snapshot_over_enabled_state() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("enabled", "stdio", &stdio_config, true, false),
                make_row("disabled-picked", "stdio", &stdio_config, false, false),
            ],
            fail: false,
        });

        let selected = vec!["mcp_disabled-picked".to_owned()];
        let servers = load_user_mcp_servers(repo.as_ref(), Some(&selected), "conv-1", &caps).await;

        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "disabled-picked"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_rows_unsupported_by_capabilities() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: true,
            sse: false,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![make_row(
                "stdio-only",
                "stdio",
                r#"{"command":"npx","args":[],"env":{}}"#,
                true,
                false,
            )],
            fail: false,
        });

        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert!(servers.is_empty());
    }
}
