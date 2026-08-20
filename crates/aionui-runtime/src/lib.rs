#![warn(clippy::disallowed_types)]

//! Managed runtime and subprocess infrastructure for aioncore.

mod agent_env;
// Fork-exclusive: on-demand npm-package download/resolution for the
// claude-agent-acp / codex-acp wrapper CLIs, still the live path
// `factory/acp.rs` depends on. Not adopted into upstream's `managed_cli`
// (native-binary, prepared at package time) this sync — see the module-level
// comment in `acp_tool_runtime/mod.rs` for why.
pub mod acp_tool_runtime;
mod cache;
mod http_client;
pub mod managed_cli;
pub mod managed_resources;
pub mod managed_resources_contract;
pub use managed_resources_contract::current_runtime_key;
pub mod node_runtime;
mod registry_npx_lock;
mod resolver;
mod shell_env;

pub use acp_tool_runtime::{
    ManagedAcpToolError, ManagedAcpToolFailureKind, ManagedAcpToolId, ManagedAcpToolProgress,
    ManagedAcpToolProgressPhase, ManagedAcpToolProgressReporter, ManagedAcpToolSupport, ResolvedManagedAcpTool,
    SharedManagedAcpToolProgressReporter, doctor_snapshot as acp_tool_doctor_snapshot, ensure_managed_acp_tool,
    ensure_managed_acp_tool_with_reporter, managed_acp_tool_contract_for_export, prepare_managed_acp_tool_to_root,
    probe_managed_acp_tool_supported,
};
pub use agent_env::agent_process_env;
pub use cache::init;
pub use managed_cli::{CLAUDE_CLI_VERSION, CODEX_CLI_VERSION, cli_version, resolve_bundled_cli};
pub use managed_resources::{ManagedResourcesMode, managed_resources_mode, set_managed_resources_mode};
pub use node_runtime::{
    DoctorRow, NodeRuntimeError, NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase,
    NodeRuntimeProgressReporter, NodeRuntimeSupport, NodeTool, ResolvedCommand, ResolvedNodeRuntime,
    ResolvedNodeSource, RuntimeCommandProbe, SharedNodeRuntimeProgressReporter, doctor_snapshot,
    doctor_snapshot_for_test, ensure_node_runtime, ensure_node_runtime_with_reporter, ensure_runtime_command,
    ensure_runtime_command_with_reporter, probe_node_runtime_supported, probe_runtime_command,
};
pub use registry_npx_lock::{RegistryNpxLockError, pin_registry_npx_args, should_skip_registry_npx_version_probe};
pub use resolver::{resolve_command_in, resolve_command_path};
pub use shell_env::enhance_process_path;
mod spawn;
pub use spawn::{Builder, kill_process_tree};

#[cfg(test)]
mod test_support;
