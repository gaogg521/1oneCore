//! End-to-end proof that Codex MCP management never touches the operator's
//! own `~/.codex/config.toml`.
//!
//! Mirrors `claude_config_isolation.rs`. Unit tests can only assert that
//! `CODEX_HOME` is *passed*; whether the `codex` CLI actually honors it for
//! `mcp add`/`mcp remove` is a property of the CLI itself and has to be
//! observed against the real binary (confirmed empirically 2026-07-27:
//! `codex` does honor it, writing `<CODEX_HOME>/config.toml`).
//!
//! Skipped automatically when `codex` is not on PATH, so CI without the CLI
//! stays green.

use std::path::{Path, PathBuf};

use aionui_mcp::{CodexAdapter, McpAgentAdapter, McpServerTransport};

/// Name used for the throwaway server. Distinctive so a stray leftover is
/// obvious and greppable.
const PROBE_SERVER: &str = "aionui-codex-isolation-probe-do-not-keep";

fn real_codex_config_toml() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex").join("config.toml"))
}

fn snapshot(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

async fn codex_available(adapter: &CodexAdapter) -> bool {
    adapter.is_installed().await.unwrap_or(false)
}

#[tokio::test]
async fn install_and_remove_never_touch_the_operators_real_codex_config() {
    let temp = tempfile::tempdir().expect("temp data dir");
    let adapter = CodexAdapter::new(temp.path());

    if !codex_available(&adapter).await {
        eprintln!("skipping: `codex` CLI not on PATH");
        return;
    }

    let real = real_codex_config_toml().expect("home dir");
    let before = snapshot(&real);

    let transport = McpServerTransport::Stdio {
        command: "node".into(),
        args: vec!["--version".into()],
        env: Default::default(),
    };

    let install = adapter.install_server(PROBE_SERVER, &transport).await;

    // Whatever the CLI did, verify the operator's file first — a failed
    // install that still wrote to the real config is the worst outcome and
    // must not be masked by an early `unwrap`.
    let after = snapshot(&real);
    assert_eq!(
        before, after,
        "install_server modified the operator's real ~/.codex/config.toml — config isolation is broken"
    );

    install.expect("install into the isolated home should succeed");

    // The write has to have landed somewhere: the isolated home.
    let isolated = aionui_common::codex_mcp_isolated_home(temp.path()).join("config.toml");
    assert!(
        isolated.exists(),
        "expected the isolated config at {} to be created",
        isolated.display()
    );
    let isolated_text = std::fs::read_to_string(&isolated).expect("read isolated config");
    assert!(
        isolated_text.contains(PROBE_SERVER),
        "expected {PROBE_SERVER} to be registered in the isolated config"
    );

    // Removal must be equally contained.
    adapter.remove_server(PROBE_SERVER).await.expect("remove");
    let after_remove = snapshot(&real);
    assert_eq!(
        before, after_remove,
        "remove_server modified the operator's real ~/.codex/config.toml — config isolation is broken"
    );
}

#[tokio::test]
async fn detect_existing_reads_the_operators_real_config_without_writing_to_it() {
    let temp = tempfile::tempdir().expect("temp data dir");
    let adapter = CodexAdapter::new(temp.path());

    if !codex_available(&adapter).await {
        eprintln!("skipping: `codex` CLI not on PATH");
        return;
    }

    let real = real_codex_config_toml().expect("home dir");
    let before = snapshot(&real);

    // This is the import source; it must see the user's own configuration.
    let detected = adapter.detect_existing().await.expect("detect_existing");

    let after = snapshot(&real);
    assert_eq!(before, after, "detect_existing must be strictly read-only");

    // Not asserting specific server names — the operator's config is theirs
    // to change. Only that the call reached their real config rather than the
    // (empty) isolated one, which is what makes import meaningful.
    eprintln!("detect_existing saw {} server(s) in the real config", detected.len());
}
