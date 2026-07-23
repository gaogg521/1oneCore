use std::path::{Path, PathBuf};

/// Resolve a command name to an absolute path.
///
/// Commands are resolved from the startup-enhanced `$PATH` via
/// `which::which`.
///
/// On Windows, if a bare name lookup fails we retry with the common
/// shim suffixes (`.cmd`, `.ps1`, `.bat`). Tools installed via npm
/// global / pnpm / yarn typically ship as `name.cmd`, and a user with a
/// trimmed `PATHEXT` would otherwise see them as missing.
pub fn resolve_command_path(cmd: &str) -> Option<PathBuf> {
    match cmd {
        // Fork: Cursor Agent CLI often lives under LOCALAPPDATA, not on PATH.
        "agent" => resolve_known_cursor_agent_install_path()
            .or_else(|| which::which("agent").ok())
            .or_else(|| windows_shim_fallback("agent")),
        // Upstream removed Bun runtime; do not revive bun/bunx special cases.
        other => which::which(other).ok().or_else(|| windows_shim_fallback(other)),
    }
}

/// Cursor Agent CLI ships under `%LOCALAPPDATA%\cursor-agent\` on Windows and
/// is often not on PATH even when installed.
fn resolve_known_cursor_agent_install_path() -> Option<PathBuf> {
    resolve_known_cursor_agent_install_path_from_env(std::env::var_os("LOCALAPPDATA").as_deref())
}

fn resolve_known_cursor_agent_install_path_from_env(local_app_data: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let local_app_data = local_app_data?;
    let base = PathBuf::from(local_app_data).join("cursor-agent");
    for name in [
        "agent.exe",
        "agent.cmd",
        "agent",
        "cursor-agent.exe",
        "cursor-agent.cmd",
    ] {
        let candidate = base.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn windows_shim_fallback(cmd: &str) -> Option<PathBuf> {
    // If the caller already passed an extension, no point retrying.
    if Path::new(cmd).extension().is_some() {
        return None;
    }
    for ext in ["cmd", "ps1", "bat"] {
        if let Ok(p) = which::which(format!("{cmd}.{ext}")) {
            return Some(p);
        }
    }
    None
}

#[cfg(not(windows))]
fn windows_shim_fallback(_cmd: &str) -> Option<PathBuf> {
    None
}

/// Resolve `cmd` to an absolute path **within `dir` only** — does not walk
/// `PATH`. Honours `PATHEXT` (so `widget.exe` is found on Windows), and on
/// Windows additionally tries `.cmd`, `.ps1`, `.bat` shim suffixes for
/// npm-/pnpm-installed CLIs whose extension `PATHEXT` may not list.
///
/// `dir` is wrapped via `std::env::join_paths` before being handed to
/// `which::which_in`, so a `dir` that itself contains the OS PATH
/// separator (`:` on Unix, `;` on Windows) cannot be misinterpreted as
/// two directories. If `dir` cannot be expressed as a single PATH
/// entry, we return `None` rather than searching a phantom location.
///
/// Returns `None` if the command cannot be resolved inside the directory.
pub fn resolve_command_in(cmd: &str, dir: &Path) -> Option<PathBuf> {
    let paths = std::env::join_paths([dir]).ok()?;
    if let Ok(p) = which::which_in(cmd, Some(&paths), dir) {
        return Some(p);
    }
    windows_shim_fallback_in(cmd, dir)
}

/// Try `cmd` plus the common Windows shim suffixes (`.cmd`, `.ps1`, `.bat`)
/// inside a single directory. Used by `resolve_command_in` for callers that
/// want a directory-scoped lookup (the global `windows_shim_fallback` below
/// goes through `which::which`, which walks the entire `PATH`).
#[cfg(windows)]
fn windows_shim_fallback_in(cmd: &str, dir: &Path) -> Option<PathBuf> {
    if Path::new(cmd).extension().is_some() {
        return None;
    }
    for ext in ["cmd", "ps1", "bat"] {
        let candidate = dir.join(format!("{cmd}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(windows))]
fn windows_shim_fallback_in(_cmd: &str, _dir: &Path) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolve_command_in_finds_executable_in_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join("widget");
        std::fs::write(&bin, b"#!/bin/sh\necho hi\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let found = resolve_command_in("widget", tmp.path()).expect("must find");
        assert_eq!(found, bin);
    }

    #[test]
    fn resolve_command_in_returns_none_for_missing_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let found = resolve_command_in("definitely-not-here", tmp.path());
        assert!(found.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_command_in_handles_dir_with_colon_safely() {
        // A path containing `:` is a separator-collision hazard for the
        // PATH string `which_in` consumes. We must NOT internally split
        // and search a wrong second segment — return None instead.
        let tmp = tempfile::TempDir::new().unwrap();
        let weird = tmp.path().join("with:colon");
        std::fs::create_dir(&weird).unwrap();
        // No `widget` file is created anywhere — the only way this could
        // return Some is if the function wrongly split `with:colon` and
        // found something in another segment.
        let found = resolve_command_in("widget", &weird);
        assert!(found.is_none(), "must not split on `:` inside dir; got {:?}", found);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_command_in_falls_back_to_cmd_shim_on_windows() {
        // Simulate an npm-installed CLI: only `widget.cmd` exists, not `widget.exe`.
        let tmp = tempfile::TempDir::new().unwrap();
        let shim = tmp.path().join("widget.cmd");
        std::fs::write(&shim, b"@echo off\r\necho hi\r\n").unwrap();

        let found = resolve_command_in("widget", tmp.path()).expect("must find shim");
        assert!(
            found.to_string_lossy().to_lowercase().ends_with("widget.cmd"),
            "expected the .cmd shim; got {}",
            found.display()
        );
    }

    #[test]
    fn resolve_cursor_agent_install_path_from_local_app_data() {
        let tmp = tempfile::TempDir::new().unwrap();
        let agent_dir = tmp.path().join("cursor-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_cmd = agent_dir.join("agent.cmd");
        std::fs::write(&agent_cmd, b"@echo off\r\n").unwrap();

        let found = resolve_known_cursor_agent_install_path_from_env(Some(tmp.path().as_os_str()))
            .expect("must find cursor agent shim");
        assert_eq!(found, agent_cmd);
    }
}
