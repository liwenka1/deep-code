//! Path normalization for LSP URI ↔ filesystem matching.

use std::path::{Path, PathBuf};

/// Canonicalize when possible so `/var` and `/private/var` compare equal on macOS.
#[must_use]
pub fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Compare two paths after canonicalization.
#[must_use]
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    normalize_path(left) == normalize_path(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_equal_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"x").unwrap();
        #[cfg(unix)]
        {
            let link = dir.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(paths_equal(&target, &link));
        }
    }

    #[test]
    fn normalize_path_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("foo.rs");
        std::fs::write(&file, b"").unwrap();
        let once = normalize_path(&file);
        let twice = normalize_path(&once);
        assert_eq!(once, twice);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_resolves_to_private_var() {
        if !Path::new("/var").exists() {
            return;
        }
        let normalized = normalize_path(Path::new("/var"));
        assert!(
            normalized.to_string_lossy().contains("private"),
            "expected /var to canonicalize through /private/var, got {}",
            normalized.display()
        );
        assert!(paths_equal(Path::new("/var"), Path::new("/private/var")));
    }
}
