use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const MANAGED_RESOURCES_CONTRACT_FILE: &str = "manifest.json";
pub const MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION: u8 = 2;
const SUPPORTED_RUNTIME_KEYS: [&str; 6] = [
    "win32-x64",
    "win32-arm64",
    "darwin-x64",
    "darwin-arm64",
    "linux-x64",
    "linux-arm64",
];
const REQUIRED_CLI_NAMES: [&str; 2] = ["claude", "codex"];
/// The ACP wrapper layers this fork spawns for Claude / Codex sessions. They are
/// npm packages materialized under `acp/` at packaging time, and are NOT
/// interchangeable with the native `cli/claude` + `cli/codex` binaries above:
/// `factory/acp.rs` resolves sessions through `acp_tool_runtime`, which requires
/// this subtree. A bundle missing it fails hard at runtime on any machine whose
/// user-data cache has not already materialized these versions.
const REQUIRED_ACP_TOOL_SLUGS: [&str; 2] = ["claude-agent-acp", "codex-acp"];

/// The runtime key (`<os>-<arch>`) identifying the current platform's managed
/// resources subtree. Lives beside `SUPPORTED_RUNTIME_KEYS` — the values must
/// stay in sync, and the bundled-CLI module that used to own this is gone.
pub fn current_runtime_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("windows", "x86_64") => Some("win32-x64"),
        ("windows", "aarch64") => Some("win32-arm64"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedResourcesContract {
    pub schema_version: u8,
    pub runtime_key: String,
    pub node: ManagedNodeResourceContract,
    pub clis: Vec<ManagedCliResourceContract>,
    /// Defaulted so a pre-`acpTools` manifest still deserializes — it is then
    /// rejected by `validate_contract` with a field-specific message rather than
    /// an opaque serde error.
    #[serde(default)]
    pub acp_tools: Vec<ManagedAcpToolResourceContract>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedNodeResourceContract {
    pub version: String,
    pub root: String,
    pub executable: String,
}

/// A bundled agent CLI (claude / codex). Unlike the removed ACP-tool contract
/// there is no node bridge or local manifest — the CLI is a native binary (plus,
/// for codex, sidecars under its `vendor/<triple>` subtree captured via
/// `required_files` / `required_directories`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedCliResourceContract {
    pub name: String,
    pub version: String,
    /// Relative to the managed-resources root, e.g. `cli/claude/2.1.215/darwin-arm64`.
    pub root: String,
    /// Must equal the contract `runtime_key`.
    pub platform_directory: String,
    /// The main executable, relative to `root` (e.g. `claude` or
    /// `vendor/aarch64-apple-darwin/bin/codex`).
    pub executable: String,
    /// Extra files that must exist relative to `root` (e.g. codex sidecars
    /// `codex-path/rg`, `codex-resources/zsh/bin/zsh`). May be empty (claude).
    #[serde(default)]
    pub required_files: Vec<String>,
    /// Extra directories that must exist relative to `root`. May be empty.
    #[serde(default)]
    pub required_directories: Vec<String>,
}

/// A bundled ACP wrapper package (`claude-agent-acp` / `codex-acp`). Unlike a
/// managed CLI this is Node code: the runtime spawns `node <entrypoint>` and
/// reads a per-tool `manifest.json` sitting at the root of the subtree.
///
/// Shape is deliberately identical to the schema-v1 `acpTools` entry: both the
/// packaging verifier and the installed-app verifier already validate that shape
/// field-for-field, so keeping it lets them cover v2 bundles unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedAcpToolResourceContract {
    /// `claude-agent-acp` or `codex-acp`.
    pub slug: String,
    pub version: String,
    pub package_name: String,
    /// Relative to the managed-resources root, e.g.
    /// `acp/claude-agent-acp/0.58.1/win32-x64`. The runtime rebuilds this exact
    /// path from its own version constants, so a drift here is a hard failure.
    pub root: String,
    /// Must equal the contract `runtime_key`.
    pub platform_directory: String,
    /// Per-tool manifest filename, relative to `root`.
    pub manifest: String,
    /// Node entrypoint, relative to `root`.
    pub entrypoint: String,
    pub path_entries: Vec<String>,
    pub required_files: Vec<String>,
    pub required_directories: Vec<String>,
    pub platform_executable: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ManagedResourcesContractError {
    message: String,
}

impl ManagedResourcesContractError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(action: &str, path: &Path, error: std::io::Error) -> Self {
        Self::invalid(format!("{action} {}: {error}", path.display()))
    }
}

pub fn validate_contract(
    root: &Path,
    contract: &ManagedResourcesContract,
) -> Result<(), ManagedResourcesContractError> {
    validate_schema(contract)?;
    validate_node_schema(&contract.node)?;
    validate_clis_schema(contract)?;
    validate_acp_tools_schema(contract)?;
    validate_node_paths(root, &contract.node)?;
    for cli in &contract.clis {
        validate_cli_paths(root, cli)?;
    }
    for tool in &contract.acp_tools {
        validate_acp_tool_paths(root, tool)?;
    }
    Ok(())
}

pub fn write_contract(
    root: &Path,
    contract: &ManagedResourcesContract,
) -> Result<PathBuf, ManagedResourcesContractError> {
    validate_contract(root, contract)?;
    let path = root.join(MANAGED_RESOURCES_CONTRACT_FILE);
    let mut contents = serde_json::to_string_pretty(contract).map_err(|error| {
        ManagedResourcesContractError::invalid(format!("serialize managed resources contract: {error}"))
    })?;
    contents.push('\n');
    fs::write(&path, contents).map_err(|error| ManagedResourcesContractError::io("write contract", &path, error))?;
    Ok(path)
}

pub fn relative_contract_path(base: &Path, path: &Path) -> Result<String, ManagedResourcesContractError> {
    let relative = path.strip_prefix(base).map_err(|_| {
        ManagedResourcesContractError::invalid(format!(
            "path {} is not under managed resources root {}",
            path.display(),
            base.display()
        ))
    })?;
    let value = relative.to_string_lossy().replace('\\', "/");
    validate_contract_relative_path(&value)?;
    Ok(value)
}

fn validate_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    if contract.schema_version != MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION {
        return Err(ManagedResourcesContractError::invalid(format!(
            "unsupported schemaVersion {}",
            contract.schema_version
        )));
    }
    require_non_empty("runtimeKey", &contract.runtime_key)?;
    if !SUPPORTED_RUNTIME_KEYS.contains(&contract.runtime_key.as_str()) {
        return Err(ManagedResourcesContractError::invalid(format!(
            "unsupported runtimeKey {}",
            contract.runtime_key
        )));
    }
    Ok(())
}

fn validate_node_schema(node: &ManagedNodeResourceContract) -> Result<(), ManagedResourcesContractError> {
    require_non_empty("node.version", &node.version)?;
    validate_contract_relative_path_field("node.root", &node.root)?;
    validate_contract_relative_path_field("node.executable", &node.executable)?;
    Ok(())
}

fn validate_clis_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    let mut names = HashSet::new();

    for cli in &contract.clis {
        require_non_empty("clis[].name", &cli.name)?;
        if !names.insert(cli.name.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate clis name {}",
                cli.name
            )));
        }

        let label = format!("clis[{}]", cli.name);
        require_non_empty(format!("{label}.version"), &cli.version)?;
        validate_contract_relative_path_field(format!("{label}.root"), &cli.root)?;
        require_non_empty(format!("{label}.platformDirectory"), &cli.platform_directory)?;
        if cli.platform_directory != contract.runtime_key {
            return Err(ManagedResourcesContractError::invalid(format!(
                "clis[{}].platformDirectory {} does not match runtimeKey {}",
                cli.name, cli.platform_directory, contract.runtime_key
            )));
        }
        validate_contract_relative_path_field(format!("{label}.executable"), &cli.executable)?;
        for (index, entry) in cli.required_files.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredFiles[{index}]"), entry)?;
        }
        for (index, entry) in cli.required_directories.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredDirectories[{index}]"), entry)?;
        }
    }

    for required_name in REQUIRED_CLI_NAMES {
        if !names.contains(required_name) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "missing required clis name {required_name}"
            )));
        }
    }

    Ok(())
}

fn validate_acp_tools_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    let mut slugs = HashSet::new();

    for tool in &contract.acp_tools {
        require_non_empty("acpTools[].slug", &tool.slug)?;
        if !slugs.insert(tool.slug.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate acpTools slug {}",
                tool.slug
            )));
        }

        let label = format!("acpTools[{}]", tool.slug);
        require_non_empty(format!("{label}.version"), &tool.version)?;
        validate_contract_relative_path_field(format!("{label}.root"), &tool.root)?;
        require_non_empty(format!("{label}.platformDirectory"), &tool.platform_directory)?;
        if tool.platform_directory != contract.runtime_key {
            return Err(ManagedResourcesContractError::invalid(format!(
                "acpTools[{}].platformDirectory {} does not match runtimeKey {}",
                tool.slug, tool.platform_directory, contract.runtime_key
            )));
        }
        require_non_empty(format!("{label}.packageName"), &tool.package_name)?;
        validate_contract_relative_path_field(format!("{label}.manifest"), &tool.manifest)?;
        validate_contract_relative_path_field(format!("{label}.entrypoint"), &tool.entrypoint)?;
        validate_contract_relative_path_field(format!("{label}.platformExecutable"), &tool.platform_executable)?;
        for (index, entry) in tool.path_entries.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.pathEntries[{index}]"), entry)?;
        }
        for (index, entry) in tool.required_files.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredFiles[{index}]"), entry)?;
        }
        for (index, entry) in tool.required_directories.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredDirectories[{index}]"), entry)?;
        }
    }

    for required_slug in REQUIRED_ACP_TOOL_SLUGS {
        if !slugs.contains(required_slug) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "missing required acpTools slug {required_slug} — the bundle would ship without the \
                 ACP wrapper layer and every fresh install would fail to start Claude/Codex"
            )));
        }
    }

    Ok(())
}

fn validate_acp_tool_paths(
    root: &Path,
    tool: &ManagedAcpToolResourceContract,
) -> Result<(), ManagedResourcesContractError> {
    let tool_root = root.join(&tool.root);
    if !tool_root.is_dir() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required directory missing: {}",
            tool_root.display()
        )));
    }

    let entrypoint = tool_root.join(&tool.entrypoint);
    if !entrypoint.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            entrypoint.display()
        )));
    }

    // The runtime reads this back before it will use the subtree; a bundle
    // without it validates structurally but still fails on the user's machine.
    let local_manifest = tool_root.join(&tool.manifest);
    if !local_manifest.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            local_manifest.display()
        )));
    }

    for required_file in &tool.required_files {
        let path = tool_root.join(required_file);
        if !path.is_file() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required file missing: {}",
                path.display()
            )));
        }
    }
    for required_directory in &tool.required_directories {
        let path = tool_root.join(required_directory);
        if !path.is_dir() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required directory missing: {}",
                path.display()
            )));
        }
    }

    Ok(())
}

fn validate_node_paths(root: &Path, node: &ManagedNodeResourceContract) -> Result<(), ManagedResourcesContractError> {
    let node_root = root.join(&node.root);
    if !node_root.is_dir() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required directory missing: {}",
            node_root.display()
        )));
    }
    let executable = node_root.join(&node.executable);
    if !executable.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            executable.display()
        )));
    }
    Ok(())
}

fn validate_cli_paths(root: &Path, cli: &ManagedCliResourceContract) -> Result<(), ManagedResourcesContractError> {
    let cli_root = root.join(&cli.root);
    if !cli_root.is_dir() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required directory missing: {}",
            cli_root.display()
        )));
    }

    let executable = cli_root.join(&cli.executable);
    if !executable.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            executable.display()
        )));
    }

    for required_file in &cli.required_files {
        let path = cli_root.join(required_file);
        if !path.is_file() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required file missing: {}",
                path.display()
            )));
        }
    }
    for required_directory in &cli.required_directories {
        let path = cli_root.join(required_directory);
        if !path.is_dir() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required directory missing: {}",
                path.display()
            )));
        }
    }

    Ok(())
}

fn require_non_empty(field: impl std::fmt::Display, value: &str) -> Result<(), ManagedResourcesContractError> {
    if value.is_empty() {
        return Err(ManagedResourcesContractError::invalid(format!("{field} is required")));
    }
    Ok(())
}

fn validate_contract_relative_path_field(
    field: impl std::fmt::Display,
    value: &str,
) -> Result<(), ManagedResourcesContractError> {
    validate_contract_relative_path(value)
        .map_err(|error| ManagedResourcesContractError::invalid(format!("{field}: {error}")))
}

fn validate_contract_relative_path(value: &str) -> Result<(), ManagedResourcesContractError> {
    if value.is_empty()
        || value.contains('\\')
        || Path::new(value).is_absolute()
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ManagedResourcesContractError::invalid(format!(
            "invalid relative contract path {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_contract(runtime_key: &str) -> ManagedResourcesContract {
        ManagedResourcesContract {
            schema_version: MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION,
            runtime_key: runtime_key.into(),
            node: ManagedNodeResourceContract {
                version: "24.11.0".into(),
                root: "node/node-v24.11.0-win-x64".into(),
                executable: "node.exe".into(),
            },
            clis: vec![
                ManagedCliResourceContract {
                    name: "claude".into(),
                    version: "2.1.215".into(),
                    root: "cli/claude/2.1.215/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    executable: "claude.exe".into(),
                    required_files: vec![],
                    required_directories: vec![],
                },
                ManagedCliResourceContract {
                    name: "codex".into(),
                    version: "0.144.6".into(),
                    root: "cli/codex/0.144.6/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    executable: "vendor/x86_64-pc-windows-msvc/bin/codex.exe".into(),
                    required_files: vec!["vendor/x86_64-pc-windows-msvc/codex-path/rg.exe".into()],
                    required_directories: vec!["vendor/x86_64-pc-windows-msvc".into()],
                },
            ],
            acp_tools: vec![
                ManagedAcpToolResourceContract {
                    slug: "claude-agent-acp".into(),
                    version: "0.58.1".into(),
                    package_name: "@agentclientprotocol/claude-agent-acp".into(),
                    root: "acp/claude-agent-acp/0.58.1/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    manifest: "manifest.json".into(),
                    entrypoint: "node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js".into(),
                    path_entries: vec!["node_modules/.bin".into()],
                    required_files: vec!["package.json".into(), "package-lock.json".into()],
                    required_directories: vec!["node_modules".into()],
                    platform_executable: "node_modules/.bin/claude-agent-acp.cmd".into(),
                },
                ManagedAcpToolResourceContract {
                    slug: "codex-acp".into(),
                    version: "1.1.2".into(),
                    package_name: "@agentclientprotocol/codex-acp".into(),
                    root: "acp/codex-acp/1.1.2/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    manifest: "manifest.json".into(),
                    entrypoint: "node_modules/@agentclientprotocol/codex-acp/dist/index.js".into(),
                    path_entries: vec!["node_modules/.bin".into()],
                    required_files: vec!["package.json".into(), "package-lock.json".into()],
                    required_directories: vec!["node_modules".into()],
                    platform_executable: "node_modules/.bin/codex-acp.cmd".into(),
                },
            ],
        }
    }

    /// Materialize just enough of the bundle for path validation to pass, so a
    /// test that removes one piece is exercising that piece and nothing else.
    fn materialize_valid_bundle(root: &Path, contract: &ManagedResourcesContract) {
        let node_root = root.join(&contract.node.root);
        std::fs::create_dir_all(&node_root).expect("node root");
        std::fs::write(node_root.join(&contract.node.executable), b"").expect("node exe");

        for cli in &contract.clis {
            let cli_root = root.join(&cli.root);
            let executable = cli_root.join(&cli.executable);
            std::fs::create_dir_all(executable.parent().expect("exe parent")).expect("cli root");
            std::fs::write(&executable, b"").expect("cli exe");
            for required_file in &cli.required_files {
                let path = cli_root.join(required_file);
                std::fs::create_dir_all(path.parent().expect("file parent")).expect("required file parent");
                std::fs::write(&path, b"").expect("required file");
            }
            for required_directory in &cli.required_directories {
                std::fs::create_dir_all(cli_root.join(required_directory)).expect("required dir");
            }
        }

        for tool in &contract.acp_tools {
            let tool_root = root.join(&tool.root);
            let entrypoint = tool_root.join(&tool.entrypoint);
            std::fs::create_dir_all(entrypoint.parent().expect("entrypoint parent")).expect("acp root");
            std::fs::write(&entrypoint, b"").expect("entrypoint");
            std::fs::write(tool_root.join(&tool.manifest), b"{}").expect("local manifest");
            for required_file in &tool.required_files {
                let path = tool_root.join(required_file);
                std::fs::create_dir_all(path.parent().expect("file parent")).expect("required file parent");
                std::fs::write(&path, b"").expect("required file");
            }
            for required_directory in &tool.required_directories {
                std::fs::create_dir_all(tool_root.join(required_directory)).expect("required dir");
            }
        }
    }

    #[test]
    fn contract_serializes_v2_camel_case_schema() {
        let contract = example_contract("win32-x64");
        let value = serde_json::to_value(&contract).expect("serialize");

        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(value["runtimeKey"], "win32-x64");
        assert!(value.get("schema_version").is_none());
        assert_eq!(value["clis"][0]["name"], "claude");
        assert_eq!(
            value["clis"][1]["executable"],
            "vendor/x86_64-pc-windows-msvc/bin/codex.exe"
        );
    }

    #[test]
    fn validate_contract_rejects_duplicate_cli_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis[1].name = "claude".into();

        let error = validate_contract(temp.path(), &contract).expect_err("duplicate name should fail");

        assert!(error.to_string().contains("duplicate clis name claude"));
    }

    #[test]
    fn validate_contract_rejects_missing_required_cli_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis.retain(|cli| cli.name != "codex");

        let error = validate_contract(temp.path(), &contract).expect_err("missing required name should fail");

        assert!(error.to_string().contains("missing required clis name codex"));
    }

    #[test]
    fn validate_contract_rejects_unsafe_relative_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        for bad in ["/abs/path", "cli\\claude", "", "../escape", "cli/../escape"] {
            let mut contract = example_contract("win32-x64");
            contract.clis[0].root = bad.into();

            let error = validate_contract(temp.path(), &contract).expect_err("unsafe path should fail");

            assert!(error.to_string().contains("invalid relative contract path"), "{error}");
        }
    }

    #[test]
    fn validate_contract_rejects_platform_mismatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis[0].platform_directory = "linux-x64".into();

        let error = validate_contract(temp.path(), &contract).expect_err("platform mismatch should fail");
        assert!(
            error
                .to_string()
                .contains("platformDirectory linux-x64 does not match runtimeKey win32-x64")
        );
    }

    #[test]
    fn validate_contract_rejects_missing_required_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = example_contract("win32-x64");
        std::fs::create_dir_all(temp.path().join("node").join("node-v24.11.0-win-x64")).expect("create node root");

        let error = validate_contract(temp.path(), &contract).expect_err("missing paths should fail");

        assert!(
            error.to_string().contains("required file missing")
                || error.to_string().contains("required directory missing")
        );
    }

    #[test]
    fn a_fully_materialized_bundle_validates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &contract);

        validate_contract(temp.path(), &contract).expect("a complete bundle should validate");
    }

    /// This is the regression that shipped in 2.1.51: the manifest looked
    /// well-formed and every native CLI was present, but the ACP wrapper subtree
    /// was gone, so a fresh install could not start Claude or Codex at all.
    #[test]
    fn validate_contract_rejects_a_bundle_with_no_acp_tools_at_all() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &contract);
        contract.acp_tools.clear();

        let error = validate_contract(temp.path(), &contract).expect_err("an acp-less bundle must not ship");

        assert!(error.to_string().contains("missing required acpTools slug"), "{error}");
    }

    #[test]
    fn validate_contract_rejects_dropping_either_acp_tool_individually() {
        for slug in REQUIRED_ACP_TOOL_SLUGS {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut contract = example_contract("win32-x64");
            materialize_valid_bundle(temp.path(), &contract);
            contract.acp_tools.retain(|tool| tool.slug != slug);

            let error = validate_contract(temp.path(), &contract).expect_err("a missing wrapper must not ship");

            assert!(
                error
                    .to_string()
                    .contains(&format!("missing required acpTools slug {slug}")),
                "{error}"
            );
        }
    }

    /// The manifest can claim the subtree while the subtree is absent — that is
    /// precisely the state a stale or partially-copied bundle is in, and it is
    /// what the runtime trips over, so paths are checked on disk not just parsed.
    #[test]
    fn validate_contract_rejects_acp_tool_declared_but_not_on_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &contract);
        std::fs::remove_dir_all(temp.path().join(&contract.acp_tools[0].root)).expect("remove acp subtree");

        let error = validate_contract(temp.path(), &contract).expect_err("declared-but-absent must fail");

        assert!(error.to_string().contains("required directory missing"), "{error}");
    }

    /// A subtree whose per-tool manifest is missing passes a naive "is the
    /// directory there" check but still fails inside `validate_tool_root`.
    #[test]
    fn validate_contract_rejects_acp_tool_without_its_local_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &contract);
        std::fs::remove_file(
            temp.path()
                .join(&contract.acp_tools[0].root)
                .join(&contract.acp_tools[0].manifest),
        )
        .expect("remove local manifest");

        let error = validate_contract(temp.path(), &contract).expect_err("missing local manifest must fail");

        assert!(error.to_string().contains("required file missing"), "{error}");
    }

    #[test]
    fn validate_contract_rejects_acp_tool_platform_mismatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &contract);
        contract.acp_tools[0].platform_directory = "darwin-arm64".into();

        let error = validate_contract(temp.path(), &contract).expect_err("platform mismatch should fail");

        assert!(
            error
                .to_string()
                .contains("platformDirectory darwin-arm64 does not match runtimeKey win32-x64"),
            "{error}"
        );
    }

    #[test]
    fn validate_contract_rejects_duplicate_acp_tool_slugs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &contract);
        contract.acp_tools[1].slug = "claude-agent-acp".into();

        let error = validate_contract(temp.path(), &contract).expect_err("duplicate slug should fail");

        assert!(
            error.to_string().contains("duplicate acpTools slug claude-agent-acp"),
            "{error}"
        );
    }

    #[test]
    fn validate_contract_rejects_unsafe_acp_relative_paths() {
        for bad in ["/abs/path", "acp\\claude", "", "../escape", "acp/../escape"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut contract = example_contract("win32-x64");
            materialize_valid_bundle(temp.path(), &contract);
            contract.acp_tools[0].root = bad.into();

            let error = validate_contract(temp.path(), &contract).expect_err("unsafe path should fail");

            assert!(error.to_string().contains("invalid relative contract path"), "{error}");
        }
    }

    /// A pre-`acpTools` manifest must not silently look valid: it deserializes
    /// (the field defaults) but has to be rejected on validation.
    #[test]
    fn a_manifest_without_the_acp_tools_field_deserializes_then_fails_validation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let reference = example_contract("win32-x64");
        materialize_valid_bundle(temp.path(), &reference);

        let mut value = serde_json::to_value(&reference).expect("serialize");
        value.as_object_mut().expect("object").remove("acpTools");
        let parsed: ManagedResourcesContract = serde_json::from_value(value).expect("legacy manifest still parses");

        assert!(parsed.acp_tools.is_empty());
        let error = validate_contract(temp.path(), &parsed).expect_err("legacy manifest must not validate");
        assert!(error.to_string().contains("missing required acpTools slug"), "{error}");
    }
}
