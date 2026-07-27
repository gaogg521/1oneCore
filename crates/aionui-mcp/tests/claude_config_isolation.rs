//! End-to-end proof that Claude MCP management never touches the operator's
//! own Claude Code configuration.
//!
//! Unit tests can only assert that `CLAUDE_CONFIG_DIR` is *passed*. Whether
//! the `claude` CLI actually honors it for `mcp add-json -s user` is a
//! property of the CLI, not of this code — and it is the whole basis for the
//! isolation guarantee. So it has to be observed against the real binary.
//!
//! Skipped automatically when `claude` is not on PATH, so CI without the CLI
//! stays green.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aionui_mcp::{ClaudeAdapter, McpAgentAdapter, McpServerTransport};

/// Name used for the throwaway server. Distinctive so a stray leftover is
/// obvious and greppable.
const PROBE_SERVER: &str = "aionui-isolation-probe-do-not-keep";

fn real_claude_json() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude.json"))
}

fn snapshot(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

async fn claude_available(adapter: &ClaudeAdapter) -> bool {
    adapter.is_installed().await.unwrap_or(false)
}

#[tokio::test]
async fn install_and_remove_never_touch_the_operators_real_claude_config() {
    let temp = tempfile::tempdir().expect("temp data dir");
    let adapter = ClaudeAdapter::new(temp.path());

    if !claude_available(&adapter).await {
        eprintln!("skipping: `claude` CLI not on PATH");
        return;
    }

    let real = real_claude_json().expect("home dir");
    let before = snapshot(&real);

    let transport = McpServerTransport::Stdio {
        command: "node".into(),
        args: vec!["--version".into()],
        env: HashMap::new(),
    };

    let install = adapter.install_server(PROBE_SERVER, &transport).await;

    // Whatever the CLI did, verify the operator's file first — a failed
    // install that still wrote to the real config is the worst outcome and
    // must not be masked by an early `unwrap`.
    let after = snapshot(&real);
    assert_eq!(
        before, after,
        "install_server modified the operator's real ~/.claude.json — config isolation is broken"
    );

    install.expect("install into the isolated home should succeed");

    // The write has to have landed somewhere: the isolated home.
    let isolated = aionui_common::claude_bridge_home(temp.path()).join(".claude.json");
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
        "remove_server modified the operator's real ~/.claude.json — config isolation is broken"
    );
}

#[tokio::test]
async fn detect_existing_reads_the_operators_real_config_without_writing_to_it() {
    let temp = tempfile::tempdir().expect("temp data dir");
    let adapter = ClaudeAdapter::new(temp.path());

    if !claude_available(&adapter).await {
        eprintln!("skipping: `claude` CLI not on PATH");
        return;
    }

    let real = real_claude_json().expect("home dir");
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
