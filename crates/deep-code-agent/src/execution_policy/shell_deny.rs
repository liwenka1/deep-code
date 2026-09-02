//! Structured deny detection for shell commands.
//!
//! Splits a command line into segments on shell operators and inspects each
//! segment's program *basename* (quotes stripped) and flag semantics, so
//! `/bin/rm -rf /`, `rm  -rf /`, and `cd /tmp && rm -rf /` all resolve to the
//! same denied shape a bare prefix match would miss.
//!
//! Deny rules deliberately ignore identity matching: for a deny rule the
//! flags are the danger (`rm -rf`), whereas identity extraction skips flags.
//! Trusted (allow) matching lives separately in [`super::command_shape`].
//!
//! Scope, honestly: this is a best-effort UX floor over PLAIN command forms,
//! not a security boundary, and it deliberately does not chase indirection
//! (wrappers, `sh -c` scripts, substitutions). It doesn't have to: any
//! command containing indirection is structurally excluded from every
//! automatic pass ([`has_shell_indirection`]) and wrapped/interpreter forms
//! are never trusted, so those always land on a human first. What parsing
//! misses is contained by the human at the prompt — or, under `Yolo`, by the
//! OS sandbox (plus the per-turn checkpoint for the writable workspace).

use serde::{Deserialize, Serialize};

use super::shell_lex::{basename_lower, clean_token, has_shell_indirection, segments};
use crate::i18n::TextId;

/// Why a command segment was denied. The string is surfaced to the user and
/// logged as the matched rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenyReason(pub &'static str);

/// Whether a (cleaned) token is a leading `VAR=value` environment assignment,
/// which we skip when locating the program word.
fn is_env_assignment(token: &str) -> bool {
    token.contains('=') && !token.starts_with('-') && !token.contains('/')
}

/// The program name of a segment, reduced to its lowercased basename so that
/// `/usr/bin/sudo` and `sudo` compare equal. Returns `None` for an empty
/// segment or a leading assignment like `FOO=bar cmd` (we skip env-prefixes).
fn program_of(segment: &str) -> Option<String> {
    for token in segment.split_whitespace() {
        // Skip leading `VAR=value` environment assignments.
        if is_env_assignment(&clean_token(token)) {
            continue;
        }
        return Some(basename_lower(token));
    }
    None
}

/// Positional/flag tokens after the program word, each with shell quoting
/// removed (see [`clean_token`]) so a quoted flag or path is inspected exactly
/// as the shell would run it.
fn args_of(segment: &str) -> Vec<String> {
    let mut seen_program = false;
    let mut out = Vec::new();
    for token in segment.split_whitespace() {
        let cleaned = clean_token(token);
        if !seen_program {
            if is_env_assignment(&cleaned) {
                continue; // env prefix
            }
            seen_program = true;
            continue; // the program word itself
        }
        out.push(cleaned);
    }
    out
}

/// True if the short-flag bundles or long flags in `args` contain a flag whose
/// short form is `short` (e.g. `'r'`) or whose long form is in `longs`.
fn has_flag(args: &[String], short: char, longs: &[&str]) -> bool {
    args.iter().any(|arg| {
        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            longs.contains(&name)
        } else if let Some(bundle) = arg.strip_prefix('-') {
            bundle.chars().any(|c| c == short)
        } else {
            false
        }
    })
}

/// Whether a DOS-style `/x` switch is present, case-insensitively.
///
/// `cmd.exe` switches are `/s`, `/q`, `/f` — invisible to [`has_flag`], which
/// only understands `-x` / `--long`. They are also case-insensitive (`/S` ==
/// `/s`), and may be bundled with a value (`/f:x`).
fn has_dos_switch(args: &[String], switch: char) -> bool {
    args.iter().any(|arg| {
        arg.strip_prefix('/').is_some_and(|rest| {
            // cmd.exe accepts bundled switches, and `del /f/s/q <path>` is the
            // idiomatic spelling in Windows cleanup batch files — i.e. the form
            // a model is most likely to emit verbatim. Checking only the whole
            // remainder meant `/s/q` matched neither `s` nor `q`, so the
            // recurse+force guard simply never fired on that spelling.
            rest.split('/').any(|piece| {
                piece
                    .split(':')
                    .next()
                    .unwrap_or(piece)
                    .eq_ignore_ascii_case(&switch.to_string())
            })
        })
    })
}

/// Windows system trees a recursive delete must never touch. `\Users` is
/// deliberately absent: real projects live under it, so denying it would refuse
/// `rd /s /q C:\Users\me\proj\node_modules` — an everyday cleanup. Destroying
/// someone else's home tree is the out-of-workspace problem, which the approval
/// gate owns, not this floor.
const WINDOWS_SYSTEM_ROOTS: &[&str] = &[
    "\\windows",
    "\\system32",
    "\\program files",
    // 8.3 short-name aliases resolve to the same trees, so matching only the
    // long spelling left `rd /s /q C:\Progra~1` open.
    "\\progra~1",
    "\\progra~2",
    "\\programdata",
];

/// Whether a token names a whole drive (`c:`, `D:\`) — the argument shape that
/// separates `format C:` from a repo-local script called `format`.
fn is_drive_spec(token: &str) -> bool {
    let token = token.trim().trim_end_matches(['\\', '/']);
    let bytes = token.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// The other spellings `format` accepts for a raw volume: a volume GUID path
/// (`\\?\Volume{...}`), a device-namespace path (`\\.\C:`), or the same bare
/// drive behind a `\\?\` extended-length prefix (`\\?\C:`). Unlike a mounted-
/// folder target these cannot collide with a repo-relative script argument, so
/// refusing them costs nothing. (A mount-point target — `format C:\mnt\data` —
/// is indistinguishable from an ordinary path argument and stays with the
/// approval gate; so does an extended-length path that carries a real sub-path,
/// `\\?\C:\dir`, which is a file argument rather than a raw volume.)
fn is_volume_or_device_path(token: &str) -> bool {
    let lower = token.trim().to_ascii_lowercase();
    if lower.starts_with("\\\\?\\volume{") || lower.starts_with("\\\\.\\") {
        return true;
    }
    // `\\?\` over a *bare* drive is the extended-length spelling of `\\.\C:`;
    // over a sub-path it is just a long file path, which `is_drive_spec` rejects.
    lower.strip_prefix("\\\\?\\").is_some_and(is_drive_spec)
}

/// Whether a recursive-force delete (`rd /s /q`, `del /s /q`) names a target
/// catastrophic enough to hard-refuse.
///
/// A blanket deny on the recurse+force shape — the exact mirror of `rm -rf` —
/// looks symmetric but is not: on Unix `rm -r <dir>` stays available as the
/// everyday escape, whereas `rd /s` without `/q` stops to ask for confirmation
/// and stdin is `Stdio::null()`, so it can never complete. Denying the whole
/// shape therefore leaves Windows with *no* working way to delete a directory
/// tree. So this floor refuses only the shapes that are unambiguously
/// destructive and keeps `rd /s /q node_modules` runnable.
///
/// Not covered here: an absolute path to somewhere else in the user's home. That
/// is the out-of-workspace question the approval gate answers.
fn dos_delete_target_is_catastrophic(args: &[String]) -> bool {
    args.iter()
        // Switches are not targets (`/s`, `/q`, `/f:x`).
        .filter(|arg| !arg.starts_with('/'))
        .any(|target| {
            let target = target.trim();
            if target.is_empty() {
                return false;
            }
            // `%VAR%` cannot be resolved statically, so the target is unknown —
            // same reasoning as `$` on the Unix side. A lone `%` is just a
            // character in a filename (`report%20final.log`); a variable
            // reference needs the closing one too.
            if target.matches('%').count() >= 2 {
                return true;
            }
            let normalized = target.to_ascii_lowercase().replace('/', "\\");
            // The workspace itself, or anything climbing out of it — matched by
            // path component, so `my..dir` and `v1..2` stay ordinary names while
            // a genuine `..` component is still refused.
            if normalized
                .split('\\')
                .any(|part| part == "." || part == "..")
            {
                return true;
            }
            // Root of the current drive.
            if normalized == "\\" {
                return true;
            }
            // Drive root: `c:`, `c:\`.
            let bytes = normalized.as_bytes();
            let after_drive =
                if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                    &normalized[2..]
                } else {
                    normalized.as_str()
                };
            if after_drive.is_empty() || after_drive == "\\" {
                return true;
            }
            // Win32 strips trailing dots and spaces, so `C:\Windows.` resolves to
            // `C:\Windows`; compare on the resolved spelling.
            let trimmed = after_drive
                .trim_end_matches('\\')
                .trim_end_matches(['.', ' ']);
            // `del /f /s /q C:\*` — the canonical wipe-the-drive string, and it
            // was not refused. The system-root branch below handles a wildcard
            // *under* a system tree (`C:\Windows\*` prefix-matches), but a
            // wildcard sitting directly at the drive root left it nothing to
            // match on. A target made only of wildcard characters names the
            // whole of whatever it is rooted at.
            if trimmed
                .trim_start_matches('\\')
                .chars()
                .all(|ch| matches!(ch, '*' | '?' | '.'))
            {
                return true;
            }
            WINDOWS_SYSTEM_ROOTS
                .iter()
                .any(|root| trimmed == *root || trimmed.starts_with(&format!("{root}\\")))
        })
}

/// Whether a `chmod` mode argument grants write to "other"/"all" (world-
/// writable), covering octal (`777`, `0666`, `4777`) and symbolic (`o+w`,
/// `a+w`, `+w`, `a=rwx`) forms. Best-effort — chmod modes have many shapes; it
/// errs toward flagging.
fn chmod_world_writable(arg: &str) -> bool {
    if arg.starts_with('-') {
        return false; // a flag like `-R`, not a mode
    }
    // Octal: the last digit is the "other" triad; its 2-bit is world write.
    if !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()) {
        return arg
            .bytes()
            .last()
            .is_some_and(|last| (last - b'0') & 0o2 != 0);
    }
    // Symbolic `[ugoa…][+-=][perms][,…]`: world-writable if a clause grants `w`
    // to a scope that includes `o`/`a`, or names no scope (which means all).
    arg.split(',').any(|clause| {
        let Some(op) = clause.find(['+', '=']) else {
            return false;
        };
        let (scope, perms) = clause.split_at(op);
        let world_scope = scope.is_empty() || scope.contains('a') || scope.contains('o');
        world_scope && perms[1..].contains('w')
    })
}

/// Inspect one already-split segment. Returns the deny reason if the segment is
/// a PLAIN known-dangerous command. Program matching is basename-based with
/// quotes stripped; it deliberately does NOT chase wrappers (`env rm …`),
/// inline interpreters (`sh -c '…'`), or substitutions — those indirect forms
/// can never be auto-approved (see [`has_shell_indirection`] and the untrusted
/// default), so they always reach a human, and under `Yolo` the OS sandbox is
/// the containment. This floor exists to stop the common destructive shapes a
/// model emits verbatim, not to win an obfuscation arms race.
fn deny_segment(segment: &str) -> Option<DenyReason> {
    // Fork bomb: whitespace-insensitive signature match.
    let squished: String = segment.chars().filter(|c| !c.is_whitespace()).collect();
    if squished.contains(":():{") || squished.contains(":(){") || squished.contains(":|:&") {
        return Some(DenyReason("fork bomb pattern"));
    }

    let program = program_of(segment)?;
    let args = args_of(segment);

    match program.as_str() {
        "sudo" | "su" | "doas" => Some(DenyReason("privilege escalation")),
        "rm" => {
            let recursive =
                has_flag(&args, 'r', &["recursive"]) || has_flag(&args, 'R', &["recursive"]);
            let force = has_flag(&args, 'f', &["force"]);
            (recursive && force).then_some(DenyReason("recursive force remove (rm -rf)"))
        }
        "dd" => args
            .iter()
            .any(|arg| {
                arg.strip_prefix("of=")
                    .is_some_and(|target| target.starts_with("/dev/"))
            })
            .then_some(DenyReason("dd write to device (of=/dev/…)")),
        "mkfs" | "fdisk" | "parted" => Some(DenyReason("disk formatting/partitioning")),
        _ if program.starts_with("mkfs.") => Some(DenyReason("disk formatting")),
        "chmod" => args
            .iter()
            .any(|arg| chmod_world_writable(arg))
            .then_some(DenyReason("world-writable chmod (777)")),

        // Windows equivalents. This floor was POSIX-only, so on Windows — where
        // the Job Object sandbox confines nothing — there was no floor at all.
        // Each rule mirrors its Unix counterpart's shape rather than banning the
        // program: `del`/`rd` are denied only in the recurse+force form, the way
        // `rm` is denied only as `rm -rf`. Harmless on Unix, where these
        // programs either do not exist or have no `/s` switch.
        // Recursive delete is refused by TARGET, not by shape — see
        // `dos_delete_target_is_catastrophic` for why mirroring `rm -rf`
        // literally would leave Windows unable to delete anything.
        // (`rmdir` is the same command as `rd`; on Unix it only removes empty
        // dirs and has no `/s`, so the rule cannot misfire there.)
        "del" | "erase" | "rd" | "rmdir" => (has_dos_switch(&args, 's')
            && dos_delete_target_is_catastrophic(&args))
        .then_some(DenyReason("recursive delete of a root or system path")),
        // `diskpart` has no benign form. `format` does collide with a repo-local
        // formatter (`./format`, `scripts/format`, a `format` bin on PATH), which
        // this floor cannot be overridden to allow — so require the shape of a
        // real disk format: a drive spec (`format C:`, `format /fs:ntfs D:`),
        // a volume GUID path, or a device path.
        "diskpart" => Some(DenyReason("disk formatting/partitioning")),
        "format" => args
            .iter()
            .any(|arg| is_drive_spec(arg) || is_volume_or_device_path(arg))
            .then_some(DenyReason("disk formatting/partitioning")),
        // Registry deletion: `reg delete <key> /f`. `reg query`/`reg add` stay.
        "reg" => args
            .first()
            .is_some_and(|sub| sub.eq_ignore_ascii_case("delete"))
            .then_some(DenyReason("registry deletion (reg delete)")),
        // Ownership/ACL takeover of a tree — the standard prelude to wiping
        // files a normal user could not touch, and never needed inside a
        // workspace.
        "takeown" => Some(DenyReason("ownership takeover (takeown)")),
        _ => None,
    }
}

/// Detect a network-fetch piped into a shell interpreter, e.g.
/// `curl https://x | sh` or `wget -O- url | bash`. Segment splitting alone
/// loses the pipe relationship, so this inspects the producer/consumer pair.
/// Plain program names only (see [`deny_segment`] for why wrapped forms are
/// out of scope); the interpreter set includes scripting languages that can
/// `eval` piped stdin.
fn deny_pipe_to_shell(command: &str) -> Option<DenyReason> {
    if !command.contains('|') {
        return None;
    }
    let parts: Vec<&str> = command.split('|').map(str::trim).collect();
    let fetches = |seg: &str| matches!(program_of(seg).as_deref(), Some("curl" | "wget" | "fetch"));
    let is_shell = |seg: &str| {
        matches!(
            program_of(seg).as_deref(),
            Some(
                "sh" | "bash"
                    | "zsh"
                    | "dash"
                    | "ksh"
                    | "perl"
                    | "python"
                    | "python3"
                    | "ruby"
                    | "node"
                    | "php"
                    // Windows interpreters were missing, so `curl x | powershell`
                    // — the standard Windows one-line installer shape — was not
                    // denied on ANY platform.
                    | "powershell"
                    | "pwsh"
                    | "cmd"
            )
        )
    };
    let has_fetch = parts.iter().any(|seg| fetches(seg));
    let feeds_shell = parts.iter().skip(1).any(|seg| is_shell(seg));
    (has_fetch && feeds_shell).then_some(DenyReason("network fetch piped to shell"))
}

/// Evaluate a full command line against the built-in deny rules. Returns the
/// first matching reason, or `None` if nothing is denied. A command is denied
/// if ANY of its segments is dangerous. Plain forms only: indirect forms
/// (wrappers, `sh -c`, substitutions) are structurally excluded from every
/// automatic pass, so they land on a human instead of on this floor.
#[must_use]
pub fn builtin_deny(command: &str) -> Option<DenyReason> {
    if let Some(reason) = deny_pipe_to_shell(command) {
        return Some(reason);
    }
    segments(command).into_iter().find_map(deny_segment)
}

/// Static, no-execution safety notes surfaced at the approval prompt: why a
/// command warrants review and how to make it safer. This does NOT dry-run or
/// diff the command — shell side effects are impractical to preview — it
/// classifies by program/flag/path shape, reusing the same segment split as
/// the deny checks so notes and denials always agree on what a segment is.
/// One advisory note as language-neutral keys: why a command warrants review
/// (`reason`) and how to make it safer (`suggestion`). The TUI renders both in
/// the user's language — presentation stays out of the policy layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyNote {
    pub reason: TextId,
    pub suggestion: TextId,
}

/// Internal builder that dedups notes as they are recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SafetyNotes {
    notes: Vec<SafetyNote>,
}

impl SafetyNotes {
    /// Record a note once — a repeated reason (e.g. two network calls in one
    /// line) collapses to a single entry.
    fn note(&mut self, reason: TextId, suggestion: TextId) {
        if !self.notes.iter().any(|existing| existing.reason == reason) {
            self.notes.push(SafetyNote { reason, suggestion });
        }
    }
}

/// Advisory static analysis of a shell command for the approval prompt. Only
/// meaningful for commands that already need approval (denied commands never
/// reach here). Returns empty notes for a plain, low-signal command.
#[must_use]
pub fn safety_notes(command: &str) -> Vec<SafetyNote> {
    let mut notes = SafetyNotes::default();
    if command.contains('>') {
        notes.note(
            TextId::SafetyRedirectReason,
            TextId::SafetyRedirectSuggestion,
        );
    }
    for segment in segments(command) {
        let Some(program) = program_of(segment) else {
            continue;
        };
        let args = args_of(segment);
        // Positional (non-flag) tokens, for subcommand and path inspection.
        let positional: Vec<String> = args
            .iter()
            .filter(|token| !token.starts_with('-'))
            .map(|token| token.to_ascii_lowercase())
            .collect();

        if positional
            .iter()
            .any(|arg| arg.starts_with('/') || arg.starts_with('~') || arg.contains(".."))
        {
            notes.note(
                TextId::SafetyPathOutsideReason,
                TextId::SafetyPathOutsideSuggestion,
            );
        }

        let subcommand = positional.first().map(String::as_str);
        match program.as_str() {
            "curl" | "wget" | "nc" | "ncat" | "ssh" | "scp" | "rsync" | "ftp" | "telnet" => {
                notes.note(TextId::SafetyNetworkReason, TextId::SafetyNetworkSuggestion);
            }
            "rm" | "rmdir" | "unlink" | "shred" | "trash" => {
                notes.note(TextId::SafetyDeleteReason, TextId::SafetyDeleteSuggestion);
            }
            "chmod" | "chown" => {
                notes.note(TextId::SafetyChmodReason, TextId::SafetyChmodSuggestion);
            }
            "git"
                if matches!(
                    subcommand,
                    Some("push" | "pull" | "fetch" | "clone" | "remote")
                ) =>
            {
                notes.note(
                    TextId::SafetyGitRemoteReason,
                    TextId::SafetyGitRemoteSuggestion,
                );
            }
            "npm" | "pnpm" | "yarn" | "pip" | "pip3" | "gem" | "go" | "cargo"
                if matches!(subcommand, Some("install" | "ci" | "add" | "get")) =>
            {
                notes.note(TextId::SafetyInstallReason, TextId::SafetyInstallSuggestion);
            }
            _ => {}
        }
    }
    notes.notes
}

/// cc-style `acceptEdits` allowlist for shell/job commands: a bounded
/// filesystem-mutation command. Every segment's program must be in the set and
/// `rm` must not recurse. It does NOT check whether a path stays inside the
/// workspace — the OS sandbox enforces that boundary at execution (a write
/// outside the workspace is denied there), so an out-of-workspace edge slips to
/// a sandboxed failure rather than needing per-token path parsing here. A hard
/// deny (e.g. `rm -rf`) never reaches here — `builtin_deny` short-circuits it.
#[must_use]
pub fn is_workspace_fs_edit(command: &str) -> bool {
    // `sed` is deliberately absent: its `e`/`w` script flags run commands and
    // write arbitrary paths from inside the script argument, so it is never a
    // bounded edit. In-workspace text edits go through the write tools.
    const FS_EDIT: &[&str] = &["mkdir", "touch", "mv", "cp", "rm", "rmdir"];
    // Redirection/substitution/expansion can run programs, write paths, or
    // name targets this per-segment program check never inspects, so such a
    // command is never a bounded edit.
    if has_shell_indirection(command) {
        return false;
    }
    let segs = segments(command);
    if segs.is_empty() {
        return false;
    }
    segs.iter().all(|segment| {
        let Some(program) = program_of(segment) else {
            return false;
        };
        if !FS_EDIT.contains(&program.as_str()) {
            return false;
        }
        let args = args_of(segment);
        // A recursive `rm` deletes a whole subtree — not a bounded edit, and the
        // one destruction the sandbox can't undo (the workspace itself is
        // writable). `rm <file>` and `rmdir` (empty dirs) stay auto-approvable.
        !(program == "rm"
            && (has_flag(&args, 'r', &["recursive"]) || has_flag(&args, 'R', &["recursive"])))
    })
}

#[cfg(test)]
mod tests;
