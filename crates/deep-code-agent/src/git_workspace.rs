//! Host-side Git operations over model-writable repositories.
//!
//! `diff` also runs clean filters, so disabling diff drivers and fsmonitor is
//! insufficient. Only inert metadata queries touch the original Git directory.
//! Every index/worktree operation uses our own configuration, index and object
//! directory, with the source objects available as read-only alternates.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod view;

#[derive(Debug, thiserror::Error)]
pub enum IsolatedGitError {
    #[error("git is not installed")]
    GitMissing,
    #[error("not a Git working tree")]
    NotARepository,
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct GitOutput {
    pub output: Output,
    pub truncated: bool,
}

/// A snapshot of HEAD and the index, with independently owned Git metadata.
/// Keep it alive until its commands finish. Eval creates this before running
/// the model, so edits to HEAD/config/index cannot replace the patch baseline.
#[derive(Debug)]
pub struct IsolatedGit {
    metadata: tempfile::TempDir,
    work_tree: PathBuf,
    directory: PathBuf,
    has_head: bool,
    source: cap_std::fs::Dir,
    source_path: PathBuf,
    index: Vec<u8>,
    baseline: String,
    source_symlinks: bool,
    #[cfg(unix)]
    source_filemode: bool,
    view_links: Vec<(PathBuf, Vec<u8>)>,
}

impl IsolatedGit {
    pub fn new(workspace: &Path) -> Result<Self, IsolatedGitError> {
        Self::new_pinned(&crate::paths::canonicalize(workspace)?)
    }

    /// Use a root already approved by a live UI without resolving a replacement
    /// symlink as fresh user intent.
    pub fn new_pinned(workspace: &Path) -> Result<Self, IsolatedGitError> {
        // System temp roots are writable by sandboxed tools. Keep the config
        // and index under the host's protected state directory instead.
        let state = crate::paths::home_dir()
            .ok_or_else(|| {
                IsolatedGitError::Failed(
                    "no absolute home directory for isolated Git metadata".into(),
                )
            })?
            .join(crate::paths::DEEP_CODE_DIR)
            .join("git");
        Self::new_in_pinned(workspace, &state)
    }

    #[cfg(test)]
    fn new_in(workspace: &Path, storage: &Path) -> Result<Self, IsolatedGitError> {
        Self::new_in_pinned(&crate::paths::canonicalize(workspace)?, storage)
    }

    fn new_in_pinned(workspace: &Path, storage: &Path) -> Result<Self, IsolatedGitError> {
        crate::pinned_dir::validate_pinned_workspace(workspace)?;
        let caller = workspace.to_path_buf();
        let physical_root = caller
            .ancestors()
            .find(|directory| directory.join(".git").symlink_metadata().is_ok())
            .ok_or(IsolatedGitError::NotARepository)?
            .to_path_buf();
        let source = crate::pinned_dir::open_canonical(&physical_root)?;
        let inside = query(workspace, &["rev-parse", "--is-inside-work-tree"])?;
        if !inside.status.success() || line(&inside.stdout) != b"true" {
            return Err(IsolatedGitError::NotARepository);
        }
        let objects = checked(query(
            workspace,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
        )?)?;
        let objects = line(&objects);
        // Alternates are newline-delimited paths. Reject an unrepresentable
        // path instead of letting one path introduce additional object stores.
        if objects.contains(&b'\n') || objects.contains(&b'\r') {
            return Err(IsolatedGitError::Failed(
                "newline in Git object directory path".into(),
            ));
        }
        let format = checked(query(workspace, &["rev-parse", "--show-object-format"])?)?;
        let format = match line(&format) {
            b"sha1" => "sha1",
            b"sha256" => "sha256",
            _ => {
                return Err(IsolatedGitError::Failed(
                    "unsupported Git object format".into(),
                ));
            }
        };
        let head = query(workspace, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
        let head = if head.status.success() {
            let oid = line(&head.stdout);
            let length = if format == "sha1" { 40 } else { 64 };
            if oid.len() != length || !oid.iter().all(u8::is_ascii_hexdigit) {
                return Err(IsolatedGitError::Failed(
                    "invalid Git HEAD object id".into(),
                ));
            }
            Some(oid.to_vec())
        } else if head.status.code() == Some(1) {
            None // Unborn branch: diff against the index, as Git normally does.
        } else {
            checked(head)?;
            unreachable!()
        };
        let top = checked(query(workspace, &["rev-parse", "--show-toplevel"])?)?;
        #[cfg(unix)]
        let top = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(line(&top).to_vec()))
        };
        #[cfg(not(unix))]
        let top = PathBuf::from(std::str::from_utf8(line(&top)).map_err(|_| {
            IsolatedGitError::Failed("Git working-tree path is not valid UTF-8".into())
        })?);
        let top = crate::paths::canonicalize(&top)?;
        if top != physical_root {
            return Err(IsolatedGitError::Failed(
                "Git configuration redirects the working tree outside its repository".into(),
            ));
        }
        let index = checked(query(&top, &["ls-files", "--stage", "-z"])?)?;
        let content_config = content_config(workspace)?;

        // Concurrent /diff operations may create these directories together.
        if let Err(error) = crate::paths::ensure_owned_dirs(storage, 2) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            crate::paths::ensure_owned_dirs(storage, 2)?;
        }
        if crate::paths::canonicalize(storage)?.starts_with(&caller) {
            return Err(IsolatedGitError::Failed(
                "isolated Git metadata must be outside the writable workspace".into(),
            ));
        }
        let metadata = tempfile::Builder::new()
            .prefix("snapshot-")
            .tempdir_in(storage)?;
        checked(output(
            clean_git()
                .current_dir(metadata.path())
                .args([
                    "init",
                    "--bare",
                    "--quiet",
                    "--template=",
                    &format!("--object-format={format}"),
                ])
                .arg(metadata.path()),
        )?)?;
        std::fs::OpenOptions::new()
            .append(true)
            .open(metadata.path().join("config"))?
            .write_all(content_config.as_bytes())?;
        let mut alternates = objects.to_vec();
        alternates.push(b'\n');
        std::fs::write(metadata.path().join("objects/info/alternates"), alternates)?;
        if let Some(head) = &head {
            let mut detached = head.clone();
            detached.push(b'\n');
            std::fs::write(metadata.path().join("HEAD"), detached)?;
        }
        let work_tree = metadata.path().join("worktree");
        let directory = work_tree.join(
            caller
                .strip_prefix(&top)
                .map_err(|_| IsolatedGitError::NotARepository)?,
        );
        std::fs::create_dir_all(&directory)?;
        let mut snapshot = Self {
            metadata,
            work_tree,
            directory,
            has_head: head.is_some(),
            source,
            source_path: top,
            index,
            baseline: "HEAD".to_string(),
            source_symlinks: !content_config.contains("\tsymlinks = false\n"),
            #[cfg(unix)]
            source_filemode: !content_config.contains("\tfilemode = false\n"),
            view_links: Vec::new(),
        };
        snapshot.restore_index()?;
        if !snapshot.has_head {
            let tree = checked(output(snapshot.command().arg("write-tree"))?)?;
            snapshot.baseline = String::from_utf8(line(&tree).to_vec())
                .map_err(|_| IsolatedGitError::Failed("invalid baseline tree id".into()))?;
        }
        snapshot.refresh()?;
        crate::pinned_dir::validate_pinned_workspace(workspace)?;
        Ok(snapshot)
    }

    pub fn diff_base(&self) -> &str {
        &self.baseline
    }

    pub fn has_head(&self) -> bool {
        self.has_head
    }

    pub fn run_bounded(
        &self,
        args: &[&str],
        byte_limit: usize,
    ) -> Result<GitOutput, IsolatedGitError> {
        run_output(
            self.command().args(args),
            None,
            byte_limit,
            std::time::Duration::from_secs(20),
        )
    }

    /// Only validated file-content settings are imported. Executable config
    /// and inherited secrets are absent; in-tree filters have no definitions.
    /// Diffs must also use `--ignore-submodules=dirty` to prevent Git from
    /// spawning status queries inside separately configured submodules.
    pub fn command(&self) -> Command {
        self.command_at(&self.work_tree, &self.directory)
    }

    fn source_listing(&self) -> Command {
        self.command_at(&self.source_path, &self.source_path)
    }

    fn command_at(&self, work_tree: &Path, directory: &Path) -> Command {
        let mut command = clean_git();
        command
            .current_dir(self.metadata.path())
            .arg("-C")
            .arg(directory)
            .arg("--git-dir")
            .arg(self.metadata.path())
            .arg("--work-tree")
            .arg(work_tree)
            .args([
                "-c",
                "core.bare=false",
                "-c",
                "core.symlinks=false",
                "-c",
                // Read gitlink commits, but never spawn `git status` inside
                // a submodule whose own configuration is model-controlled.
                "diff.ignoreSubmodules=dirty",
                "-c",
                "submodule.recurse=false",
            ]);
        command
    }
}

fn line(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

/// Copy only settings that affect how ordinary file bytes/modes are compared.
/// In particular, Windows checkouts may have CRLF from a global autocrlf
/// setting. Reading that setting is safe; importing arbitrary config is not.
fn content_config(workspace: &Path) -> Result<String, IsolatedGitError> {
    let mut command = user_config_command();
    let result = output(command.arg("-C").arg(workspace).args([
        "config",
        "--null",
        "--get-regexp",
        r"^core\.(autocrlf|eol|filemode|ignorecase|symlinks|precomposeunicode)$",
    ]))?;
    if result.status.code() == Some(1) {
        return Ok(String::new());
    }
    let values = checked(result)?;
    let mut settings = std::collections::BTreeMap::new();
    for entry in values
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let split = entry
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(entry.len());
        let key = std::str::from_utf8(&entry[..split])
            .map_err(|_| IsolatedGitError::Failed("invalid Git config key".into()))?;
        let value = if split == entry.len() {
            "true".to_string() // A key without `=` is Git's implicit true.
        } else {
            std::str::from_utf8(&entry[split + 1..])
                .map_err(|_| IsolatedGitError::Failed("invalid Git config value".into()))?
                .to_ascii_lowercase()
        };
        let value = match (key, value.as_str()) {
            ("core.eol", "lf" | "crlf" | "native") | ("core.autocrlf", "input") => value.as_str(),
            ("core.eol", _) => {
                return Err(IsolatedGitError::Failed("invalid core.eol setting".into()));
            }
            (_, "true" | "yes" | "on" | "1") => "true",
            (_, "false" | "no" | "off" | "0" | "") => "false",
            _ => {
                let normalized = checked(output(
                    user_config_command().arg("-C").arg(workspace).args([
                        "config",
                        "--type=bool",
                        "--get",
                        key,
                    ]),
                )?)?;
                match line(&normalized) {
                    b"true" => "true",
                    b"false" => "false",
                    _ => return Err(IsolatedGitError::Failed(format!("invalid {key} setting"))),
                }
            }
        };
        let key = match key {
            "core.autocrlf" => "autocrlf",
            "core.eol" => "eol",
            "core.filemode" => "filemode",
            "core.ignorecase" => "ignorecase",
            "core.symlinks" => "symlinks",
            "core.precomposeunicode" => "precomposeunicode",
            _ => return Err(IsolatedGitError::Failed("unexpected Git config key".into())),
        };
        settings.insert(key, value.to_string());
    }
    let mut config = String::from("\n[core]\n");
    for (key, value) in settings {
        config.push_str(&format!("\t{key} = {value}\n"));
    }
    Ok(config)
}

fn user_config_command() -> Command {
    let mut command = clean_git();
    // This one inert config query may read the user's config layers. None of
    // their aliases, filters, hooks or transport settings reach later commands.
    for name in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_GLOBAL",
    ] {
        command.env_remove(name);
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn clean_git() -> Command {
    let mut command = Command::new("git");
    command.env_clear();
    // Git for Windows and Apple's xcrun shim need these OS/runtime locations.
    for name in ["PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", null)
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-c",
        ])
        .arg(format!("core.hooksPath={null}"))
        .stdin(Stdio::null());
    command
}

fn query(workspace: &Path, args: &[&str]) -> Result<Output, IsolatedGitError> {
    // These built-ins only read refs, object locations or index entries.
    // Never use this path for commands that inspect working-tree contents.
    output(clean_git().arg("-C").arg(workspace).args(args))
}

fn output(command: &mut Command) -> Result<Output, IsolatedGitError> {
    output_with_input(command, None)
}

fn output_input(command: &mut Command, input: Vec<u8>) -> Result<Output, IsolatedGitError> {
    output_with_input(command, Some(input))
}

fn output_with_input(
    command: &mut Command,
    input: Option<Vec<u8>>,
) -> Result<Output, IsolatedGitError> {
    let result = run_output(
        command,
        input,
        64 * 1024 * 1024,
        std::time::Duration::from_secs(20),
    )?;
    if result.truncated {
        return Err(IsolatedGitError::Failed("Git output exceeds 64 MiB".into()));
    }
    Ok(result.output)
}

fn run_output(
    command: &mut Command,
    input: Option<Vec<u8>>,
    limit: usize,
    timeout: std::time::Duration,
) -> Result<GitOutput, IsolatedGitError> {
    let deadline = std::time::Instant::now() + timeout;
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                IsolatedGitError::GitMissing
            } else {
                error.into()
            }
        })?;
    fn read_pipe(
        reader: impl Read + Send + 'static,
        limit: usize,
    ) -> std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = reader
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send(result);
        });
        rx
    }
    let stdout = read_pipe(child.stdout.take().expect("piped stdout"), limit);
    let stderr = read_pipe(child.stderr.take().expect("piped stderr"), 8192);
    let writer = input.map(|input| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(stdin.write_all(&input));
        });
        rx
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(IsolatedGitError::Failed("Git command timed out".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let receive = |rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>| {
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .map_err(|_| IsolatedGitError::Failed("Git output pipe timed out".into()))?
            .map_err(IsolatedGitError::from)
    };
    let mut stdout = receive(stdout)?;
    let mut stderr = receive(stderr)?;
    let truncated = stdout.len() > limit;
    stdout.truncate(limit);
    stderr.truncate(8192);
    if let Some(writer) = writer {
        let written = writer
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .map_err(|_| IsolatedGitError::Failed("Git input pipe timed out".into()))?;
        if status.success() {
            written?;
        }
    }
    Ok(GitOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        truncated,
    })
}

fn checked(output: Output) -> Result<Vec<u8>, IsolatedGitError> {
    if !output.status.success() {
        return Err(IsolatedGitError::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(workspace: &Path, args: &[&str]) -> Vec<u8> {
        checked(
            output(
                clean_git()
                    .current_dir(workspace)
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                    .args(args),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("working tree");
        std::fs::create_dir(&workspace).unwrap();
        git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("tracked.txt"), "before\n").unwrap();
        git(&workspace, &["add", "."]);
        git(&workspace, &["commit", "-qm", "baseline"]);
        (root, workspace)
    }

    #[test]
    fn worktree_operations_do_not_execute_repository_programs_or_modify_its_index() {
        let (root, workspace) = fixture();
        let index = std::fs::read(workspace.join(".git/index")).unwrap();
        let marker = root
            .path()
            .join("executed")
            .to_string_lossy()
            .replace('\\', "/");
        let program = format!("echo executed > '{marker}'; cat");
        for key in [
            "filter.evil.clean",
            "filter.evil.process",
            "diff.evil.command",
            "diff.evil.textconv",
            "core.fsmonitor",
        ] {
            git(&workspace, &["config", key, &program]);
        }
        git(&workspace, &["config", "filter.evil.required", "true"]);
        std::fs::write(
            workspace.join(".gitattributes"),
            "*.txt filter=evil diff=evil\n",
        )
        .unwrap();
        std::fs::write(workspace.join("tracked.txt"), "after\n").unwrap();
        std::fs::write(workspace.join("new.txt"), "new\n").unwrap();
        let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        let diff = checked(
            output(
                isolated
                    .command()
                    .args(["diff", "--no-ext-diff", "--no-textconv", "HEAD"]),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&diff).contains("+after"));
        checked(output(isolated.command().args(["add", "-A"])).unwrap()).unwrap();
        let diff = checked(
            output(
                isolated
                    .command()
                    .args(["diff", "--cached", "--no-ext-diff", "--no-textconv"]),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&diff).contains("+new"));
        assert!(!root.path().join("executed").exists());
        assert_eq!(std::fs::read(workspace.join(".git/index")).unwrap(), index);
    }

    #[test]
    fn linked_worktrees_preserve_staged_additions_and_untracked_files() {
        let (root, workspace) = fixture();
        let linked = root.path().join("linked tree");
        git(
            &workspace,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(linked.join("staged.txt"), "staged\n").unwrap();
        git(&linked, &["add", "staged.txt"]);
        std::fs::write(linked.join("untracked.txt"), "untracked\n").unwrap();
        let isolated = IsolatedGit::new_in(&linked, &root.path().join("state/git")).unwrap();
        let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&diff).contains("+staged"));
        let untracked = checked(
            output(
                isolated
                    .command()
                    .args(["ls-files", "--others", "--exclude-standard"]),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(untracked, b"untracked.txt\n");
    }

    #[test]
    fn unborn_repositories_diff_against_the_index() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("new.txt"), "first\n").unwrap();
        git(&workspace, &["add", "new.txt"]);
        std::fs::write(workspace.join("new.txt"), "second\n").unwrap();
        let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        assert!(!isolated.has_head());
        let diff = checked(output(isolated.command().arg("diff")).unwrap()).unwrap();
        let diff = String::from_utf8_lossy(&diff);
        assert!(diff.contains("-first") && diff.contains("+second"));
    }

    #[test]
    fn redirected_worktrees_cannot_supply_private_contents_to_git() {
        let (root, workspace) = fixture();
        let outside = root.path().join("private-checkout");
        git(
            root.path(),
            &[
                "clone",
                "-q",
                workspace.to_str().unwrap(),
                outside.to_str().unwrap(),
            ],
        );
        std::fs::write(outside.join("tracked.txt"), "private-sentinel\n").unwrap();
        let mut isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        let approved = workspace.canonicalize().unwrap();
        std::fs::rename(&workspace, root.path().join("previous-workspace")).unwrap();
        if !crate::test_symlinks::symlink_dir_for_test(&outside, &workspace) {
            return;
        }
        let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
        assert!(diff.is_empty(), "{}", String::from_utf8_lossy(&diff));
        assert!(isolated.refresh().is_err());
        assert!(IsolatedGit::new_in_pinned(&approved, &root.path().join("state/git")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn working_copy_creation_does_not_change_source_inode_metadata() {
        use std::os::unix::fs::MetadataExt;
        let (root, workspace) = fixture();
        let file = workspace.join("tracked.txt");
        let before = file.metadata().unwrap();
        let _isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        let after = file.metadata().unwrap();
        assert_eq!(
            (before.nlink(), before.ctime(), before.ctime_nsec()),
            (after.nlink(), after.ctime(), after.ctime_nsec())
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_input_and_stderr_are_drained_concurrently() {
        let mut command = Command::new("/bin/sh");
        command.env_clear().env("PATH", "/usr/bin:/bin").args(["-c", "awk 'BEGIN { for (i=0; i<20000; i++) print \"noise\" }' >&2; cat >/dev/null; printf done"]);
        let result = run_output(
            &mut command,
            Some(vec![1; 1024 * 1024]),
            4096,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(result.output.status.success());
        assert_eq!(result.output.stdout, b"done");
        assert!(result.output.stderr.len() <= 8192);
    }

    #[test]
    fn directory_symlinks_are_data_and_never_read_as_directories() {
        let (root, workspace) = fixture();
        std::fs::create_dir(workspace.join("folder")).unwrap();
        std::fs::write(workspace.join("folder/file.txt"), "inside\n").unwrap();
        git(&workspace, &["add", "."]);
        git(&workspace, &["commit", "-qm", "folder"]);
        std::fs::rename(
            workspace.join("folder"),
            root.path().join("previous-folder"),
        )
        .unwrap();
        let outside = root.path().join("private");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "private-sentinel\n").unwrap();
        if !crate::test_symlinks::symlink_dir_for_test(&outside, &workspace.join("folder")) {
            return;
        }
        let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&diff).contains("private-sentinel"));
        assert!(isolated.stage_all().unwrap().status.success());
        let entries =
            checked(output(isolated.command().args(["ls-files", "--stage"])).unwrap()).unwrap();
        assert!(
            String::from_utf8_lossy(&entries)
                .lines()
                .any(|line| line.starts_with("120000 ") && line.ends_with("\tfolder"))
        );
    }

    #[test]
    fn symlink_type_changes_keep_git_modes_and_never_follow_targets() {
        for original_link in [false, true] {
            let (root, workspace) = fixture();
            let name = workspace.join("tracked.txt");
            if original_link {
                std::fs::remove_file(&name).unwrap();
                if !crate::test_symlinks::symlink_file_for_test(Path::new("missing-target"), &name)
                {
                    return;
                }
                git(&workspace, &["add", "."]);
                git(&workspace, &["commit", "-qm", "link"]);
                std::fs::remove_file(&name).unwrap();
                std::fs::write(&name, "regular\n").unwrap();
            } else {
                std::fs::remove_file(&name).unwrap();
                if !crate::test_symlinks::symlink_file_for_test(Path::new("missing-target"), &name)
                {
                    return;
                }
            }
            let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
            let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
            let diff = String::from_utf8_lossy(&diff);
            assert!(diff.contains("120000") && diff.contains("100644"), "{diff}");
        }
    }

    #[test]
    fn symlinked_git_control_files_do_not_become_rules_in_the_private_view() {
        for tracked in [false, true] {
            let (root, workspace) = fixture();
            for (name, target) in [(".gitignore", "*"), (".gitattributes", "* text")] {
                if !crate::test_symlinks::symlink_file_for_test(
                    Path::new(target),
                    &workspace.join(name),
                ) {
                    return;
                }
            }
            if tracked {
                git(&workspace, &["add", ".gitignore", ".gitattributes"]);
                git(&workspace, &["commit", "-qm", "control links"]);
            }
            std::fs::write(workspace.join("untracked.txt"), b"raw\r\n").unwrap();
            let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
            let names = checked(
                output(
                    isolated
                        .command()
                        .args(["ls-files", "--others", "--exclude-standard"]),
                )
                .unwrap(),
            )
            .unwrap();
            assert!(String::from_utf8_lossy(&names).contains("untracked.txt"));
            let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
            assert!(diff.is_empty(), "{}", String::from_utf8_lossy(&diff));
            checked(isolated.stage_all().unwrap()).unwrap();
            assert_eq!(
                checked(output(isolated.command().args(["show", ":untracked.txt"])).unwrap())
                    .unwrap(),
                b"raw\r\n"
            );
            assert_eq!(
                checked(output(isolated.command().args(["show", ":.gitignore"])).unwrap()).unwrap(),
                b"*"
            );
            let entries = checked(
                output(
                    isolated
                        .command()
                        .args(["ls-files", "--stage", ".gitignore"]),
                )
                .unwrap(),
            )
            .unwrap();
            assert!(entries.starts_with(b"120000 "));
        }
    }

    #[test]
    fn a_directory_replacing_a_file_is_staged_as_files_instead_of_a_gitlink() {
        let (root, workspace) = fixture();
        let directory = workspace.join("tracked.txt");
        std::fs::remove_file(&directory).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("child.txt"), "child\n").unwrap();
        let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        checked(isolated.stage_all().unwrap()).unwrap();
        let diff = checked(output(isolated.command().args(["diff", "--cached"])).unwrap()).unwrap();
        let diff = String::from_utf8_lossy(&diff);
        assert!(
            diff.contains("tracked.txt/child.txt") && diff.contains("+child"),
            "{diff}"
        );
        assert!(!diff.contains("Subproject commit"), "{diff}");
    }

    #[test]
    fn an_unpopulated_gitlink_keeps_its_original_commit() {
        let (root, workspace) = fixture();
        let commit = git(&workspace, &["rev-parse", "HEAD"]);
        let commit = std::str::from_utf8(line(&commit)).unwrap();
        git(
            &workspace,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                commit,
                "nested",
            ],
        );
        git(&workspace, &["commit", "-qm", "unpopulated gitlink"]);
        std::fs::create_dir(workspace.join("nested")).unwrap();
        let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        let diff = checked(
            output(
                isolated
                    .command()
                    .args(["diff", "--ignore-submodules=dirty", "HEAD"]),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(diff.is_empty(), "{}", String::from_utf8_lossy(&diff));
        checked(isolated.stage_all().unwrap()).unwrap();
        let diff = checked(output(isolated.command().args(["diff", "--cached"])).unwrap()).unwrap();
        assert!(diff.is_empty(), "{}", String::from_utf8_lossy(&diff));
    }

    #[cfg(unix)]
    #[test]
    fn metadata_queries_timeout_instead_of_hanging_on_a_fifo() {
        let (root, workspace) = fixture();
        std::fs::remove_file(workspace.join(".git/config")).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(workspace.join(".git/config"))
                .status()
                .unwrap()
                .success()
        );
        let result = run_output(
            clean_git()
                .arg("-C")
                .arg(&workspace)
                .args(["rev-parse", "HEAD"]),
            None,
            4096,
            std::time::Duration::from_millis(200),
        );
        assert!(
            matches!(result, Err(IsolatedGitError::Failed(message)) if message.contains("timed out"))
        );
        drop(root);
    }

    #[test]
    fn paths_from_a_subdirectory_keep_their_original_git_meaning() {
        let (root, workspace) = fixture();
        let directory = workspace.join("subdirectory");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("nested.txt"), "before\n").unwrap();
        git(&workspace, &["add", "."]);
        git(&workspace, &["commit", "-qm", "nested"]);
        std::fs::write(directory.join("nested.txt"), "after\n").unwrap();
        let isolated = IsolatedGit::new_in(&directory, &root.path().join("state/git")).unwrap();
        let diff = checked(
            output(
                isolated
                    .command()
                    .args(["diff", "HEAD", "--", "nested.txt"]),
            )
            .unwrap(),
        )
        .unwrap();
        let diff = String::from_utf8_lossy(&diff);
        assert!(
            diff.contains("a/subdirectory/nested.txt") && diff.contains("+after"),
            "{diff}"
        );
        let all = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
        assert!(
            !String::from_utf8_lossy(&all).contains("tracked.txt"),
            "the full index must be retained"
        );
    }

    #[test]
    fn content_settings_preserve_crlf_file_modes_and_git_boolean_semantics() {
        for setting in [
            "autocrlf = true",
            "autocrlf",
            "autocrlf =",
            "autocrlf = 2",
            "autocrlf = 1k",
        ] {
            let (root, workspace) = fixture();
            let mut config = std::fs::OpenOptions::new()
                .append(true)
                .open(workspace.join(".git/config"))
                .unwrap();
            writeln!(config, "\n[core]\n{setting}\nfilemode = false").unwrap();
            let expected = git(
                &workspace,
                &["config", "--type=bool", "--get", "core.autocrlf"],
            );
            std::fs::write(
                workspace.join("tracked.txt"),
                if expected == b"true\n" {
                    &b"before\r\n"[..]
                } else {
                    &b"before\n"[..]
                },
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    workspace.join("tracked.txt"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
            let actual = checked(
                output(isolated.command().args([
                    "config",
                    "--type=bool",
                    "--get",
                    "core.autocrlf",
                ]))
                .unwrap(),
            )
            .unwrap();
            assert_eq!(actual, expected);
            let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
            assert!(diff.is_empty(), "{}", String::from_utf8_lossy(&diff));
        }
    }

    #[test]
    fn submodule_commits_are_visible_without_running_submodule_filters() {
        let (root, workspace) = fixture();
        let nested = workspace.join("nested");
        std::fs::create_dir(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        std::fs::write(nested.join("file.txt"), "before\n").unwrap();
        git(&nested, &["add", "."]);
        git(&nested, &["commit", "-qm", "submodule"]);
        std::fs::write(
            workspace.join(".gitmodules"),
            "[submodule \"nested\"]\npath = nested\nurl = unused\nignore = none\n",
        )
        .unwrap();
        git(&workspace, &["add", "."]);
        git(&workspace, &["commit", "-qm", "gitlink"]);
        git(&nested, &["commit", "--allow-empty", "-qm", "new gitlink"]);
        let marker = root
            .path()
            .join("executed")
            .to_string_lossy()
            .replace('\\', "/");
        git(
            &nested,
            &[
                "config",
                "filter.evil.clean",
                &format!("echo executed > '{marker}'; cat"),
            ],
        );
        std::fs::write(nested.join(".gitattributes"), "file.txt filter=evil\n").unwrap();
        std::fs::write(nested.join("file.txt"), "after\n").unwrap();
        let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
        let diff = checked(
            output(
                isolated
                    .command()
                    .args(["diff", "--ignore-submodules=dirty", "HEAD"]),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&diff).contains("Subproject commit"));
        assert!(!root.path().join("executed").exists());
    }

    #[test]
    fn commands_clear_inherited_secrets_and_git_environment() {
        const CHILD: &str = "DEEP_CODE_GIT_ENV_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let (root, workspace) = fixture();
            std::fs::write(workspace.join("tracked.txt"), "before\r\n").unwrap();
            let isolated = IsolatedGit::new_in(&workspace, &root.path().join("state/git")).unwrap();
            let diff = checked(output(isolated.command().args(["diff", "HEAD"])).unwrap()).unwrap();
            assert!(diff.is_empty(), "global autocrlf must be preserved");
            assert_eq!(
                output(
                    isolated
                        .command()
                        .args(["config", "--get", "alias.inherited"])
                )
                .unwrap()
                .status
                .code(),
                Some(1)
            );
            checked(output(isolated.command().args([
                "-c", r#"alias.check-env=!test -z "${DEEPSEEK_API_KEY}${DEEP_CODE_RUNTIME_TOKEN}${GIT_CONFIG_COUNT}""#,
                "check-env",
            ])).unwrap()).unwrap();
            return;
        }
        // A subprocess gives this test real inherited values without mutating
        // the environment beneath other concurrently running tests.
        let global = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            global.path(),
            "[core]\nautocrlf = true\n[alias]\ninherited = !false\n",
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_workspace::tests::commands_clear_inherited_secrets_and_git_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("DEEPSEEK_API_KEY", "sentinel")
            .env("DEEP_CODE_RUNTIME_TOKEN", "sentinel")
            .env("GIT_DIR", "does-not-exist")
            .env("GIT_INDEX_FILE", "does-not-exist")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_GLOBAL", global.path())
            .env("GIT_CONFIG_KEY_0", "alias.inherited")
            .env("GIT_CONFIG_VALUE_0", "!false")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
}
