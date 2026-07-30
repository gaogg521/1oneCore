use std::path::Path;

use aionui_runtime::Builder as CmdBuilder;
use tracing::{info, warn};

use crate::constants::{
    LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
    LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
};
use crate::error::ExtensionError;
use crate::types::LifecycleHooks;

/// Which lifecycle hook to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    OnInstall,
    OnUninstall,
    OnActivate,
    OnDeactivate,
}

impl HookKind {
    /// Default timeout in seconds for this hook kind.
    pub fn timeout_secs(self) -> u64 {
        match self {
            Self::OnInstall => LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
            Self::OnUninstall => LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
            Self::OnActivate => LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS,
            Self::OnDeactivate => LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS,
        }
    }

    /// Human-readable label for logging and error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::OnInstall => "onInstall",
            Self::OnUninstall => "onUninstall",
            Self::OnActivate => "onActivate",
            Self::OnDeactivate => "onDeactivate",
        }
    }
}

/// Resolve the hook script path from the manifest for a given hook kind.
pub fn resolve_hook_path(hooks: &LifecycleHooks, kind: HookKind) -> Option<&str> {
    let value = match kind {
        HookKind::OnInstall => hooks.on_install.as_deref(),
        HookKind::OnUninstall => hooks.on_uninstall.as_deref(),
        HookKind::OnActivate => hooks.on_activate.as_deref(),
        HookKind::OnDeactivate => hooks.on_deactivate.as_deref(),
    };
    value.filter(|s| !s.is_empty())
}

/// Build the child-process command for a hook script.
///
/// Scripts are dispatched to an interpreter by file extension instead of being
/// spawned directly, because a direct spawn is unreliable on every platform:
///
/// - `.sh` / `.bash`: Windows' `CreateProcess` refuses a shell script outright
///   (`ERROR_BAD_EXE_FORMAT`, os error 193), so hooks written as shell scripts
///   never ran there at all. On Unix a direct spawn needs the executable bit,
///   which archive-based extension distribution routinely strips.
/// - `.ps1`: PowerShell will not run a script file passed as the program
///   either — it has to come in through `-File`.
///
/// Anything else (native binaries, `.cmd`/`.bat`, extensionless files carrying
/// a shebang) is spawned directly, as before.
///
/// Returns `Err(reason)` when the required interpreter is not installed; the
/// caller turns that into a `HookFailed` carrying the extension context.
fn build_hook_command(script: &Path) -> Result<CmdBuilder, String> {
    let extension = script
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("sh") | Some("bash") => {
            let shell = posix_shell().ok_or_else(|| {
                "this hook is a shell script but no POSIX shell (bash/sh) was found on PATH".to_owned()
            })?;
            let mut builder = CmdBuilder::clean_cli(shell);
            // The POSIX shells available on Windows (Git for Windows / MSYS2)
            // re-parse the raw command line with POSIX escaping rules, so the
            // backslashes in `C:\Users\…\hook.sh` are consumed as escapes and
            // the script name arrives as `C:Users…`. Measured on a real run:
            // `/bin/bash: C:Usersallenzhao…: No such file or directory`.
            // Forward slashes are accepted just as well and survive that pass.
            #[cfg(windows)]
            builder.arg(script.to_string_lossy().replace('\\', "/"));
            #[cfg(not(windows))]
            builder.arg(script);
            Ok(builder)
        }
        Some("ps1") => {
            let powershell = aionui_runtime::resolve_command_path("powershell")
                .or_else(|| aionui_runtime::resolve_command_path("pwsh"))
                .ok_or_else(|| "this hook is a PowerShell script but PowerShell was not found on PATH".to_owned())?;
            let mut builder = CmdBuilder::clean_cli(powershell);
            builder.args(["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
            builder.arg(script);
            Ok(builder)
        }
        _ => Ok(CmdBuilder::clean_cli(script)),
    }
}

/// Locate a POSIX shell to run `.sh` hooks with.
///
/// Unix always has `/bin/sh`. On Windows this depends on whatever the user has
/// installed; when nothing is found the caller reports that rather than
/// failing with an opaque OS error.
fn posix_shell() -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        return Some(std::path::PathBuf::from("/bin/sh"));
    }
    #[cfg(not(unix))]
    {
        // `sh` first: on Windows that name is only ever supplied by a real
        // POSIX toolchain (Git for Windows / MSYS2). `bash` is riskier —
        // it commonly resolves to
        // `%LOCALAPPDATA%\Microsoft\WindowsApps\bash.exe`, which is the WSL
        // launcher, not a Windows-side shell. WSL runs in its own filesystem
        // namespace (`/mnt/c/...`), so the Windows path we hand it comes back
        // as "No such file or directory" — measured on a real run before this
        // ordering was in place.
        aionui_runtime::resolve_command_path("sh")
            .or_else(|| aionui_runtime::resolve_command_path("bash").filter(|path| !is_wsl_launcher(path)))
    }
}

/// Whether a resolved executable is a Microsoft Store execution alias.
///
/// Everything under `WindowsApps` is an alias stub; for `bash` that stub is
/// the WSL launcher, which cannot accept Windows-side paths.
#[cfg(not(unix))]
fn is_wsl_launcher(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsApps")
    })
}

/// Execute a lifecycle hook script in a child process.
///
/// - `ext_dir`: absolute path to the extension root directory (used as cwd).
/// - `hook_path`: script path relative to `ext_dir`.
/// - `kind`: which hook is being executed (determines timeout and label).
/// - `extension_name`: used for logging and error context.
///
/// Returns `Ok(())` on success. Returns an error if the script is not found,
/// times out, or exits with a non-zero status.
pub async fn execute_hook(
    ext_dir: &Path,
    hook_path: &str,
    kind: HookKind,
    extension_name: &str,
) -> Result<(), ExtensionError> {
    execute_hook_with_timeout(ext_dir, hook_path, kind, extension_name, kind.timeout_secs()).await
}

/// Same as [`execute_hook`], but with an explicit timeout instead of the one
/// implied by `kind`.
///
/// Exists so the timeout path can be exercised for real: the shortest built-in
/// timeout is 30s, so tests previously reached around `execute_hook` and
/// timed a bare `tokio::process::Command` themselves — which asserted nothing
/// about this module and skipped the interpreter dispatch in
/// [`build_hook_command`] entirely.
pub async fn execute_hook_with_timeout(
    ext_dir: &Path,
    hook_path: &str,
    kind: HookKind,
    extension_name: &str,
    timeout_secs: u64,
) -> Result<(), ExtensionError> {
    let script = ext_dir.join(hook_path);

    if !script.exists() {
        warn!(
            extension = extension_name,
            hook = kind.label(),
            path = %script.display(),
            "lifecycle hook script not found, skipping"
        );
        return Err(ExtensionError::HookNotFound(script.display().to_string()));
    }

    let label = kind.label();

    info!(
        extension = extension_name,
        hook = label,
        path = %script.display(),
        timeout_secs,
        "executing lifecycle hook"
    );

    let mut builder = build_hook_command(&script).map_err(|reason| {
        warn!(
            extension = extension_name,
            hook = label,
            path = %script.display(),
            reason = %reason,
            "lifecycle hook interpreter unavailable"
        );
        ExtensionError::HookFailed {
            extension_name: extension_name.to_owned(),
            hook: label.to_owned(),
            reason,
        }
    })?;
    builder.current_dir(ext_dir);
    let child_future = builder.output();

    let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child_future).await;

    match result {
        Err(_elapsed) => {
            warn!(
                extension = extension_name,
                hook = label,
                timeout_secs,
                "lifecycle hook timed out"
            );
            Err(ExtensionError::HookTimeout {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                timeout_secs,
            })
        }
        Ok(Err(io_err)) => {
            warn!(
                extension = extension_name,
                hook = label,
                error = %io_err,
                "lifecycle hook I/O error"
            );
            Err(ExtensionError::HookFailed {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                reason: io_err.to_string(),
            })
        }
        Ok(Ok(output)) => {
            if output.status.success() {
                info!(
                    extension = extension_name,
                    hook = label,
                    "lifecycle hook completed successfully"
                );
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string());
                warn!(
                    extension = extension_name,
                    hook = label,
                    exit_code = %code,
                    stderr = %stderr,
                    "lifecycle hook exited with error"
                );
                Err(ExtensionError::HookFailed {
                    extension_name: extension_name.to_owned(),
                    hook: label.to_owned(),
                    reason: format!("exit code {code}: {}", stderr.trim()),
                })
            }
        }
    }
}

/// Determine whether the `onInstall` hook should run.
///
/// Returns `true` when:
/// - There is no persisted version (first-time install).
/// - The persisted version differs from the current manifest version.
pub fn needs_install_hook(current_version: &str, persisted_version: Option<&str>) -> bool {
    match persisted_version {
        None => true,
        Some(prev) => prev != current_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // needs_install_hook
    // -----------------------------------------------------------------------

    #[test]
    fn test_needs_install_first_time() {
        assert!(needs_install_hook("1.0.0", None));
    }

    #[test]
    fn test_needs_install_version_changed() {
        assert!(needs_install_hook("2.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_no_install_same_version() {
        assert!(!needs_install_hook("1.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_needs_install_downgrade() {
        assert!(needs_install_hook("0.9.0", Some("1.0.0")));
    }

    // -----------------------------------------------------------------------
    // HookKind
    // -----------------------------------------------------------------------

    #[test]
    fn test_hook_kind_timeout_values() {
        assert_eq!(HookKind::OnInstall.timeout_secs(), 120);
        assert_eq!(HookKind::OnUninstall.timeout_secs(), 60);
        assert_eq!(HookKind::OnActivate.timeout_secs(), 30);
        assert_eq!(HookKind::OnDeactivate.timeout_secs(), 30);
    }

    #[test]
    fn test_hook_kind_labels() {
        assert_eq!(HookKind::OnInstall.label(), "onInstall");
        assert_eq!(HookKind::OnUninstall.label(), "onUninstall");
        assert_eq!(HookKind::OnActivate.label(), "onActivate");
        assert_eq!(HookKind::OnDeactivate.label(), "onDeactivate");
    }

    // -----------------------------------------------------------------------
    // resolve_hook_path
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_hook_path_present() {
        let hooks = LifecycleHooks {
            on_install: Some("scripts/install.sh".into()),
            on_activate: Some("scripts/activate.sh".into()),
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnInstall),
            Some("scripts/install.sh")
        );
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnActivate),
            Some("scripts/activate.sh")
        );
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnDeactivate), None);
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnUninstall), None);
    }

    #[test]
    fn test_resolve_hook_path_empty_string() {
        let hooks = LifecycleHooks {
            on_install: Some(String::new()),
            on_activate: None,
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnInstall), None);
    }

    // -----------------------------------------------------------------------
    // posix_shell
    // -----------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn wsl_launcher_stub_is_not_treated_as_a_posix_shell() {
        // The Store execution alias for `bash` is the WSL launcher; handing it
        // a Windows path fails, so it must never be picked as the hook shell.
        assert!(is_wsl_launcher(Path::new(
            r"C:\Users\someone\AppData\Local\Microsoft\WindowsApps\bash.exe"
        )));
        assert!(!is_wsl_launcher(Path::new(r"C:\Program Files\Git\usr\bin\sh.exe")));
    }

    // -----------------------------------------------------------------------
    // execute_hook (async unit tests)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_hook_script_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_hook(dir.path(), "nonexistent.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ExtensionError::HookNotFound(_)));
    }

    #[tokio::test]
    async fn test_execute_hook_success() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("hook.sh");
        std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "hook.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_hook_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("fail.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho 'something broke' >&2\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "fail.sh", HookKind::OnInstall, "test-ext").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ExtensionError::HookFailed {
                extension_name,
                hook,
                reason,
            } => {
                assert_eq!(extension_name, "test-ext");
                assert_eq!(hook, "onInstall");
                assert!(reason.contains("something broke"));
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_execute_hook_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("slow.sh");
        // Script that sleeps longer than we allow
        std::fs::write(&script_path, "#!/bin/sh\nsleep 5\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Drive the real code path with an injected deadline instead of timing a
        // bare Command, which bypassed both the interpreter dispatch and every
        // assertion this module owns.
        let result = execute_hook_with_timeout(dir.path(), "slow.sh", HookKind::OnActivate, "test-ext", 1).await;

        match result.expect_err("should have timed out") {
            ExtensionError::HookTimeout {
                extension_name,
                hook,
                timeout_secs,
            } => {
                assert_eq!(extension_name, "test-ext");
                assert_eq!(hook, "onActivate");
                assert_eq!(timeout_secs, 1);
            }
            other => panic!("expected HookTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_execute_hook_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cwd_marker.txt");
        let script_path = dir.path().join("check_cwd.sh");
        // Write cwd to a file so we can verify it
        std::fs::write(&script_path, "#!/bin/sh\npwd > cwd_marker.txt\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "check_cwd.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok());
        // Written through a relative path, so landing here already proves the cwd.
        assert!(marker.exists(), "relative write should land in the extension dir");
        let cwd_content = std::fs::read_to_string(&marker).unwrap();
        assert!(!cwd_content.trim().is_empty(), "hook should have reported a cwd");

        // Textual comparison only holds where `pwd` speaks the platform's own
        // path syntax; Git Bash on Windows reports POSIX paths (`/d/...`).
        #[cfg(unix)]
        {
            // (may have symlink resolution differences, compare canonical)
            let expected = dir.path().canonicalize().unwrap();
            let actual = Path::new(cwd_content.trim()).canonicalize().unwrap();
            assert_eq!(actual, expected);
        }
    }
}
