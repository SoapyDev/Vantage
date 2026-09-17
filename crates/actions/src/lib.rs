//! CLI action executor: spawns shell commands with timeout and captures
//! their output. Template injection and dictionary handling are the caller's
//! concern (runner crate) - this crate only executes.

use std::collections::HashMap;
use std::time::Duration;

/// A fully-resolved command, ready to execute (templates already injected).
#[derive(Debug, Clone)]
pub struct ActionCommand {
    pub name: String,
    pub command: String,
    /// Shell selector: "sh", "bash", "powershell", "cmd", or any program
    /// invoked as `<program> -c <command>`. `None` = OS default
    /// (powershell on Windows, sh elsewhere).
    pub shell: Option<String>,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub cwd: Option<String>,
}

impl ActionCommand {
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            shell: None,
            env: HashMap::new(),
            timeout: DEFAULT_TIMEOUT,
            cwd: None,
        }
    }
}

/// Default per-action timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct ActionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub timed_out: bool,
    /// Spawn-level error (shell not found, ...), distinct from non-zero exit.
    pub spawn_error: Option<String>,
}

impl ActionResult {
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self.timed_out && self.spawn_error.is_none() && self.exit_code == Some(0)
    }
}

/// Returns the shell program and its "run a command string" flag.
/// `None` selector = OS default: powershell on Windows, sh elsewhere.
fn shell_invocation(shell: Option<&str>) -> (String, String) {
    let selector = shell.map(str::trim).filter(|s| !s.is_empty());

    match selector {
        None => {
            if cfg!(windows) {
                ("powershell".to_string(), "-Command".to_string())
            } else {
                ("sh".to_string(), "-c".to_string())
            }
        }
        Some("powershell") | Some("pwsh") => {
            (selector.unwrap().to_string(), "-Command".to_string())
        }
        Some("cmd") => ("cmd".to_string(), "/C".to_string()),
        Some(other) => (other.to_string(), "-c".to_string()),
    }
}

/// Builds the shell process for a resolved command, with captured output.
fn configured_process(command: &ActionCommand) -> tokio::process::Command {
    let (program, flag) = shell_invocation(command.shell.as_deref());

    let mut process = tokio::process::Command::new(program);
    process
        .arg(flag)
        .arg(&command.command)
        .envs(&command.env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    if let Some(cwd) = &command.cwd {
        process.current_dir(cwd);
    }
    process
}

/// Executes a resolved command through the configured shell.
/// Never returns an `Err`: every failure mode is captured in the result.
pub async fn execute(command: &ActionCommand) -> ActionResult {
    let mut process = configured_process(command);
    let start = std::time::Instant::now();

    let child = match process.spawn() {
        Ok(child) => child,
        Err(e) => {
            let (program, _) = shell_invocation(command.shell.as_deref());
            return ActionResult {
                spawn_error: Some(format!("failed to spawn '{program}': {e}")),
                duration_ms: start.elapsed().as_millis(),
                ..Default::default()
            };
        }
    };

    match tokio::time::timeout(command.timeout, child.wait_with_output()).await {
        Err(_elapsed) => ActionResult {
            timed_out: true,
            duration_ms: start.elapsed().as_millis(),
            ..Default::default()
        },
        Ok(Err(e)) => ActionResult {
            spawn_error: Some(format!("failed to collect output: {e}")),
            duration_ms: start.elapsed().as_millis(),
            ..Default::default()
        },
        Ok(Ok(output)) => ActionResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
            duration_ms: start.elapsed().as_millis(),
            timed_out: false,
            spawn_error: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(command: &str) -> ActionCommand {
        ActionCommand::new("test action", command)
    }

    #[tokio::test]
    async fn runs_command_and_captures_stdout() {
        let result = execute(&cmd("echo hello")).await;
        assert!(result.is_success(), "{result:?}");
        assert_eq!(result.stdout.trim(), "hello");
        assert_eq!(result.exit_code, Some(0));
    }

    #[cfg(unix)]
    const STDERR_AND_EXIT_3: &str = "echo oops 1>&2; exit 3";
    #[cfg(windows)]
    const STDERR_AND_EXIT_3: &str = "[Console]::Error.WriteLine('oops'); exit 3";

    #[tokio::test]
    async fn captures_stderr_and_nonzero_exit() {
        let result = execute(&cmd(STDERR_AND_EXIT_3)).await;
        assert!(!result.is_success());
        assert_eq!(result.exit_code, Some(3), "{result:?}");
        assert_eq!(result.stderr.trim(), "oops");
    }

    #[cfg(unix)]
    const ECHO_ENV: &str = "echo $VANTAGE_VALUE";
    #[cfg(windows)]
    const ECHO_ENV: &str = "echo $env:VANTAGE_VALUE";

    #[tokio::test]
    async fn injects_env_variables() {
        let mut command = cmd(ECHO_ENV);
        command
            .env
            .insert("VANTAGE_VALUE".to_string(), "42".to_string());
        let result = execute(&command).await;
        assert!(result.is_success(), "{result:?}");
        assert_eq!(result.stdout.trim(), "42");
    }

    #[tokio::test]
    async fn honors_timeout() {
        let mut command = cmd("sleep 5");
        command.timeout = Duration::from_millis(200);
        let start = std::time::Instant::now();
        let result = execute(&command).await;
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(result.timed_out);
        assert!(!result.is_success());
    }

    #[tokio::test]
    async fn honors_cwd() {
        #[cfg(unix)]
        let (target, print_cwd) = ("/tmp".to_string(), "pwd");
        #[cfg(windows)]
        let (target, print_cwd) = (
            std::env::temp_dir().to_string_lossy().to_string(),
            "(Get-Location).Path",
        );

        let mut command = cmd(print_cwd);
        command.cwd = Some(target.clone());
        let result = execute(&command).await;
        assert!(result.is_success(), "{result:?}");

        // Canonicalize both sides: resolves Windows 8.3 short paths
        // (ALEXAN~1) and symlinked temp dirs to a comparable form.
        let canon = |s: &str| {
            std::fs::canonicalize(s.trim())
                .map(|p| p.to_string_lossy().to_lowercase())
                .unwrap_or_else(|_| s.trim().to_lowercase())
        };
        assert_eq!(canon(&result.stdout), canon(&target));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_shell_override() {
        let mut command = cmd("echo $0");
        command.shell = Some("bash".to_string());
        let result = execute(&command).await;
        assert!(result.is_success(), "{result:?}");
        assert!(result.stdout.contains("bash"), "{result:?}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn explicit_shell_override() {
        // %COMSPEC% only expands under cmd; powershell would keep it literal.
        let mut command = cmd("echo %COMSPEC%");
        command.shell = Some("cmd".to_string());
        let result = execute(&command).await;
        assert!(result.is_success(), "{result:?}");
        assert!(
            result.stdout.to_lowercase().contains("cmd.exe"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn unknown_shell_reports_spawn_error() {
        let mut command = cmd("echo hi");
        command.shell = Some("definitely-not-a-shell-xyz".to_string());
        let result = execute(&command).await;
        assert!(!result.is_success());
        assert!(result.spawn_error.is_some(), "{result:?}");
    }
}
