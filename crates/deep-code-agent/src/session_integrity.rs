//! Authorship tag for the write grants carried in a session record.
//!
//! A session record lives at `<workspace>/.deep-code/sessions/<id>.json` —
//! deliberately inside the primary writable root, so the model can write it
//! like any other file — and `-c` ranks only host-authenticated records by their
//! signed `updated_at_ms`. Its `extra_roots` then become the resumed session's
//! write boundary. Before the full-record tag, a model-written file could name
//! `~/.cargo` (a `rustc-wrapper` there runs on the next `cargo build`) or
//! `~/Library/LaunchAgents`, give it a large timestamp, and the next `-c` would
//! restore it silently.
//!
//! `refuse_as_unattended_root` is the floor for that channel, but a floor can
//! only enumerate what is *categorically* forbidden — the filesystem root, the
//! home directory, credential stores. `~/.cargo` is none of those; it is
//! merely code execution later. No list of dangerous directories is ever going
//! to be complete.
//!
//! So instead of judging the path, this authenticates the author. Grants are
//! tagged with an HMAC keyed by a secret the model cannot reach, and a record
//! whose tag does not verify has its transcript and grants dropped on resume.
//! Forging a record therefore cannot carry either conversation state or
//! authority into the next provider turn.
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
//! What the tags buy in all three cases is that forging now requires reaching
//! the key, rather than merely writing the session file — which the model can
//! already write, as an ordinary file inside its own workspace.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// File holding the per-user signing key, under [`crate::paths::DEEP_CODE_DIR`].
const KEY_FILE: &str = "session-key";

/// HMAC-SHA256 output, hex — 32 bytes in, 64 chars out.
const KEY_BYTES: usize = 32;

/// Why the host could not produce a signing key.
///
/// This is user-visible: without a key nothing can be persisted, and the old
/// wording ("could not be authenticated for persistence") sent people looking
/// for a corrupt session file instead of an unwritable home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyUnavailable {
    NoHomeDirectory,
    NotCreatable,
    Unreadable,
}

impl std::fmt::Display for KeyUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHomeDirectory => f.write_str(
                "no home directory is set, so ~/.deep-code/session-key cannot be created",
            ),
            Self::NotCreatable => f.write_str(
                "~/.deep-code/session-key could not be created (is the home directory writable?)",
            ),
            Self::Unreadable => f.write_str(
                "~/.deep-code/session-key is unreadable or invalid; restore the original key to recover signed sessions",
            ),
        }
    }
}

/// Loaded once per process. An `Err` means no key could be read or created, in
/// which case nothing verifies and every recorded grant is dropped: losing a
/// grant only ever narrows the boundary, so that is the safe direction to
/// fail, and the caller says so out loud.
static KEY: OnceLock<Result<[u8; KEY_BYTES], KeyUnavailable>> = OnceLock::new();

// Per-test override. A test build starts with a fixed key so the suite never
// touches `$HOME` — a run that silently skipped when the home directory was
// read-only reported green for a channel it never exercised. A test can
// override it, including with `Err`, to drive the fail-closed path.
#[cfg(test)]
thread_local! {
    static KEY_OVERRIDE: std::cell::Cell<Option<Result<[u8; KEY_BYTES], KeyUnavailable>>> =
        const { std::cell::Cell::new(Some(Ok([0x5a; KEY_BYTES]))) };
}

fn key() -> Result<[u8; KEY_BYTES], KeyUnavailable> {
    #[cfg(test)]
    if let Some(overridden) = KEY_OVERRIDE.with(std::cell::Cell::get) {
        return overridden;
    }
    match KEY.get_or_init(load_or_create_key) {
        Ok(bytes) => Ok(*bytes),
        Err(reason) => Err(*reason),
    }
}

fn load_or_create_key() -> Result<[u8; KEY_BYTES], KeyUnavailable> {
    let dir = crate::paths::home_dir()
        .map(|home| home.join(crate::paths::DEEP_CODE_DIR))
        .ok_or(KeyUnavailable::NoHomeDirectory)?;
    load_or_create_key_in(&dir)
}

/// The filesystem implementation is also exercised directly in tests, without
/// changing HOME or going through the fixed signing key used by unit tests.
fn load_or_create_key_in(dir: &Path) -> Result<[u8; KEY_BYTES], KeyUnavailable> {
    let path = dir.join(KEY_FILE);
    match read_key(&path) {
        Ok(key) => return Ok(key),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        // Never rotate an existing key on an I/O error or corrupt contents:
        // that would invalidate every session signed with the original key.
        Err(_) => return Err(KeyUnavailable::Unreadable),
    }
    std::fs::create_dir_all(dir).map_err(|_| KeyUnavailable::NotCreatable)?;
    let fresh = random_key().ok_or(KeyUnavailable::NotCreatable)?;
    publish_key(&path, &fresh).map_err(|_| KeyUnavailable::NotCreatable)?;
    read_key(&path).map_err(|_| KeyUnavailable::Unreadable)
}

fn random_key() -> Option<[u8; KEY_BYTES]> {
    use ring::rand::SecureRandom;
    let mut bytes = [0_u8; KEY_BYTES];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes)
}

fn read_key(path: &Path) -> std::io::Result<[u8; KEY_BYTES]> {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("session key is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((KEY_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    bytes.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid session key length",
        )
    })
}

/// Publish a complete, private key without replacing an existing name. A
/// competing process either publishes first or reads the winner's complete
/// key; no reader can observe an empty/partial file and no loser can unlink it.
fn publish_key(path: &Path, bytes: &[u8; KEY_BYTES]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("key has no parent"))?;
    // NamedTempFile is created with mode 0600 on Unix, before any key bytes
    // are written. persist_noclobber uses a no-replace rename or hard link.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error),
    }
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

/// Whether this host can sign at all. Serialization paths check it first so a
/// missing key is reported as itself, not as a broken record.
pub(crate) fn ensure_key() -> Result<(), KeyUnavailable> {
    key().map(|_| ())
}

/// Canonical bytes for the full record tag.
///
/// The tag is cleared for the duration of the call and restored afterwards, so
/// signing does not deep-clone a transcript that can be megabytes, and serde's
/// deterministic struct-field order gives every writer the same representation
/// without trusting map iteration order.
///
/// Note what this signs: a *re-serialization of the parsed struct*, not the
/// bytes on disk. Adding a persisted field therefore changes the message for
/// every existing record. That is why adding one requires bumping
/// `SESSION_SCHEMA_VERSION`: the version check rejects old files explicitly,
/// instead of letting them fail verification and be discarded as if forged.
fn record_message(record: &mut crate::session_store::SessionRecord) -> Option<Vec<u8>> {
    let tag = record.record_mac.take();
    let message = serde_json::to_vec(&*record).ok();
    record.record_mac = tag;
    message
}

/// Tag for the complete persisted record, or `None` when the host key is not
/// available or the record cannot be serialized.
#[must_use]
pub(crate) fn sign_record(record: &mut crate::session_store::SessionRecord) -> Option<String> {
    let key_bytes = key().ok()?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes);
    let tag = ring::hmac::sign(&key, &record_message(record)?);
    Some(to_hex(tag.as_ref()))
}

/// Verify every persisted field, including the transcript and timestamps.
#[must_use]
pub(crate) fn verify_record(record: &mut crate::session_store::SessionRecord) -> bool {
    let (Ok(key_bytes), Some(tag), Some(message)) =
        (key(), record.record_mac.clone(), record_message(record))
    else {
        return false;
    };
    let Some(tag) = from_hex(&tag) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes);
    ring::hmac::verify(&key, &message, &tag).is_ok()
}

/// Tag for this record's grant list, or `None` when there is no key (and
/// therefore nothing to claim).
#[must_use]
pub(crate) fn sign_roots(id: &str, workspace: &Path, roots: &[PathBuf]) -> Option<String> {
    let key_bytes = key().ok()?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes);
    let tag = ring::hmac::sign(&key, &message(id, workspace, roots));
    Some(to_hex(tag.as_ref()))
}

/// Whether `tag` really was produced by this host for exactly these grants.
///
/// An empty grant list needs no tag: there is nothing to authorize, so a
/// record that grants nothing verifies whether or not it carries one.
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
    let (Ok(key_bytes), Some(tag)) = (key(), tag) else {
        return false;
    };
    let Some(tag) = from_hex(tag) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes);
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

    #[test]
    fn publishing_a_competing_key_keeps_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE);
        publish_key(&path, &[1; KEY_BYTES]).unwrap();
        publish_key(&path, &[2; KEY_BYTES]).unwrap();
        assert_eq!(read_key(&path).unwrap(), [1; KEY_BYTES]);
        assert_eq!(load_or_create_key_in(dir.path()).unwrap(), [1; KEY_BYTES]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn invalid_existing_keys_are_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE);
        for contents in [Vec::new(), vec![7; KEY_BYTES - 1], vec![7; KEY_BYTES + 1]] {
            std::fs::write(&path, &contents).unwrap();
            assert_eq!(
                load_or_create_key_in(dir.path()),
                Err(KeyUnavailable::Unreadable)
            );
            assert_eq!(std::fs::read(&path).unwrap(), contents);
        }
    }

    #[test]
    fn concurrent_processes_share_the_first_key() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        const CHILD: &str = "DEEP_CODE_KEY_INIT_TEST";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            let pid = std::process::id();
            std::fs::write(dir.join(format!("ready-{pid}")), []).unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while !dir.join("go").exists() {
                assert!(
                    Instant::now() < deadline,
                    "parent did not release key initialization"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            let key = load_or_create_key_in(&dir.join("keys")).unwrap();
            std::fs::write(dir.join(format!("result-{pid}")), key).unwrap();
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut children: Vec<_> = (0..8)
            .map(|_| {
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "session_integrity::tests::concurrent_processes_share_the_first_key",
                        "--nocapture",
                    ])
                    .env(CHILD, dir.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !children
            .iter()
            .all(|child| dir.path().join(format!("ready-{}", child.id())).exists())
        {
            if Instant::now() >= deadline {
                for child in &mut children {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                panic!("key initialization children did not start");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::fs::write(dir.path().join("go"), []).unwrap();
        while children
            .iter_mut()
            .any(|child| child.try_wait().unwrap().is_none())
        {
            if Instant::now() >= deadline {
                for child in &mut children {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                panic!("concurrent key initialization did not finish");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let key = read_key(&dir.path().join("keys").join(KEY_FILE)).unwrap();
        for child in children {
            let pid = child.id();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read(dir.path().join(format!("result-{pid}"))).unwrap(),
                key
            );
        }
    }

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
    }

    #[test]
    fn a_tag_verifies_only_for_exactly_what_was_signed() {
        // Test builds always have a key (see `key()`); a skip here used to hide
        // an unrunnable channel behind a green suite.
        let tag = sign_roots("sess-1", Path::new("/ws"), &roots()).expect("test key");
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

    /// Nothing granted, nothing to authorize.
    #[test]
    fn an_empty_grant_list_needs_no_tag() {
        assert!(verify_roots("sess-1", Path::new("/ws"), &[], None));
    }

    #[test]
    fn a_record_tag_covers_the_transcript_and_timestamps() {
        let mut record = crate::session_store::SessionRecord::new(PathBuf::from("/ws"), "system");
        let tag = sign_record(&mut record).expect("test key");
        record.record_mac = Some(tag);
        assert!(verify_record(&mut record));

        record.updated_at_ms = record.updated_at_ms.saturating_add(1);
        assert!(!verify_record(&mut record));
    }

    /// A host with no usable key must say so: nothing can be persisted, and the
    /// error has to name the key, not the session file.
    #[test]
    fn a_missing_key_is_reported_as_a_key_problem() {
        let previous = KEY_OVERRIDE.with(std::cell::Cell::get);
        KEY_OVERRIDE.with(|slot| slot.set(Some(Err(KeyUnavailable::NotCreatable))));
        assert_eq!(ensure_key(), Err(KeyUnavailable::NotCreatable));
        let mut record = crate::session_store::SessionRecord::new(PathBuf::from("/ws"), "system");
        assert_eq!(sign_record(&mut record), None);
        assert!(!verify_record(&mut record));
        KEY_OVERRIDE.with(|slot| slot.set(previous));
    }
}
