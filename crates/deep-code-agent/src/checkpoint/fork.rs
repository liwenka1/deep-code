use std::io::{self, Read, Write};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;

use super::*;

impl CheckpointStore {
    /// Materialize a saved tree in a fresh workspace. Hold a shared lock so
    /// retention cannot prune the tree while it is being copied.
    pub(crate) fn copy_for_fork(
        &self,
        id: &CheckpointId,
        destination: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), ToolError> {
        validate_checkpoint_id(id)?;
        let storage = self.open_storage()?;
        let _lock = StoreLock::acquire(&storage, false, Some(cancel))
            .map_err(|error| ToolError::exec_failed("checkpoint", error.to_string()))?;
        let source = storage
            .open_dir_nofollow(&id.0)
            .map_err(|error| checkpoint_error("open selected checkpoint", error))?;
        let target = crate::pinned_dir::open_canonical(destination)
            .map_err(|error| checkpoint_error("open restored workspace", error))?;
        let copy = WorkspaceCopy {
            cancel,
            #[cfg(unix)]
            original_root: &self.workspace,
            #[cfg(unix)]
            restored_root: destination,
        };
        copy.directory(&source, &target, Path::new(""), &mut [0; 64 * 1024])
            .map_err(|error| checkpoint_error("copy selected checkpoint", error))
    }
}

struct WorkspaceCopy<'a> {
    cancel: &'a CancellationToken,
    #[cfg(unix)]
    original_root: &'a Path,
    #[cfg(unix)]
    restored_root: &'a Path,
}

impl WorkspaceCopy<'_> {
    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            Err(io::Error::other("workspace copy cancelled"))
        } else {
            Ok(())
        }
    }

    fn directory(
        &self,
        source: &Dir,
        target: &Dir,
        relative: &Path,
        buffer: &mut [u8],
    ) -> io::Result<()> {
        for entry in source.entries()? {
            self.check_cancelled()?;
            let entry = entry?;
            let name = entry.file_name();
            let relative = relative.join(&name);
            if should_skip(&relative) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                let source_dir = source.open_dir_nofollow(&name)?;
                target.create_dir(&name)?;
                let target_dir = target.open_dir_nofollow(&name)?;
                self.directory(&source_dir, &target_dir, &relative, buffer)?;
            } else if kind.is_file() {
                self.file(source, target, Path::new(&name), buffer)?;
            } else {
                #[cfg(unix)]
                if kind.is_symlink() {
                    let link = source.read_link_contents(&name)?;
                    let link = link
                        .strip_prefix(self.original_root)
                        .map_or_else(|_| link.clone(), |path| self.restored_root.join(path));
                    target.symlink_contents(link, &name)?;
                    continue;
                }
                return Err(io::Error::other(format!(
                    "unsupported entry in checkpoint: {}",
                    relative.display()
                )));
            }
        }
        Ok(())
    }

    fn file(&self, source: &Dir, target: &Dir, name: &Path, buffer: &mut [u8]) -> io::Result<()> {
        let mut read = OpenOptions::new();
        read.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            read.custom_flags(libc::O_NONBLOCK);
        }
        let mut input = source.open_with(name, &read)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("checkpoint file changed while copying"));
        }
        let mut output = target.open_with(name, OpenOptions::new().write(true).create_new(true))?;
        loop {
            self.check_cancelled()?;
            let length = input.read(buffer)?;
            if length == 0 {
                break;
            }
            output.write_all(&buffer[..length])?;
        }
        output.set_permissions(metadata.permissions())?;
        Ok(())
    }
}
