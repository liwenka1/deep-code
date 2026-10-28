//! Directory handles for host-side I/O into model-writable workspaces.
//! Once opened, every lookup is relative to a handle, never a checked path
//! that can be redirected between validation and use.

use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, DirBuilder};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Open an already approved canonical path without following any component.
pub(crate) fn open_canonical(path: &Path) -> io::Result<Dir> {
    if !path.is_absolute() {
        return Err(io::Error::other(
            "workspace path must be absolute and canonical",
        ));
    }
    let mut root = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
            Component::Normal(name) => names.push(name),
            _ => return Err(io::Error::other("workspace path must be canonical")),
        }
    }
    let mut directory = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    for name in names {
        directory = directory.open_dir_nofollow(name)?;
    }
    Ok(directory)
}

/// Check inherited primary roots before loading config, stopping a runtime or
/// opening session storage at a path that may no longer name the approved root.
pub fn validate_pinned_workspace(path: &Path) -> io::Result<()> {
    open_canonical(path).map(|_| ()).map_err(|error| {
        io::Error::other(format!(
            "workspace {} is no longer its approved directory: {error}",
            path.display()
        ))
    })
}

pub(crate) fn owned_child(parent: &Dir, name: &str) -> io::Result<Dir> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match parent.create_dir_with(name, &builder) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    parent.open_dir_nofollow(name)
}
