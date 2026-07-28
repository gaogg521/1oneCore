//! Filesystem locations for the agent CLI bridges' isolated config homes.
//!
//! # Why these live here
//!
//! Two independent subsystems need to agree, byte for byte, on where a
//! bridged agent CLI reads its configuration from:
//!
//! - `aionui-ai-agent` spawns the agent and points the CLI at the isolated
//!   home via an env var (`CLAUDE_CONFIG_DIR`).
//! - `aionui-mcp` shells out to the same CLI (`claude mcp add/list/remove`)
//!   to manage MCP server registrations.
//!
//! When those two disagree, the app splits into two invisible config
//! universes: MCP servers registered by the agent land in one file while
//! the MCP management UI reads another, so each side reports the other's
//! servers as missing. Worse, the management side then defaults to the
//! operator's *real* `~/.claude.json`, which means installing or removing
//! an MCP server inside this app silently mutates the user's own Claude
//! Code installation.
//!
//! Both crates sit above `aionui-common`, so this is the lowest common
//! place that can hold the single source of truth.

use std::path::{Path, PathBuf};

/// Directory name (under the app data dir) for the Claude bridge's isolated
/// `CLAUDE_CONFIG_DIR`.
///
/// Claude Code resolves both `settings.json` and the MCP registry
/// (`.claude.json`) relative to this directory.
pub const CLAUDE_BRIDGE_HOME_DIR_NAME: &str = "claude-bridge-isolated-home";

/// Env var Claude Code reads to relocate its config home.
pub const CLAUDE_CONFIG_DIR_ENV_KEY: &str = "CLAUDE_CONFIG_DIR";

/// Resolve the Claude bridge's isolated config home under `data_dir`.
///
/// This is the only place the directory name is spelled out; every caller —
/// agent spawn and MCP CLI invocation alike — must route through here.
pub fn claude_bridge_home(data_dir: &Path) -> PathBuf {
    data_dir.join(CLAUDE_BRIDGE_HOME_DIR_NAME)
}

/// Resolve the Claude bridge home and ensure it exists on disk.
///
/// Returns the path regardless of whether creation succeeded — callers use
/// it as an env var value, and Claude Code creates missing files itself.
/// The `Err` arm is for logging: a failure here means the CLI may fall back
/// to the operator's real `~/.claude`, which is exactly what the isolation
/// exists to prevent.
pub fn ensure_claude_bridge_home(data_dir: &Path) -> (PathBuf, Result<(), std::io::Error>) {
    let path = claude_bridge_home(data_dir);
    let created = std::fs::create_dir_all(&path);
    (path, created)
}

/// Directory name (under the app data dir) for isolating `codex` CLI's MCP
/// registry writes.
///
/// **Not the same thing as "the Codex bridge"** elsewhere in this codebase
/// (`aionui-codex-bridge`, `CODEX_CONFIG`/`MODEL_PROVIDER`) — that bridge
/// injects a custom LLM provider into the spawned `codex-acp` process and is
/// unrelated to MCP server management. This constant exists purely so
/// `aionui-mcp`'s `CodexAdapter` never runs `codex mcp add/remove` against
/// the operator's real `~/.codex` (confirmed empirically: `codex mcp
/// add/list/remove` honor `CODEX_HOME` and write `<CODEX_HOME>/config.toml`
/// under `[mcp_servers.<name>]`). Deliberately **not** wired into agent
/// spawn — `install_server`/`remove_server` currently have no production
/// caller (see `CodexAdapter` docs), so isolating them doesn't touch the
/// running agent's own `CODEX_HOME`/auth lookup at all.
pub const CODEX_MCP_ISOLATED_HOME_DIR_NAME: &str = "codex-mcp-isolated-home";

/// Env var the `codex` CLI reads to relocate its config home.
pub const CODEX_HOME_ENV_KEY: &str = "CODEX_HOME";

/// Resolve the isolated `CODEX_HOME` used for MCP registry writes under
/// `data_dir`.
pub fn codex_mcp_isolated_home(data_dir: &Path) -> PathBuf {
    data_dir.join(CODEX_MCP_ISOLATED_HOME_DIR_NAME)
}

/// Resolve the isolated Codex MCP home and ensure it exists on disk.
///
/// Same shape as [`ensure_claude_bridge_home`]: returns the path regardless
/// of whether creation succeeded, since callers use it as an env var value
/// and `codex` creates `config.toml` itself on first write.
pub fn ensure_codex_mcp_isolated_home(data_dir: &Path) -> (PathBuf, Result<(), std::io::Error>) {
    let path = codex_mcp_isolated_home(data_dir);
    let created = std::fs::create_dir_all(&path);
    (path, created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_bridge_home_appends_known_dir_name() {
        let path = claude_bridge_home(Path::new("/data"));
        assert!(path.ends_with(CLAUDE_BRIDGE_HOME_DIR_NAME));
        assert!(path.starts_with("/data"));
    }

    #[test]
    fn claude_bridge_home_is_stable_across_calls() {
        let a = claude_bridge_home(Path::new("/data"));
        let b = claude_bridge_home(Path::new("/data"));
        assert_eq!(a, b);
    }

    #[test]
    fn ensure_claude_bridge_home_creates_directory() {
        let tmp = std::env::temp_dir().join(format!("aionui-bridge-home-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let (path, created) = ensure_claude_bridge_home(&tmp);
        assert!(created.is_ok(), "expected directory creation to succeed");
        assert!(path.is_dir(), "expected {} to exist", path.display());

        // Idempotent: a second call on an existing directory still succeeds.
        let (path_again, created_again) = ensure_claude_bridge_home(&tmp);
        assert!(created_again.is_ok());
        assert_eq!(path, path_again);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn env_key_matches_claude_documented_name() {
        // Guards against a typo silently disabling isolation: Claude Code
        // ignores unknown env vars, so a misspelling would fail open.
        assert_eq!(CLAUDE_CONFIG_DIR_ENV_KEY, "CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn codex_mcp_isolated_home_appends_known_dir_name() {
        let path = codex_mcp_isolated_home(Path::new("/data"));
        assert!(path.ends_with(CODEX_MCP_ISOLATED_HOME_DIR_NAME));
        assert!(path.starts_with("/data"));
    }

    #[test]
    fn codex_mcp_isolated_home_is_distinct_from_claude_bridge_home() {
        // These must never collide — they isolate two different CLIs.
        let data_dir = Path::new("/data");
        assert_ne!(claude_bridge_home(data_dir), codex_mcp_isolated_home(data_dir));
        assert_ne!(CLAUDE_BRIDGE_HOME_DIR_NAME, CODEX_MCP_ISOLATED_HOME_DIR_NAME);
    }

    #[test]
    fn ensure_codex_mcp_isolated_home_creates_directory() {
        let tmp = std::env::temp_dir().join(format!("aionui-codex-mcp-home-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let (path, created) = ensure_codex_mcp_isolated_home(&tmp);
        assert!(created.is_ok(), "expected directory creation to succeed");
        assert!(path.is_dir(), "expected {} to exist", path.display());

        let (path_again, created_again) = ensure_codex_mcp_isolated_home(&tmp);
        assert!(created_again.is_ok());
        assert_eq!(path, path_again);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn codex_env_key_matches_documented_name() {
        // Guards against a typo silently disabling isolation: `codex`
        // ignores unknown env vars, so a misspelling would fail open.
        assert_eq!(CODEX_HOME_ENV_KEY, "CODEX_HOME");
    }
}
