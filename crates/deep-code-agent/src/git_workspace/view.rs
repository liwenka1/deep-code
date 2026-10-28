//! Populate a private working tree through directory capabilities. Git never
//! traverses the model's live filesystem; symlinks are represented as ordinary
//! files using Git's own symlink-emulation mode.

use super::*;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Component;

impl IsolatedGit {
    pub(super) fn restore_index(&self) -> Result<(), IsolatedGitError> {
        checked(output(self.command().args(["read-tree", "--empty"]))?)?;
        checked(output_input(
            self.command().args(["update-index", "-z", "--index-info"]),
            self.index.clone(),
        )?)?;
        Ok(())
    }

    /// Refresh file contents while retaining the original refs, configuration
    /// and index. Eval calls this after stopping all model work.
    pub fn refresh(&mut self) -> Result<(), IsolatedGitError> {
        crate::pinned_dir::validate_pinned_workspace(&self.source_path)?;
        self.restore_index()?;
        self.view_links.clear();
        let mut paths = BTreeMap::new();
        // Track original modes, never modes changed by a previous refresh/add.
        for entry in self
            .index
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| IsolatedGitError::Failed("invalid Git index entry".into()))?;
            let header = std::str::from_utf8(&entry[..tab])
                .map_err(|_| IsolatedGitError::Failed("invalid Git index header".into()))?;
            let mut header = header.split_whitespace();
            let mode = header
                .next()
                .and_then(|mode| u32::from_str_radix(mode, 8).ok())
                .ok_or_else(|| IsolatedGitError::Failed("invalid Git mode".into()))?;
            let oid = header.next().unwrap_or_default().to_string();
            let stage = header.next().unwrap_or_default();
            paths.insert(path(&entry[tab + 1..])?, (mode, oid, stage == "0"));
        }
        // An inert listing supplies ignored/untracked semantics. Every name is
        // checked again through the pinned source handle before it is used.
        let untracked = checked(output(self.source_listing().args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--full-name",
            "-z",
        ]))?)?;
        for name in untracked
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let name = path(name)?;
            if self
                .metadata
                .path()
                .parent()
                .and_then(|storage| storage.strip_prefix(&self.source_path).ok())
                .is_some_and(|storage| name.starts_with(storage))
            {
                continue;
            }
            paths.entry(name).or_insert((0, String::new(), false));
        }
        // No untrusted path is ever used in this protected namespace.
        std::fs::remove_dir_all(&self.work_tree)?;
        std::fs::create_dir_all(&self.directory)?;
        let destination = Dir::open_ambient_dir(&self.work_tree, cap_std::ambient_authority())?;
        let mut copied_bytes = 0u64;
        for (relative, (old_mode, old_oid, indexed)) in paths {
            let Some((parent, name)) = source_parent(&self.source, &relative)? else {
                continue;
            };
            let metadata = match parent.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let target_parent = relative
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            destination.create_dir_all(target_parent)?;
            let target = destination.open_dir(target_parent)?;
            if metadata.file_type().is_symlink() {
                let link = parent.read_link_contents(&name)?;
                let bytes = link.as_os_str().as_encoded_bytes();
                // Git ignores symlinked control files. An emulated link must
                // not turn its target spelling into ignore/attribute rules.
                let control = name.to_str().is_some_and(|name| {
                    name.eq_ignore_ascii_case(".gitignore")
                        || name.eq_ignore_ascii_case(".gitattributes")
                });
                target.write(&name, if control { &[] } else { bytes })?;
                if indexed {
                    self.link_entry(&relative, bytes)?;
                    if control {
                        checked(output(
                            self.command()
                                .args(["update-index", "--assume-unchanged", "--"])
                                .arg(&relative),
                        )?)?;
                    }
                }
                self.view_links.push((relative.clone(), bytes.to_vec()));
            } else if metadata.is_file() {
                let mut options = OpenOptions::new();
                options.read(true).follow(FollowSymlinks::No);
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let mut file = parent.open_with(&name, &options)?.into_std();
                let opened = file.metadata()?;
                if !opened.is_file() {
                    return Err(IsolatedGitError::Failed(
                        "working-tree entry is not a regular file".into(),
                    ));
                }
                copied_bytes = copied_bytes.saturating_add(opened.len());
                if copied_bytes > 512 * 1024 * 1024 {
                    return Err(IsolatedGitError::Failed(
                        "Git working copy exceeds the 512 MiB copy budget".into(),
                    ));
                }
                let mut copy = target.create(&name)?.into_std();
                std::io::copy(
                    &mut std::io::Read::by_ref(&mut file).take(opened.len()),
                    &mut copy,
                )?;
                #[cfg(unix)]
                copy.set_permissions(opened.permissions())?;
                if indexed && old_mode == 0o120000 && self.source_symlinks {
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        self.source_filemode && opened.permissions().mode() & 0o100 != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    self.cache_entry(
                        &relative,
                        if executable { "100755" } else { "100644" },
                        &old_oid,
                    )?;
                }
            } else if metadata.is_dir() {
                destination.create_dir_all(&relative)?;
                let directory = parent.open_dir_nofollow(&name)?;
                if directory.symlink_metadata(".git").is_err() {
                    // An ordinary directory replacing a tracked file, or an
                    // unpopulated gitlink, must not inherit the outer HEAD.
                    continue;
                }
                let head = query(
                    &self.source_path.join(&relative),
                    &["rev-parse", "--verify", "--quiet", "HEAD"],
                )?;
                let oid = line(&head.stdout);
                if head.status.success()
                    && matches!(oid.len(), 40 | 64)
                    && oid.iter().all(u8::is_ascii_hexdigit)
                {
                    let git = self.work_tree.join(&relative).join(".git");
                    std::fs::create_dir_all(git.join("objects"))?;
                    std::fs::create_dir_all(git.join("refs"))?;
                    std::fs::write(git.join("HEAD"), &head.stdout)?;
                    let config = if oid.len() == 64 {
                        "[core]\nrepositoryformatversion = 1\n[extensions]\nobjectformat = sha256\n"
                    } else {
                        "[core]\nrepositoryformatversion = 0\n"
                    };
                    std::fs::write(git.join("config"), config)?;
                }
            } else {
                return Err(IsolatedGitError::Failed(
                    "working-tree entry is a FIFO, device or socket".into(),
                ));
            }
        }
        crate::pinned_dir::validate_pinned_workspace(&self.source_path)?;
        Ok(())
    }

    fn cache_entry(&self, path: &Path, mode: &str, oid: &str) -> Result<(), IsolatedGitError> {
        checked(output(
            self.command()
                .args(["update-index", "--add", "--cacheinfo", mode, oid])
                .arg(path),
        )?)?;
        Ok(())
    }

    fn link_entry(&self, path: &Path, contents: &[u8]) -> Result<(), IsolatedGitError> {
        let oid = checked(output_input(
            self.command()
                .args(["hash-object", "-w", "--no-filters", "--stdin"]),
            contents.to_vec(),
        )?)?;
        let oid = std::str::from_utf8(line(&oid))
            .map_err(|_| IsolatedGitError::Failed("invalid blob id".into()))?;
        self.cache_entry(path, "120000", oid)
    }

    /// `git add` sees emulated links as ordinary files; restore their modes and
    /// raw target bytes after staging, without ever creating a live symlink.
    pub fn stage_all(&self) -> Result<Output, IsolatedGitError> {
        let added = output(
            self.command()
                .args(["add", "-A", "--", ".", ":(exclude).deep-code"]),
        )?;
        if added.status.success() {
            for (relative, target) in &self.view_links {
                if !relative.starts_with(".deep-code") {
                    self.link_entry(relative, target)?;
                }
            }
        }
        Ok(added)
    }
}

fn path(bytes: &[u8]) -> Result<PathBuf, IsolatedGitError> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(
        std::str::from_utf8(bytes)
            .map_err(|_| IsolatedGitError::Failed("Git path is not UTF-8".into()))?,
    );
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(name) if name != OsStr::new(".git")))
    {
        return Err(IsolatedGitError::Failed("invalid working-tree path".into()));
    }
    Ok(path)
}

fn source_parent(
    root: &Dir,
    path: &Path,
) -> Result<Option<(Dir, std::ffi::OsString)>, IsolatedGitError> {
    let mut parent = root.try_clone()?;
    for name in path.parent().unwrap_or(Path::new("")).components() {
        match parent.open_dir_nofollow(name.as_os_str()) {
            Ok(directory) => parent = directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(_)
                if parent
                    .symlink_metadata(name.as_os_str())
                    .is_ok_and(|metadata| metadata.file_type().is_symlink()) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Some((
        parent,
        path.file_name()
            .ok_or_else(|| IsolatedGitError::Failed("invalid Git path".into()))?
            .to_os_string(),
    )))
}
