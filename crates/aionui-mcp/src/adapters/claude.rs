use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aionui_common::McpSource;

use crate::adapter::{DetectedServer, McpAgentAdapter};
use crate::error::McpError;
use crate::types::McpServerTransport;

use super::cli_helpers::{
    DETECT_TIMEOUT, INHERIT_ENV, MUTATE_TIMEOUT, is_cli_installed, normalize_detection_status, run_cli_with_env,
    strip_ansi,
};

const CLI_NAME: &str = "claude";

/// Scopes to try when removing a server (user → local → project).
const REMOVE_SCOPES: &[&str] = &["user", "local", "project"];

/// MCP Agent adapter for Claude CLI.
///
/// # Config isolation: reads and writes target different homes on purpose
///
/// | Operation | `CLAUDE_CONFIG_DIR` | Why |
/// |---|---|---|
/// | [`Self::detect_existing`] | the operator's real home | It is the *import source* — the whole point is to discover what the user already configured in their own Claude Code. Read-only, so it cannot damage anything. |
/// | [`Self::detect_managed`] | bridge isolated home | Reports what this app's own agent actually has registered. |
/// | `install_server` / `remove_server` | bridge isolated home | **Never** the real home. |
///
/// The mutation rule is load-bearing, not hygiene. Both use `-s user`, so
/// without the override, managing an MCP server *inside this app* would add
/// to or delete from the user's own Claude Code installation — and a
/// malformed server installed here would then break `claude` in their
/// terminal too, long after they closed this app.
///
/// The isolated home is [`aionui_common::claude_bridge_home`] — the same
/// directory the agent factory hands to `claude-agent-acp` at spawn time.
/// If these two ever drift apart, MCP servers registered here become
/// invisible to the agent that is supposed to use them.
///
/// # CLI Commands
///
/// - **detect**: `claude mcp list`
/// - **install (stdio)**: `claude mcp add-json -s user <name> <json>`
/// - **install (http/sse)**: `claude mcp add -s user --transport <type> <name> <url> [--header ...]`
/// - **remove**: `claude mcp remove -s <scope> <name>` (tries user → local → project)
///
/// Claude's list output uses a custom format:
/// `name: command args - ✓ Connected` or `name: command args - ✗ Failed`
pub struct ClaudeAdapter {
    /// Isolated `CLAUDE_CONFIG_DIR` this adapter operates on.
    config_dir: PathBuf,
}

impl ClaudeAdapter {
    /// Build an adapter bound to the Claude bridge's isolated config home
    /// under `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            config_dir: aionui_common::claude_bridge_home(data_dir),
        }
    }

    /// Env overrides pinning an invocation to the bridge's isolated home.
    ///
    /// Required for every **mutating** command. Read commands choose
    /// deliberately — see the struct docs.
    fn isolated_env(&self) -> [(&'static str, String); 1] {
        [(
            aionui_common::CLAUDE_CONFIG_DIR_ENV_KEY,
            self.config_dir.to_string_lossy().into_owned(),
        )]
    }

    /// List MCP servers registered in *this app's* isolated Claude home.
    ///
    /// Distinct from [`Self::detect_existing`], which reports the user's own
    /// Claude Code configuration. This is what the agent spawned by this app
    /// actually sees — including servers an agent registered for itself by
    /// running `claude mcp add` mid-session.
    pub async fn detect_managed(&self) -> Result<Vec<DetectedServer>, McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        let (stdout, _stderr) =
            run_cli_with_env(CLI_NAME, &["mcp", "list"], &self.isolated_env(), DETECT_TIMEOUT).await?;
        Ok(parse_claude_list_output(&stdout))
    }
}

#[async_trait::async_trait]
impl McpAgentAdapter for ClaudeAdapter {
    fn source(&self) -> McpSource {
        McpSource::Claude
    }

    async fn is_installed(&self) -> Result<bool, McpError> {
        is_cli_installed(CLI_NAME).await
    }

    /// Read the operator's **real** Claude Code configuration.
    ///
    /// Deliberately not isolated: this feeds `GET /api/mcp/agent-configs`,
    /// whose purpose is to let the user import MCP servers they already set
    /// up in their own Claude Code. Isolating it would make the import
    /// feature scan this app's own registry and find nothing to import.
    ///
    /// Safe because it is strictly read-only — `mcp list` never writes. All
    /// mutations go to the isolated home instead (see struct docs).
    async fn detect_existing(&self) -> Result<Vec<DetectedServer>, McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        let (stdout, _stderr) = run_cli_with_env(CLI_NAME, &["mcp", "list"], INHERIT_ENV, DETECT_TIMEOUT).await?;
        Ok(parse_claude_list_output(&stdout))
    }

    async fn install_server(&self, name: &str, transport: &McpServerTransport) -> Result<(), McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        let env = self.isolated_env();

        match transport {
            McpServerTransport::Stdio {
                command,
                args,
                env: server_env,
            } => {
                let config = build_stdio_json(command, args, server_env);
                let config_str =
                    serde_json::to_string(&config).map_err(|e| McpError::AgentOperationFailed(e.to_string()))?;
                run_cli_with_env(
                    CLI_NAME,
                    &["mcp", "add-json", "-s", "user", name, &config_str],
                    &env,
                    MUTATE_TIMEOUT,
                )
                .await?;
            }
            McpServerTransport::Sse { url, headers } => {
                install_http_like(name, "sse", url, headers, &env).await?;
            }
            McpServerTransport::Http { url, headers } => {
                install_http_like(name, "http", url, headers, &env).await?;
            }
        }

        Ok(())
    }

    async fn remove_server(&self, name: &str) -> Result<(), McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        let env = self.isolated_env();

        // Try each scope; stop on first success or "not found".
        for scope in REMOVE_SCOPES {
            let (stdout, _stderr) =
                run_cli_with_env(CLI_NAME, &["mcp", "remove", "-s", scope, name], &env, MUTATE_TIMEOUT).await?;
            let lower = stdout.to_lowercase();
            if lower.contains("removed") || lower.contains("not found") {
                return Ok(());
            }
        }

        // If none of the scopes reported "removed" or "not found", treat as
        // idempotent success (server may simply not exist).
        Ok(())
    }
}

/// Install an HTTP-like (sse/http) server via `claude mcp add`.
async fn install_http_like(
    name: &str,
    transport_type: &str,
    url: &str,
    headers: &HashMap<String, String>,
    env: &[(&str, String)],
) -> Result<(), McpError> {
    let mut args = vec![
        "mcp".to_owned(),
        "add".to_owned(),
        "-s".to_owned(),
        "user".to_owned(),
        "--transport".to_owned(),
        transport_type.to_owned(),
        name.to_owned(),
        url.to_owned(),
    ];

    for (key, value) in headers {
        args.push("--header".to_owned());
        args.push(format!("{key}: {value}"));
    }

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_cli_with_env(CLI_NAME, &arg_refs, env, MUTATE_TIMEOUT).await?;
    Ok(())
}

/// Build the JSON config for `claude mcp add-json`.
fn build_stdio_json(command: &str, args: &[String], env: &HashMap<String, String>) -> serde_json::Value {
    let mut config = serde_json::json!({
        "command": command,
        "args": args,
    });
    if !env.is_empty() {
        config["env"] = serde_json::json!(env);
    }
    config
}

// ---------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------

/// Parse Claude CLI `mcp list` output.
///
/// Claude uses a custom format (not the standard Gemini/Qwen pattern):
/// ```text
/// name: command args - ✓ Connected
/// name: command args - ✗ Failed to connect
/// ```
fn parse_claude_list_output(output: &str) -> Vec<DetectedServer> {
    let cleaned = strip_ansi(output);
    let mut servers = Vec::new();

    for line in cleaned.lines() {
        let trimmed = line.trim();
        if let Some(server) = parse_claude_list_line(trimmed) {
            servers.push(server);
        }
    }

    servers
}

/// Parse a single line of Claude list output.
///
/// Pattern: `<name>: <command_or_url> - [✓|✗] <status>`
fn parse_claude_list_line(line: &str) -> Option<DetectedServer> {
    // Split on " - " to separate "name: command" from status
    let dash_pos = line.rfind(" - ")?;
    let status = normalize_detection_status(&line[dash_pos + 3..]);

    let name_cmd_part = &line[..dash_pos];

    // Claude separates the name from command/URL with ": ". Names
    // themselves may contain ":" (for example plugin-scoped MCP entries).
    let separator_pos = name_cmd_part.find(": ")?;
    let name = name_cmd_part[..separator_pos].trim();
    if name.is_empty() {
        return None;
    }

    let command_or_url = name_cmd_part[separator_pos + 2..].trim();
    if command_or_url.is_empty() {
        return None;
    }

    let normalized_command_or_url = command_or_url
        .strip_suffix(" (HTTP)")
        .or_else(|| command_or_url.strip_suffix(" (SSE)"))
        .unwrap_or(command_or_url)
        .trim();

    // Heuristic: if it looks like a URL, treat as HTTP; otherwise stdio.
    let transport =
        if normalized_command_or_url.starts_with("http://") || normalized_command_or_url.starts_with("https://") {
            // SSE heuristic: URL ending with /sse
            if normalized_command_or_url.ends_with("/sse") {
                McpServerTransport::Sse {
                    url: normalized_command_or_url.to_owned(),
                    headers: HashMap::new(),
                }
            } else {
                McpServerTransport::Http {
                    url: normalized_command_or_url.to_owned(),
                    headers: HashMap::new(),
                }
            }
        } else {
            McpServerTransport::Stdio {
                command: normalized_command_or_url.to_owned(),
                args: Vec::new(),
                env: HashMap::new(),
            }
        };

    Some(DetectedServer {
        name: name.to_owned(),
        transport,
        importable: status.eq_ignore_ascii_case("Connected") && !name.starts_with("plugin:"),
        import_skip_reason: if name.starts_with("plugin:") {
            Some("Plugin-managed MCP".to_owned())
        } else if status.eq_ignore_ascii_case("Connected") {
            None
        } else {
            Some(status)
        },
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- config isolation (regression guard) ----------------------------------

    #[test]
    fn adapter_targets_the_bridge_isolated_home_not_an_ad_hoc_path() {
        // The agent factory spawns Claude against this exact directory. If
        // the two ever diverge, MCP servers registered here become invisible
        // to the agent that is supposed to use them.
        let data_dir = Path::new("/app-data");
        let adapter = ClaudeAdapter::new(data_dir);

        assert_eq!(adapter.config_dir, aionui_common::claude_bridge_home(data_dir));
    }

    #[test]
    fn mutations_are_pinned_to_the_isolated_home() {
        let adapter = ClaudeAdapter::new(Path::new("/app-data"));
        let env = adapter.isolated_env();

        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, aionui_common::CLAUDE_CONFIG_DIR_ENV_KEY);
        assert!(
            env[0].1.contains(aionui_common::CLAUDE_BRIDGE_HOME_DIR_NAME),
            "expected isolated home in env value, got {}",
            env[0].1
        );
    }

    #[test]
    fn isolated_env_value_is_never_empty() {
        // An empty CLAUDE_CONFIG_DIR makes Claude fall back to the real home,
        // so isolation would fail *open* — silently reintroducing the bug.
        let adapter = ClaudeAdapter::new(Path::new(""));
        let env = adapter.isolated_env();
        assert!(!env[0].1.is_empty());
    }

    #[test]
    fn detect_existing_does_not_pin_the_config_dir() {
        // Guards the read/write asymmetry: `detect_existing` is the import
        // source and must see the user's own Claude Code config. Pinning it
        // to the isolated home would leave the import feature scanning this
        // app's own registry, with nothing to import.
        //
        // Asserted at the source level because the behaviour lives in which
        // env constant the call site passes, and INHERIT_ENV is the only
        // value that leaves CLAUDE_CONFIG_DIR untouched.
        assert!(
            INHERIT_ENV.is_empty(),
            "INHERIT_ENV must not set any variable, or detect_existing would stop reading the real config"
        );
    }

    // -- list output parsing --------------------------------------------------

    #[test]
    fn parse_claude_stdio_connected() {
        let output = "my-server: npx -y @test/server - ✓ Connected";
        let servers = parse_claude_list_output(output);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "my-server");
        match &servers[0].transport {
            McpServerTransport::Stdio { command, .. } => {
                assert_eq!(command, "npx -y @test/server");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn parse_claude_stdio_failed() {
        let output = "broken-srv: node index.js - ✗ Failed to connect";
        let servers = parse_claude_list_output(output);
        assert_eq!(servers.len(), 1);
        assert!(!servers[0].importable);
        assert_eq!(servers[0].import_skip_reason.as_deref(), Some("Failed to connect"));
    }

    #[test]
    fn parse_claude_http_server() {
        let output = "remote: https://example.com/mcp - ✓ Connected";
        let servers = parse_claude_list_output(output);
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Http { url, .. } => {
                assert_eq!(url, "https://example.com/mcp");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn parse_claude_sse_heuristic() {
        let output = "sse-srv: https://example.com/sse - ✓ Connected";
        let servers = parse_claude_list_output(output);
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Sse { url, .. } => {
                assert_eq!(url, "https://example.com/sse");
            }
            _ => panic!("expected Sse"),
        }
    }

    #[test]
    fn parse_claude_plugin_http_server_needing_auth() {
        let output = "plugin:slack:slack: https://mcp.slack.com/mcp (HTTP) - ! Needs authentication";
        let servers = parse_claude_list_output(output);
        assert_eq!(servers.len(), 1);
        assert!(!servers[0].importable);
        assert_eq!(servers[0].import_skip_reason.as_deref(), Some("Plugin-managed MCP"));
    }

    #[test]
    fn parse_claude_multiple_servers() {
        let output = "\
my-mcp: npx -y @test/mcp - ✓ Connected
broken: node bad.js - ✗ Failed to connect
web: https://example.com/api - ✓ Connected";
        let servers = parse_claude_list_output(output);
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "my-mcp");
        assert_eq!(servers[1].name, "broken");
        assert!(!servers[1].importable);
        assert_eq!(servers[2].name, "web");
    }

    #[test]
    fn parse_claude_with_ansi() {
        let output = "\x1b[32m✓\x1b[0m test: npx srv - \x1b[32mConnected\x1b[0m";
        let servers = parse_claude_list_output(output);
        // After ANSI strip: "✓ test: npx srv - Connected"
        // The ✓ is at the beginning of the line, not in the "name: cmd" pattern
        // but it contains "Connected" so it should be parseable
        assert_eq!(servers.len(), 1);
    }

    #[test]
    fn parse_claude_no_servers() {
        let output = "No MCP servers configured.\nTry `claude mcp add` to get started.";
        let servers = parse_claude_list_output(output);
        assert!(servers.is_empty());
    }

    #[test]
    fn parse_claude_empty_output() {
        let servers = parse_claude_list_output("");
        assert!(servers.is_empty());
    }

    #[test]
    fn build_stdio_json_without_env() {
        let json = build_stdio_json("npx", &["-y".into(), "srv".into()], &HashMap::new());
        assert_eq!(json["command"], "npx");
        assert_eq!(json["args"], serde_json::json!(["-y", "srv"]));
        assert!(json.get("env").is_none());
    }

    #[test]
    fn build_stdio_json_with_env() {
        let mut env = HashMap::new();
        env.insert("KEY".into(), "VALUE".into());
        let json = build_stdio_json("node", &[], &env);
        assert_eq!(json["command"], "node");
        assert_eq!(json["env"]["KEY"], "VALUE");
    }
}
