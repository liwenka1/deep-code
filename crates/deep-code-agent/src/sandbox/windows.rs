//! Windows sandbox: Job Object process containment.
//!
//! Unlike macOS/Linux, Windows has no clean "confine writes to a directory"
//! primitive (that needs AppContainer, which is out of scope and breaks many
//! tools). What a Job Object *does* give — and what we take here — is
//! process-tree containment: a child assigned to a `KILL_ON_JOB_CLOSE` job and
//! all its descendants are terminated when we drop the job handle, plus an
//! active-process cap to blunt fork bombs. Data-safety on Windows still rests
//! on the cross-platform approval gate.
//!
//! Job assignment must happen *after* spawn (`AssignProcessToJobObject` needs a
//! process handle), so this is wired at the shell spawn site via
//! [`super::SandboxManager::confine_spawned`], not the pre-spawn
//! `wrap_shell_command` path used elsewhere.

use std::os::windows::io::RawHandle;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

use super::{Enforcement, SandboxBackend, SandboxCapabilities};

/// Cap on concurrent processes in a job — generous, just a fork-bomb backstop.
const ACTIVE_PROCESS_LIMIT: u32 = 512;

#[must_use]
pub fn capabilities() -> SandboxCapabilities {
    // Job Objects are available on every supported Windows release — but they
    // only contain the process *tree*. They do not restrict filesystem writes
    // and do not block network access, so both dimensions report `None` and
    // callers must not describe a command here as "sandboxed" or "offline".
    // This is absence of a mechanism, not a gap within one: `Partial` would
    // claim a boundary exists with holes, when none exists at all.
    SandboxCapabilities {
        backend: SandboxBackend::WindowsJobObject,
        available: true,
        filesystem: Enforcement::None,
        network: Enforcement::None,
        detail: "Windows Job Object (process-tree kill + limits only; \
                 no filesystem or network confinement)"
            .to_string(),
    }
}

/// Owns a Job Object handle. Dropping it (`CloseHandle`) terminates the
/// assigned process tree via `KILL_ON_JOB_CLOSE`, so keep it alive for as long
/// as the child should run.
#[derive(Debug)]
pub struct JobGuard {
    handle: HANDLE,
}

// `HANDLE` is a raw pointer the guard solely owns; it is only used to
// `CloseHandle` on drop, which is sound to move/share across threads.
unsafe impl Send for JobGuard {}
unsafe impl Sync for JobGuard {}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // SAFETY: `handle` is a valid job handle created in `confine` and not
        // closed elsewhere.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Assign an already-spawned child (by raw process handle) to a fresh
/// kill-on-close job with a process cap. Returns the guard to retain, or
/// `None` on any failure (best-effort: the child then runs unconfined,
/// matching the fail-open semantics on other OSes).
#[must_use]
pub fn confine(process: RawHandle) -> Option<JobGuard> {
    // SAFETY: standard Win32 Job Object sequence; every handle is checked and
    // closed on the failure paths.
    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() || job == INVALID_HANDLE_VALUE {
            return None;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.BasicLimitInformation.ActiveProcessLimit = ACTIVE_PROCESS_LIMIT;

        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            mem::size_of_val(&info) as u32,
        );
        if set == 0 {
            CloseHandle(job);
            return None;
        }

        if AssignProcessToJobObject(job, process as HANDLE) == 0 {
            CloseHandle(job);
            return None;
        }

        Some(JobGuard { handle: job })
    }
}

/// The executable a program word names, searched the way `CreateProcess` would
/// search for a bare word — the word itself when it carries a directory, else
/// each `PATH` entry, trying `.exe` and `.com` when the word has no extension —
/// but accepting ONLY a real executable image.
///
/// This is the Windows half of running an unattended command without a shell.
/// A `.cmd`/`.bat` is a script for `cmd.exe`, and launching it hands the
/// arguments straight back to the interpreter this path exists to keep out; a
/// cmd builtin (`echo`, `dir`, `type`) resolves to no file at all. Both are
/// refused: on Windows an unattended command runs a program or does not run.
pub(super) fn resolve_executable(program: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Path, PathBuf};

    let word = Path::new(program);
    let has_extension = word.extension().is_some();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if program.contains(['\\', '/']) {
        if has_extension {
            candidates.push(word.to_path_buf());
        } else {
            candidates.push(word.with_extension("exe"));
            candidates.push(word.with_extension("com"));
        }
    } else {
        let names: Vec<String> = if has_extension {
            vec![program.to_string()]
        } else {
            vec![format!("{program}.exe"), format!("{program}.com")]
        };
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                for name in &names {
                    candidates.push(dir.join(name));
                }
            }
        }
    }
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let extension = candidate
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);
        return match extension.as_deref() {
            Some("exe" | "com") => Ok(candidate),
            _ => Err(format!(
                "`{program}` resolves to {}, which is a cmd.exe script rather than an executable \
                 image; an unattended command runs without cmd.exe, so it cannot run this way",
                candidate.display()
            )),
        };
    }
    Err(format!(
        "`{program}` is not an executable on PATH; an unattended command runs without cmd.exe, so \
         a builtin or a .cmd/.bat wrapper cannot run this way"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::process::{Command, Stdio};

    #[test]
    fn confine_assigns_running_child_to_job() {
        // A child that stays alive briefly so the assignment targets a live
        // process; the guard drop then kills it via KILL_ON_JOB_CLOSE.
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "-n", "5", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test child");

        let guard = confine(child.as_raw_handle());
        assert!(guard.is_some(), "Job Object confinement should succeed");

        drop(guard); // KILL_ON_JOB_CLOSE terminates the process tree.
        let _ = child.kill();
        let _ = child.wait();
    }
}
