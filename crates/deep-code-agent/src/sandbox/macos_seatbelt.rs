use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::policy::SandboxPolicy;

pub const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

const BASE_POLICY: &str = r#"
(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow user-preference-read)
(allow sysctl-read)
(allow ipc-posix-sem)
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-read* (literal "/dev/urandom"))
(allow file-write-data
  (require-all
    (path "/dev/null")
    (vnode-type CHARACTER-DEVICE)))
(allow file-read*)
"#;

const NETWORK_POLICY: &str = r#"
(allow network-outbound)
(allow network-inbound)
(allow system-socket)
"#;

const SENSITIVE_WRITE_DENY: &str = r#"
(deny file-write*
  (subpath (param "HOME_SSH")))
(deny file-write*
  (subpath (param "HOME_AWS")))
(deny file-write*
  (subpath (param "HOME_GNUPG")))
(deny file-write*
  (subpath (param "HOME_NETRC")))
"#;

pub fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if !Path::new(SANDBOX_EXEC_PATH).exists() {
            return false;
        }
        Command::new(SANDBOX_EXEC_PATH)
            .args(["-p", "(version 1)(allow default)", "--", "/usr/bin/true"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

pub fn wrap_shell_command(
    command: &str,
    cwd: &Path,
    workspace: &Path,
    policy: &SandboxPolicy,
) -> Command {
    let inner = bare_shell_command(command, cwd);
    let program = inner.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = inner
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let mut wrapped = Command::new(SANDBOX_EXEC_PATH);
    wrapped.args(seatbelt_args(vec![program], args, workspace, cwd, policy));
    wrapped.current_dir(cwd);
    wrapped
}

fn bare_shell_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(cwd);
    cmd
}

fn seatbelt_args(
    mut program: Vec<String>,
    args: Vec<String>,
    workspace: &Path,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Vec<String> {
    program.extend(args);
    let mut seatbelt_args = vec![
        "-p".to_string(),
        build_policy_string(policy, workspace, cwd),
    ];
    for (key, value) in build_params(workspace, cwd) {
        seatbelt_args.push(format!("-D{key}={}", value.display()));
    }
    seatbelt_args.push("--".to_string());
    seatbelt_args.extend(program);
    seatbelt_args
}

fn build_policy_string(policy: &SandboxPolicy, workspace: &Path, cwd: &Path) -> String {
    let mut policy_text = BASE_POLICY.to_string();
    policy_text.push_str(SENSITIVE_WRITE_DENY);

    if policy.has_network_access() {
        policy_text.push_str(NETWORK_POLICY);
    }

    if !matches!(policy, SandboxPolicy::ReadOnly) {
        for index in 0..policy.writable_roots(workspace, cwd).len() {
            policy_text.push_str(&format!(
                "\n(allow file-write* (subpath (param \"WRITABLE_ROOT_{index}\")))"
            ));
        }
    }

    policy_text
}

fn build_params(workspace: &Path, cwd: &Path) -> Vec<(String, PathBuf)> {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut params = vec![("WORKSPACE".to_string(), workspace.clone())];
    params.push(("WRITABLE_ROOT_0".to_string(), workspace));
    if cwd != params[0].1 {
        params.push(("WRITABLE_ROOT_1".to_string(), cwd));
    }

    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        params.push(("HOME_SSH".to_string(), home.join(".ssh")));
        params.push(("HOME_AWS".to_string(), home.join(".aws")));
        params.push(("HOME_GNUPG".to_string(), home.join(".gnupg")));
        params.push(("HOME_NETRC".to_string(), home.join(".netrc")));
    }

    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn seatbelt_availability_is_queryable() {
        let _ = is_available();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_write_outside_workspace() {
        if !is_available() {
            eprintln!("skipping seatbelt smoke test: sandbox-exec unavailable");
            return;
        }

        let workspace = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("blocked.txt");
        let command = format!("echo leaked > {}", target.display());

        let status = wrap_shell_command(
            &command,
            workspace.path(),
            workspace.path(),
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandboxed command should spawn");

        assert!(!status.success() || !target.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_allows_write_inside_workspace() {
        if !is_available() {
            eprintln!("skipping seatbelt smoke test: sandbox-exec unavailable");
            return;
        }

        let workspace = tempdir().unwrap();
        let target = workspace.path().join("allowed.txt");
        let command = format!(
            "echo ok > {}",
            target.file_name().unwrap().to_string_lossy()
        );

        let status = wrap_shell_command(
            &command,
            workspace.path(),
            workspace.path(),
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandboxed command should spawn");

        assert!(status.success());
        assert!(target.exists());
        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("ok"));
    }
}
