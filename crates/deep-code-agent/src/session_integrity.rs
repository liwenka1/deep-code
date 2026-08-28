//! Authorship tag for the write grants carried in a session record.
//!
//! A session record lives at `<workspace>/.deep-code/sessions/<id>.json` —
//! deliberately inside the primary writable root, so the model can write it
//! like any other file — and `-c` picks the newest by an `updated_at_ms` read
//! out of that same file. Its `extra_roots` then become the resumed session's
//! write boundary. That made the record a channel for granting a root with
//! nobody asked: write one naming `~/.cargo` (a `rustc-wrapper` there runs on
//! the next `cargo build`) or `~/Library/LaunchAgents`, give it a large
//! timestamp, and the next `-c` restores it silently.
//!
//! `refuse_as_unattended_root` is the floor for that channel, but a floor can
//! only enumerate what is *categorically* forbidden — the filesystem root, the
//! home directory, credential stores. `~/.cargo` is none of those; it is
//! merely code execution later. No list of dangerous directories is ever going
//! to be complete.
//!
//! So instead of judging the path, this authenticates the author. Grants are
//! tagged with an HMAC keyed by a secret the model cannot reach, and a record
//! whose tag does not verify has its grants dropped. Forging a record still
//! works — it just cannot carry authority any more.
//!
//! **Where the key is safe, and where it is not.** The key lives in
//! `~/.deep-code`, which the model-requested grant channel refuses to make
//! writable and which Seatbelt denies reading outright. Within the roots a
//! session is actually given, the file tools cannot reach it.
//!
//! Three ways it is reachable, each a consequence of a decision made
//! elsewhere:
//!
//! - A root the HUMAN grants that contains it — a session launched at `$HOME`,
//!   or `--add-dir ~`. `refuse_as_unattended_root` deliberately does not police
//!   those ("the distinction is authorship, not danger"), so `write_file` can
//!   overwrite the key with 32 bytes of its choosing, and the next process
//!   loads them. Worth separating from the credential-store case that decision
//!   was made for: handing over your own secrets is your call, but this key is
//!   the anchor the grant system trusts, and consenting to a wide boundary in
//!   one workspace does not imply consenting to unattended grants in another.
//! - A sandboxed shell on **Linux**: Landlock cannot express a read denial at
//!   all, so a command there can read the key, and reading it is enough to
//!   forge a tag. The same already-documented gap as "Linux credential reads".
//! - A shell on **Windows**, which has no filesystem confinement whatsoever
//!   (`sandbox::windows` reports `Enforcement::None`) — so read *and* write.
//!
//! What the tag buys in all three cases is that forging now requires reaching
//! the key, rather than merely writing the session file — which the model can
//! already write, as an ordinary file inside its own workspace.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// File holding the per-user signing key, under [`crate::paths::DEEP_CODE_DIR`].
const KEY_FILE: &str = "session-key";

/// HMAC-SHA256 output, hex — 32 bytes in, 64 chars out.
const KEY_BYTES: usize = 32;

/// Loaded once per process. `None` means no key could be read or created, in
/// which case nothing verifies and every recorded grant is dropped: losing a
/// grant only ever narrows the boundary, so that is the safe direction to
/// fail, and the caller says so out loud.
static KEY: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn key() -> Option<&'static [u8]> {
    KEY.get_or_init(load_or_create_key).as_deref()
}

fn load_or_create_key() -> Option<Vec<u8>> {
    let dir = crate::paths::home_dir()?.join(crate::paths::DEEP_CODE_DIR);
    let path = dir.join(KEY_FILE);
    if let Ok(existing) = std::fs::read(&path)
        && existing.len() == KEY_BYTES
    {
        return Some(existing);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let fresh = random_key()?;
    write_private(&path, &fresh).ok()?;
    // Re-read rather than trusting the write: two processes launching at once
    // both generate a key and the loser's bytes are the ones on disk. Whoever
    // reads last wins consistently, so both agree on one key.
    match std::fs::read(&path) {
        Ok(stored) if stored.len() == KEY_BYTES => Some(stored),
        _ => Some(fresh),
    }
}

fn random_key() -> Option<Vec<u8>> {
    use ring::rand::SecureRandom;
    let mut bytes = vec![0_u8; KEY_BYTES];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    // Unlink-then-`create_new`, matching `config::write`'s function of the
    // same name: `create(true).truncate(true)` follows a symlink at the final
    // component, so a planted link would have this write the HMAC key — the
    // trust anchor for recorded write grants — through to wherever it points.
    // `~/.deep-code` is sandbox-denied, so this is depth rather than a live
    // hole; two functions with one name and one purpose should not differ in
    // strength.
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    // A pre-existing key file created before the mode was applied (or by an
    // older build) gets tightened on the way past.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Length-prefixed so no two different grant lists can render to the same
/// bytes (`["/a/b"]` and `["/a", "/b"]` would otherwise collide under plain
/// concatenation). The session id and workspace are covered too: a tag must
/// not be liftable from one record onto another, nor replayed into a
/// different workspace than the one it was approved in.
fn message(id: &str, workspace: &Path, roots: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut push = |bytes: &[u8]| {
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    };
    push(id.as_bytes());
    push(workspace.as_os_str().as_encoded_bytes());
    push(&(roots.len() as u64).to_le_bytes());
    for root in roots {
        push(root.as_os_str().as_encoded_bytes());
    }
    out
}

/// Tag for this record's grant list, or `None` when there is no key (and
/// therefore nothing to claim).
#[must_use]
pub(crate) fn sign_roots(id: &str, workspace: &Path, roots: &[PathBuf]) -> Option<String> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key()?);
    let tag = ring::hmac::sign(&key, &message(id, workspace, roots));
    Some(to_hex(tag.as_ref()))
}

/// Whether `tag` really was produced by this host for exactly these grants.
///
/// An empty grant list needs no tag: there is nothing to authorize, so a
/// record that grants nothing verifies whether or not it carries one. That is
/// what lets records written before this field existed resume normally
/// instead of every one of them warning.
#[must_use]
pub(crate) fn verify_roots(
    id: &str,
    workspace: &Path,
    roots: &[PathBuf],
    tag: Option<&str>,
) -> bool {
    if roots.is_empty() {
        return true;
    }
    let (Some(key_bytes), Some(tag)) = (key(), tag) else {
        return false;
    };
    let Some(tag) = from_hex(tag) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key_bytes);
    // `ring::hmac::verify` is constant-time.
    ring::hmac::verify(&key, &message(id, workspace, roots), &tag).is_ok()
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
    }

    #[test]
    fn a_tag_verifies_only_for_exactly_what_was_signed() {
        let Some(tag) = sign_roots("sess-1", Path::new("/ws"), &roots()) else {
            eprintln!("no key on this host; skipping");
            return;
        };
        assert!(verify_roots(
            "sess-1",
            Path::new("/ws"),
            &roots(),
            Some(&tag)
        ));

        // Every bound field is really bound.
        assert!(
            !verify_roots("sess-2", Path::new("/ws"), &roots(), Some(&tag)),
            "a tag must not be liftable onto another session id"
        );
        assert!(
            !verify_roots("sess-1", Path::new("/other"), &roots(), Some(&tag)),
            "a tag must not replay into another workspace"
        );
        assert!(
            !verify_roots(
                "sess-1",
                Path::new("/ws"),
                &[PathBuf::from("/tmp/one"), PathBuf::from("/etc")],
                Some(&tag)
            ),
            "a tag must not cover a grant list it did not sign"
        );
        assert!(
            !verify_roots("sess-1", Path::new("/ws"), &roots(), None),
            "a grant list with no tag is unauthorized"
        );
        assert!(
            !verify_roots("sess-1", Path::new("/ws"), &roots(), Some("not-hex")),
            "a malformed tag is unauthorized"
        );
    }

    /// The length prefixes exist so that regrouping the same characters across
    /// entries cannot produce the same signed message.
    #[test]
    fn grant_lists_that_differ_only_in_grouping_sign_differently() {
        let joined = vec![PathBuf::from("/a/b")];
        let split = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_ne!(
            message("id", Path::new("/ws"), &joined),
            message("id", Path::new("/ws"), &split)
        );
    }

    /// Nothing granted, nothing to authorize — otherwise every record written
    /// before this field existed would warn on resume.
    #[test]
    fn an_empty_grant_list_needs_no_tag() {
        assert!(verify_roots("sess-1", Path::new("/ws"), &[], None));
    }
}
