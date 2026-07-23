use std::collections::HashMap;

use crate::cc_switch;
use crate::manager::acp::mode_normalize::normalize_requested_mode;
use crate::shared_kernel::PersistedSessionState;
use aionui_api_types::{AcpBuildExtra, AgentMetadata};
use aionui_common::CommandSpec;
use aionui_db::CodexBridgeConfig;
use serde_json::{Map, Value, json};

const CODEX_BRIDGE_PROVIDER_ID: &str = "onework_bridge";
const CODEX_BRIDGE_TOKEN_ENV_KEY: &str = "ONEWORK_CODEX_BRIDGE_TOKEN";
/// The managed `@agentclientprotocol/codex-acp` wrapper this app spawns for
/// Codex does NOT run the real `codex` CLI's argv parser — its own CLI only
/// recognizes `login`/`cli`/`--version` in `process.argv`, so any `-c
/// key=value` flags appended to argv (the previous approach here) are
/// silently discarded. It instead reads config overrides from two env vars
/// on its own process: `CODEX_CONFIG` (a JSON object merged into the
/// session config it sends the real codex `app-server` subprocess) and
/// `MODEL_PROVIDER` (the active provider id, read separately from the
/// config JSON). All codex-specific config in this module must go through
/// those two env vars, never through CLI args.
const CODEX_CONFIG_ENV_KEY: &str = "CODEX_CONFIG";
const CODEX_MODEL_PROVIDER_ENV_KEY: &str = "MODEL_PROVIDER";

pub(super) struct AcpLaunchPolicyInput<'a> {
    pub metadata: &'a AgentMetadata,
    pub config: &'a AcpBuildExtra,
    pub session_snapshot: Option<&'a PersistedSessionState>,
    pub runtime_env: &'a [(String, String)],
    /// Resolved Codex compatibility bridge config (see
    /// `aionui-codex-bridge`), if the caller has one configured and enabled.
    /// `None` means "use Codex's own default provider" — this app never
    /// forces a provider on Codex unless the user explicitly set one up.
    pub codex_bridge_config: Option<&'a CodexBridgeConfig>,
    /// This app's own local HTTP base URL, used to point Codex at the
    /// bridge endpoint mounted on the same server. Ignored unless
    /// `codex_bridge_config` is set and enabled.
    pub local_base_url: &'a str,
    /// Resolved Claude Code custom-provider bridge env vars
    /// (`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL`), if
    /// the caller has one configured and enabled. `None` falls back to the
    /// external cc-switch file-based integration (`cc_switch::read_claude_provider_env`).
    pub claude_bridge_env: Option<&'a HashMap<String, String>>,
    /// Context-window size (tokens) of the provider/model backing an
    /// enabled Codex bridge config, if the saved provider row has one on
    /// file. Fills `CODEX_CONFIG`'s `model_context_window` so Codex doesn't
    /// have to fall back to conservative defaults for an unlisted custom
    /// model. `None` when the bridge is off/unconfigured or the provider
    /// has no context length on file.
    pub codex_bridge_context_window: Option<i64>,
}

pub(super) fn apply_acp_launch_policy(command_spec: &mut CommandSpec, input: AcpLaunchPolicyInput<'_>) {
    let mut codex_config: Map<String, Value> = Map::new();
    let mut codex_model_provider: Option<String> = None;

    apply_codex_runtime_config(
        &mut codex_config,
        input.metadata,
        initial_mode_from_build_context(input.metadata, input.config, input.session_snapshot).as_deref(),
    );
    append_runtime_env(command_spec, input.runtime_env);
    append_claude_provider_env(command_spec, input.metadata, input.claude_bridge_env);
    append_codex_bridge_config(
        &mut codex_config,
        &mut codex_model_provider,
        command_spec,
        input.metadata,
        input.codex_bridge_config,
        input.local_base_url,
        input.codex_bridge_context_window,
    );
    finalize_codex_config_env(command_spec, input.metadata, codex_config, codex_model_provider);
}

fn append_runtime_env(command_spec: &mut CommandSpec, runtime_env: &[(String, String)]) {
    for (name, value) in runtime_env {
        command_spec.env.push(aionui_common::EnvVar {
            name: name.clone(),
            value: value.clone(),
        });
    }
}

fn append_claude_provider_env(
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    claude_bridge_env: Option<&HashMap<String, String>>,
) {
    if metadata.backend.as_deref() != Some("claude") {
        return;
    }

    // Prefer this app's own bridge config (saved provider, resolved by the
    // caller) over the external cc-switch file integration — the two are
    // mutually exclusive per launch, and the first-party path needs no
    // separately-installed tool.
    let env = match claude_bridge_env {
        Some(env) if !env.is_empty() => {
            let keys: Vec<&str> = env.keys().map(|key| key.as_str()).collect();
            tracing::info!(?keys, "claude-bridge: env vars injected");
            env.clone()
        }
        _ => {
            let cc_switch_env = cc_switch::read_claude_provider_env();
            if cc_switch_env.is_empty() {
                return;
            }
            let keys: Vec<&str> = cc_switch_env.keys().map(|key| key.as_str()).collect();
            tracing::info!(?keys, "cc-switch: env vars injected");
            cc_switch_env
        }
    };

    for (name, value) in &env {
        command_spec.env.push(aionui_common::EnvVar {
            name: name.clone(),
            value: value.clone(),
        });
    }
}

/// Point Codex's `model_providers` config at the local Codex-compatibility
/// bridge (`aionui-codex-bridge`) instead of Codex's own default provider,
/// mirroring `append_claude_provider_env`'s cc_switch injection for Claude
/// Code. Only applies when the user has explicitly enabled the bridge with a
/// saved provider + model — otherwise Codex keeps behaving exactly as it
/// does today (its own ChatGPT/API-key auth).
///
/// Contributes to the shared `CODEX_CONFIG` JSON map (see
/// `CODEX_CONFIG_ENV_KEY`'s doc comment) and reports the provider id to
/// activate via `model_provider`; the caller sets that as `MODEL_PROVIDER`.
fn append_codex_bridge_config(
    codex_config: &mut Map<String, Value>,
    model_provider: &mut Option<String>,
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    codex_bridge_config: Option<&CodexBridgeConfig>,
    local_base_url: &str,
    context_window: Option<i64>,
) {
    if metadata.backend.as_deref() != Some("codex") {
        return;
    }
    let Some(config) = codex_bridge_config else { return };
    if !config.enabled {
        return;
    }
    let (Some(provider_id), Some(model)) = (config.provider_id.as_deref(), config.model.as_deref()) else {
        return;
    };

    codex_config.insert("model".to_owned(), json!(model));
    if let Some(context_window) = context_window {
        codex_config.insert("model_context_window".to_owned(), json!(context_window));
    }
    codex_config.insert(
        "model_providers".to_owned(),
        json!({
            CODEX_BRIDGE_PROVIDER_ID: {
                "name": "One Work Bridge",
                "base_url": format!("{}/v1", local_base_url.trim_end_matches('/')),
                "wire_api": "responses",
                "env_key": CODEX_BRIDGE_TOKEN_ENV_KEY,
                "requires_openai_auth": false,
            }
        }),
    );
    *model_provider = Some(CODEX_BRIDGE_PROVIDER_ID.to_owned());

    command_spec.env.push(aionui_common::EnvVar {
        name: CODEX_BRIDGE_TOKEN_ENV_KEY.to_owned(),
        value: config.bearer_token.clone(),
    });
    tracing::info!(provider_id, model, "codex-bridge: model_providers config injected");
}

fn initial_mode_from_build_context(
    metadata: &AgentMetadata,
    config: &AcpBuildExtra,
    session_snapshot: Option<&PersistedSessionState>,
) -> Option<String> {
    session_snapshot
        .and_then(|snapshot| snapshot.current_mode_id.as_ref())
        .map(|mode| normalize_requested_mode(metadata, mode.as_str()))
        .or_else(|| {
            config
                .session_mode
                .as_ref()
                .map(|mode| normalize_requested_mode(metadata, mode))
        })
        .filter(|mode| !mode.is_empty())
}

fn apply_codex_runtime_config(codex_config: &mut Map<String, Value>, metadata: &AgentMetadata, initial_mode: Option<&str>) {
    if metadata.backend.as_deref() != Some("codex") {
        return;
    }

    codex_config.insert(
        "shell_environment_policy".to_owned(),
        json!({"inherit": "all", "include_only": []}),
    );

    let sandbox_mode = codex_sandbox_mode_for_requested_mode(initial_mode);
    codex_config.insert("sandbox_mode".to_owned(), json!(sandbox_mode));
    if sandbox_mode == "danger-full-access" {
        codex_config.insert("windows".to_owned(), json!({"sandbox": "unelevated"}));
    }
}

/// Serialize the accumulated codex config overrides into the `CODEX_CONFIG`
/// / `MODEL_PROVIDER` env vars the managed codex-acp wrapper actually reads
/// (see `CODEX_CONFIG_ENV_KEY`'s doc comment). No-op for non-Codex agents or
/// when nothing was contributed.
fn finalize_codex_config_env(
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    codex_config: Map<String, Value>,
    model_provider: Option<String>,
) {
    if metadata.backend.as_deref() != Some("codex") {
        return;
    }
    if !codex_config.is_empty() {
        command_spec.env.push(aionui_common::EnvVar {
            name: CODEX_CONFIG_ENV_KEY.to_owned(),
            value: Value::Object(codex_config).to_string(),
        });
    }
    if let Some(provider) = model_provider {
        command_spec.env.push(aionui_common::EnvVar {
            name: CODEX_MODEL_PROVIDER_ENV_KEY.to_owned(),
            value: provider,
        });
    }
}

fn codex_sandbox_mode_for_requested_mode(mode: Option<&str>) -> &'static str {
    match mode.map(str::trim) {
        Some("agent-full-access" | "full-access" | "yoloNoSandbox") => "danger-full-access",
        _ => "workspace-write",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_metadata_with_backend(backend: Option<&str>) -> AgentMetadata {
        AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: backend.map(str::to_owned),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Builtin,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: None,
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: Some("agent-full-access".into()),
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
        }
    }

    fn codex_config_env_value(command_spec: &CommandSpec) -> Value {
        let raw = command_spec
            .env
            .iter()
            .find(|entry| entry.name == CODEX_CONFIG_ENV_KEY)
            .expect("CODEX_CONFIG env var set")
            .value
            .clone();
        serde_json::from_str(&raw).expect("CODEX_CONFIG is valid JSON")
    }

    fn codex_model_provider_env_value(command_spec: &CommandSpec) -> Option<String> {
        command_spec
            .env
            .iter()
            .find(|entry| entry.name == CODEX_MODEL_PROVIDER_ENV_KEY)
            .map(|entry| entry.value.clone())
    }

    #[test]
    fn apply_acp_launch_policy_adds_runtime_env_and_codex_full_access_config() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = AcpBuildExtra {
            session_mode: Some("full-access".into()),
            ..Default::default()
        };

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &config,
                session_snapshot: None,
                runtime_env: &[("AIONUI_CONVERSATION_ID".into(), "conv-1".into())],
                codex_bridge_config: None,
                local_base_url: "http://127.0.0.1:0",
                claude_bridge_env: None,
                codex_bridge_context_window: None,
            },
        );

        assert_eq!(command_spec.args, vec!["codex-acp.js"]);
        let codex_config = codex_config_env_value(&command_spec);
        assert_eq!(
            codex_config["shell_environment_policy"],
            json!({"inherit": "all", "include_only": []})
        );
        assert_eq!(codex_config["sandbox_mode"], json!("danger-full-access"));
        assert_eq!(codex_config["windows"], json!({"sandbox": "unelevated"}));
        assert!(
            command_spec
                .env
                .iter()
                .any(|entry| entry.name == "AIONUI_CONVERSATION_ID" && entry.value == "conv-1")
        );
    }

    #[test]
    fn apply_acp_launch_policy_adds_codex_full_access_config_for_agent_full_access() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = AcpBuildExtra {
            session_mode: Some("agent-full-access".into()),
            ..Default::default()
        };

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &config,
                session_snapshot: None,
                runtime_env: &[],
                codex_bridge_config: None,
                local_base_url: "http://127.0.0.1:0",
                claude_bridge_env: None,
                codex_bridge_context_window: None,
            },
        );

        let codex_config = codex_config_env_value(&command_spec);
        assert_eq!(codex_config["sandbox_mode"], json!("danger-full-access"));
        assert_eq!(codex_config["windows"], json!({"sandbox": "unelevated"}));
    }

    #[test]
    fn apply_acp_launch_policy_keeps_legacy_full_access_dangerous_for_persisted_snapshots() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let snapshot = PersistedSessionState {
            current_mode_id: Some(crate::shared_kernel::ModeId::new("full-access")),
            ..Default::default()
        };

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &AcpBuildExtra::default(),
                session_snapshot: Some(&snapshot),
                runtime_env: &[],
                codex_bridge_config: None,
                local_base_url: "http://127.0.0.1:0",
                claude_bridge_env: None,
                codex_bridge_context_window: None,
            },
        );

        let codex_config = codex_config_env_value(&command_spec);
        assert_eq!(codex_config["sandbox_mode"], json!("danger-full-access"));
        assert_eq!(codex_config["windows"], json!({"sandbox": "unelevated"}));
    }

    #[test]
    fn apply_acp_launch_policy_skips_codex_config_for_non_codex_agents() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));
        let config = AcpBuildExtra::default();

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &config,
                session_snapshot: None,
                runtime_env: &[],
                codex_bridge_config: None,
                local_base_url: "http://127.0.0.1:0",
                claude_bridge_env: None,
                codex_bridge_context_window: None,
            },
        );

        assert_eq!(command_spec.args, vec!["claude-agent-acp.js"]);
    }

    #[test]
    fn initial_mode_from_build_context_prefers_persisted_snapshot() {
        let snapshot = PersistedSessionState {
            current_mode_id: Some(crate::shared_kernel::ModeId::new("full-access")),
            ..Default::default()
        };
        let config = AcpBuildExtra {
            session_mode: Some("auto".into()),
            ..Default::default()
        };

        let mode =
            initial_mode_from_build_context(&agent_metadata_with_backend(Some("codex")), &config, Some(&snapshot));

        assert_eq!(mode.as_deref(), Some("agent-full-access"));
    }

    fn bridge_config(enabled: bool, provider_id: Option<&str>, model: Option<&str>) -> CodexBridgeConfig {
        CodexBridgeConfig {
            id: 1,
            enabled,
            provider_id: provider_id.map(str::to_owned),
            model: model.map(str::to_owned),
            bearer_token: "test-bearer-token".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn append_codex_bridge_config_injects_provider_config_when_enabled() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = bridge_config(true, Some("prov-1"), Some("kimi-k3"));
        let mut codex_config: Map<String, Value> = Map::new();
        let mut model_provider: Option<String> = None;

        append_codex_bridge_config(
            &mut codex_config,
            &mut model_provider,
            &mut command_spec,
            &metadata,
            Some(&config),
            "http://127.0.0.1:49152",
            None,
        );

        assert_eq!(model_provider.as_deref(), Some("onework_bridge"));
        assert_eq!(codex_config["model"], json!("kimi-k3"));
        assert_eq!(
            codex_config["model_providers"]["onework_bridge"]["base_url"],
            json!("http://127.0.0.1:49152/v1")
        );
        assert_eq!(codex_config["model_providers"]["onework_bridge"]["wire_api"], json!("responses"));
        assert_eq!(
            codex_config["model_providers"]["onework_bridge"]["env_key"],
            json!(CODEX_BRIDGE_TOKEN_ENV_KEY)
        );
        assert!(
            codex_config.get("model_context_window").is_none(),
            "no context window on file for the provider — must not be fabricated"
        );
        let token_env = command_spec
            .env
            .iter()
            .find(|entry| entry.name == CODEX_BRIDGE_TOKEN_ENV_KEY)
            .expect("bearer token env var injected");
        assert_eq!(token_env.value, "test-bearer-token");
    }

    #[test]
    fn append_codex_bridge_config_injects_context_window_when_provider_has_one_on_file() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = bridge_config(true, Some("prov-1"), Some("kimi-k3"));
        let mut codex_config: Map<String, Value> = Map::new();
        let mut model_provider: Option<String> = None;

        append_codex_bridge_config(
            &mut codex_config,
            &mut model_provider,
            &mut command_spec,
            &metadata,
            Some(&config),
            "http://127.0.0.1:49152",
            Some(128_000),
        );

        assert_eq!(codex_config["model_context_window"], json!(128_000));
    }

    #[test]
    fn append_codex_bridge_config_skips_when_disabled() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = bridge_config(false, Some("prov-1"), Some("kimi-k3"));
        let mut codex_config: Map<String, Value> = Map::new();
        let mut model_provider: Option<String> = None;

        append_codex_bridge_config(
            &mut codex_config,
            &mut model_provider,
            &mut command_spec,
            &metadata,
            Some(&config),
            "http://127.0.0.1:49152",
            None,
        );

        assert!(codex_config.is_empty());
        assert!(model_provider.is_none());
        assert!(command_spec.env.is_empty());
    }

    #[test]
    fn append_codex_bridge_config_skips_when_not_configured() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let mut codex_config: Map<String, Value> = Map::new();
        let mut model_provider: Option<String> = None;

        append_codex_bridge_config(
            &mut codex_config,
            &mut model_provider,
            &mut command_spec,
            &metadata,
            None,
            "http://127.0.0.1:49152",
            None,
        );

        assert!(codex_config.is_empty());
        assert!(model_provider.is_none());
    }

    #[test]
    fn append_codex_bridge_config_skips_for_non_codex_agents() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));
        let config = bridge_config(true, Some("prov-1"), Some("kimi-k3"));
        let mut codex_config: Map<String, Value> = Map::new();
        let mut model_provider: Option<String> = None;

        append_codex_bridge_config(
            &mut codex_config,
            &mut model_provider,
            &mut command_spec,
            &metadata,
            Some(&config),
            "http://127.0.0.1:49152",
            None,
        );

        assert!(codex_config.is_empty());
        assert!(model_provider.is_none());
        assert!(command_spec.env.is_empty());
    }

    #[test]
    fn apply_acp_launch_policy_sets_model_provider_env_when_bridge_enabled() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = bridge_config(true, Some("prov-1"), Some("kimi-k3"));

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &AcpBuildExtra::default(),
                session_snapshot: None,
                runtime_env: &[],
                codex_bridge_config: Some(&config),
                local_base_url: "http://127.0.0.1:49152",
                claude_bridge_env: None,
                codex_bridge_context_window: None,
            },
        );

        assert_eq!(command_spec.args, vec!["codex-acp.js"]);
        assert_eq!(codex_model_provider_env_value(&command_spec).as_deref(), Some("onework_bridge"));
        let codex_config = codex_config_env_value(&command_spec);
        assert_eq!(codex_config["model"], json!("kimi-k3"));
        assert_eq!(codex_config["sandbox_mode"], json!("workspace-write"));
    }

    #[test]
    fn apply_acp_launch_policy_skips_model_provider_env_when_bridge_disabled() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &AcpBuildExtra::default(),
                session_snapshot: None,
                runtime_env: &[],
                codex_bridge_config: None,
                local_base_url: "http://127.0.0.1:49152",
                claude_bridge_env: None,
                codex_bridge_context_window: None,
            },
        );

        assert!(codex_model_provider_env_value(&command_spec).is_none());
        let codex_config = codex_config_env_value(&command_spec);
        assert!(codex_config.get("model").is_none());
        assert!(codex_config.get("model_providers").is_none());
    }

    #[test]
    fn append_claude_provider_env_injects_resolved_bridge_env_when_present() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));
        let mut bridge_env = HashMap::new();
        bridge_env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://litellm-internal.123u.com".to_owned());
        bridge_env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-test-token".to_owned());
        bridge_env.insert("ANTHROPIC_MODEL".to_owned(), "glm-5-2".to_owned());

        append_claude_provider_env(&mut command_spec, &metadata, Some(&bridge_env));

        for (name, value) in &bridge_env {
            assert!(
                command_spec
                    .env
                    .iter()
                    .any(|entry| &entry.name == name && &entry.value == value)
            );
        }
    }

    #[test]
    fn append_claude_provider_env_falls_back_when_bridge_env_absent() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));

        // No bridge env resolved (None) falls through to
        // `cc_switch::read_claude_provider_env()`, which reads real
        // machine-local paths (`~/.cc-switch/*`) — not injectable here, so
        // this only asserts the fallback path runs without panicking
        // rather than asserting its (machine-dependent) result is empty.
        append_claude_provider_env(&mut command_spec, &metadata, None);
    }

    #[test]
    fn append_claude_provider_env_falls_back_when_bridge_env_empty() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));
        let empty_bridge_env: HashMap<String, String> = HashMap::new();

        // Same machine-dependent caveat as the `_absent` case above — an
        // empty (but present) map must be treated the same as `None`.
        append_claude_provider_env(&mut command_spec, &metadata, Some(&empty_bridge_env));
    }

    #[test]
    fn append_claude_provider_env_skips_for_non_claude_agents() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let mut bridge_env = HashMap::new();
        bridge_env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://example.com".to_owned());

        append_claude_provider_env(&mut command_spec, &metadata, Some(&bridge_env));

        assert!(command_spec.env.is_empty());
    }
}
