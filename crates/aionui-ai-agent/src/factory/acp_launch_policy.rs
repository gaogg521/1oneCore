use crate::cc_switch;
use crate::manager::acp::mode_normalize::normalize_requested_mode;
use crate::shared_kernel::PersistedSessionState;
use aionui_api_types::{AcpBuildExtra, AgentMetadata};
use aionui_common::CommandSpec;
use aionui_db::CodexBridgeConfig;

const CODEX_CONFIG_FLAG: &str = "-c";
const CODEX_ENV_POLICY_INHERIT_ALL: &str = "shell_environment_policy.inherit=all";
const CODEX_ENV_POLICY_CLEAR_INCLUDE_ONLY: &str = "shell_environment_policy.include_only=[]";
const CODEX_WINDOWS_UNELEVATED_SANDBOX: &str = "windows.sandbox=\"unelevated\"";
const CODEX_BRIDGE_PROVIDER_ID: &str = "onework_bridge";
const CODEX_BRIDGE_TOKEN_ENV_KEY: &str = "ONEWORK_CODEX_BRIDGE_TOKEN";

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
}

pub(super) fn apply_acp_launch_policy(command_spec: &mut CommandSpec, input: AcpLaunchPolicyInput<'_>) {
    apply_codex_runtime_config_args(
        command_spec,
        input.metadata,
        initial_mode_from_build_context(input.metadata, input.config, input.session_snapshot).as_deref(),
    );
    append_runtime_env(command_spec, input.runtime_env);
    append_claude_provider_env(command_spec, input.metadata);
    append_codex_bridge_env(
        command_spec,
        input.metadata,
        input.codex_bridge_config,
        input.local_base_url,
    );
}

fn append_runtime_env(command_spec: &mut CommandSpec, runtime_env: &[(String, String)]) {
    for (name, value) in runtime_env {
        command_spec.env.push(aionui_common::EnvVar {
            name: name.clone(),
            value: value.clone(),
        });
    }
}

fn append_claude_provider_env(command_spec: &mut CommandSpec, metadata: &AgentMetadata) {
    if metadata.backend.as_deref() != Some("claude") {
        return;
    }

    let cc_switch_env = cc_switch::read_claude_provider_env();
    if cc_switch_env.is_empty() {
        return;
    }

    let keys: Vec<&str> = cc_switch_env.keys().map(|key| key.as_str()).collect();
    for (name, value) in &cc_switch_env {
        command_spec.env.push(aionui_common::EnvVar {
            name: name.clone(),
            value: value.clone(),
        });
    }
    tracing::info!(?keys, "cc-switch: env vars injected");
}

/// Point Codex's `model_providers` config at the local Codex-compatibility
/// bridge (`aionui-codex-bridge`) instead of Codex's own default provider,
/// mirroring `append_claude_provider_env`'s cc_switch injection for Claude
/// Code. Only applies when the user has explicitly enabled the bridge with a
/// saved provider + model — otherwise Codex keeps behaving exactly as it
/// does today (its own ChatGPT/API-key auth).
fn append_codex_bridge_env(
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    codex_bridge_config: Option<&CodexBridgeConfig>,
    local_base_url: &str,
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

    push_codex_config_arg(command_spec, &format!("model_provider=\"{CODEX_BRIDGE_PROVIDER_ID}\""));
    push_codex_config_arg(command_spec, &format!("model=\"{model}\""));
    push_codex_config_arg(
        command_spec,
        &format!("model_providers.{CODEX_BRIDGE_PROVIDER_ID}.name=\"One Work Bridge\""),
    );
    push_codex_config_arg(
        command_spec,
        &format!(
            "model_providers.{CODEX_BRIDGE_PROVIDER_ID}.base_url=\"{}/v1\"",
            local_base_url.trim_end_matches('/')
        ),
    );
    push_codex_config_arg(
        command_spec,
        &format!("model_providers.{CODEX_BRIDGE_PROVIDER_ID}.wire_api=\"responses\""),
    );
    push_codex_config_arg(
        command_spec,
        &format!("model_providers.{CODEX_BRIDGE_PROVIDER_ID}.env_key=\"{CODEX_BRIDGE_TOKEN_ENV_KEY}\""),
    );
    push_codex_config_arg(
        command_spec,
        &format!("model_providers.{CODEX_BRIDGE_PROVIDER_ID}.requires_openai_auth=false"),
    );

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

fn apply_codex_runtime_config_args(
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    initial_mode: Option<&str>,
) {
    if metadata.backend.as_deref() != Some("codex") {
        return;
    }

    push_codex_config_arg(command_spec, CODEX_ENV_POLICY_INHERIT_ALL);
    push_codex_config_arg(command_spec, CODEX_ENV_POLICY_CLEAR_INCLUDE_ONLY);

    let sandbox_mode = codex_sandbox_mode_for_requested_mode(initial_mode);
    push_codex_config_arg(command_spec, &format!("sandbox_mode=\"{sandbox_mode}\""));
    if sandbox_mode == "danger-full-access" {
        push_codex_config_arg(command_spec, CODEX_WINDOWS_UNELEVATED_SANDBOX);
    }
}

fn push_codex_config_arg(command_spec: &mut CommandSpec, value: &str) {
    command_spec.args.push(CODEX_CONFIG_FLAG.to_owned());
    command_spec.args.push(value.to_owned());
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
            },
        );

        assert_eq!(
            command_spec.args,
            vec![
                "codex-acp.js",
                "-c",
                "shell_environment_policy.inherit=all",
                "-c",
                "shell_environment_policy.include_only=[]",
                "-c",
                "sandbox_mode=\"danger-full-access\"",
                "-c",
                "windows.sandbox=\"unelevated\"",
            ]
        );
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
            },
        );

        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "sandbox_mode=\"danger-full-access\"")
        );
        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == CODEX_WINDOWS_UNELEVATED_SANDBOX)
        );
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
            },
        );

        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "sandbox_mode=\"danger-full-access\"")
        );
        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == CODEX_WINDOWS_UNELEVATED_SANDBOX)
        );
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
    fn append_codex_bridge_env_injects_provider_config_when_enabled() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = bridge_config(true, Some("prov-1"), Some("kimi-k3"));

        append_codex_bridge_env(&mut command_spec, &metadata, Some(&config), "http://127.0.0.1:49152");

        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "model_provider=\"onework_bridge\"")
        );
        assert!(command_spec.args.iter().any(|arg| arg == "model=\"kimi-k3\""));
        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "model_providers.onework_bridge.base_url=\"http://127.0.0.1:49152/v1\"")
        );
        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "model_providers.onework_bridge.wire_api=\"responses\"")
        );
        let token_env = command_spec
            .env
            .iter()
            .find(|entry| entry.name == CODEX_BRIDGE_TOKEN_ENV_KEY)
            .expect("bearer token env var injected");
        assert_eq!(token_env.value, "test-bearer-token");
    }

    #[test]
    fn append_codex_bridge_env_skips_when_disabled() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = bridge_config(false, Some("prov-1"), Some("kimi-k3"));

        append_codex_bridge_env(&mut command_spec, &metadata, Some(&config), "http://127.0.0.1:49152");

        assert_eq!(command_spec.args, vec!["codex-acp.js"]);
        assert!(command_spec.env.is_empty());
    }

    #[test]
    fn append_codex_bridge_env_skips_when_not_configured() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));

        append_codex_bridge_env(&mut command_spec, &metadata, None, "http://127.0.0.1:49152");

        assert_eq!(command_spec.args, vec!["codex-acp.js"]);
    }

    #[test]
    fn append_codex_bridge_env_skips_for_non_codex_agents() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));
        let config = bridge_config(true, Some("prov-1"), Some("kimi-k3"));

        append_codex_bridge_env(&mut command_spec, &metadata, Some(&config), "http://127.0.0.1:49152");

        assert_eq!(command_spec.args, vec!["claude-agent-acp.js"]);
        assert!(command_spec.env.is_empty());
    }
}
