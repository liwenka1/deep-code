//! Shell lexical normalization shared by the trust matcher and the deny floor.
//!
//! Both [`super::command_shape`] (the allow side — "is this command trusted?")
//! and [`super::shell_deny`] (the deny side — "is this command catastrophic?")
//! have to agree on ONE thing before they can disagree on anything else: *what
//! will the shell actually execute*. That agreement is these functions —
//! segment splitting, quote/backslash stripping, basename extraction, and the
//! indirection test. They live here, in a module neither side owns, so the
//! allow matcher no longer reaches into the deny module for `clean_token` (which
//! read backwards) and the shared "one view of the shell" invariant has a home.
//!
//! None of this is a full shell parser — it is a deliberate safety
//! over-approximation. Stripping quoting can only ever *expose* a dangerous
//! flag or path, never hide one; an exotic construct falls through to "needs
//! approval" rather than being auto-trusted.

/// Split a command line into individually-checkable segments on the shell
/// control operators `;`, `&&`, `||`, `|`, and newlines. Each segment is a
/// single simple command whose program/args we can inspect.
///
/// This is a pragmatic tokenizer, not a full shell parser: it does not track
/// quotes or subshells. That is a deliberate safety bias — an unparseable or
/// exotic construct falls through to "needs approval" rather than being
/// auto-trusted, and deny checks still run on every whitespace-split segment.
pub(super) fn segments(command: &str) -> Vec<&str> {
    command
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Words the shell consumes ahead of a segment's program word: control-flow
/// reserved words (`if true; then rm -rf /; fi` lexes to a segment starting
/// with `then`), the `!` negation, `time`, and the transparent wrappers whose
/// whole job is to run the rest of the line unchanged (`exec rm -rf /`,
/// `env X=1 rm -rf /`, `echo / | xargs rm -rf`).
///
/// The two sides read this list in opposite directions, which is why it lives
/// here and not in either of them. The deny floor reads *past* these words to
/// find the program they hand off to. The identity side treats a line that
/// *opens* with one as naming no operation of its own — `time <anything>` —
/// so it never collapses to the bare word (see
/// [`super::command_shape::identity`]). Wrappers that take their own options
/// first (`nice -n 5 …`, `timeout 5 …`, `env -i …`) are not parsed on the deny
/// side: their option becomes the "program" and matches no rule — the
/// wrapper-options arms race the floor deliberately stays out of (such a line
/// is never trusted and never a bounded edit, so it still lands on a human).
/// Listing such a wrapper is still worth it for the identity side, which only
/// needs to know the line names no operation of its own.
///
/// Membership is not a memory exercise: `every_word_the_shell_runs_the_tail_for_is_known`
/// asks the real shell which candidates hand off execution and fails naming
/// the ones missing from here. That test is what found `caffeinate` and
/// `xcrun`; the entries below them are their Linux counterparts, added
/// pre-emptively because this list is one-directional — an entry the local
/// shell does not treat as a wrapper costs nothing but a literal identity.
pub(super) const PREFIX_WORDS: &[&str] = &[
    "if",
    "then",
    "else",
    "elif",
    "while",
    "until",
    "do",
    "for",
    "case",
    "!",
    "time",
    "exec",
    "command",
    "builtin",
    "env",
    "nohup",
    "nice",
    "busybox",
    "xargs",
    "caffeinate",
    "xcrun",
    "setsid",
    "stdbuf",
    "timeout",
    "ionice",
    "taskset",
    "unshare",
    "chroot",
    "flock",
    "runuser",
    "arch",
    "watch",
    "script",
    "proxychains",
    "parallel",
];

/// Interpreters that execute whatever text they are handed — a script path, a
/// `-c` string, or piped stdin. The deny floor uses the list to refuse a
/// network fetch piped into one; the identity side uses it the way it uses
/// [`PREFIX_WORDS`]: `sh -c '<anything>'` and `python <any script>` name no
/// operation of their own, so a consent on one must not cover another.
///
/// Membership rule, so the next addition is not a judgement call: a runtime
/// that executes text supplied at the call site. That is why the shell's own
/// `eval`, `source` and `.` belong here — they were missing, so one session
/// "a" on `source .venv/bin/activate` covered `source ./anything.sh` for the
/// rest of the session, and in AcceptEdits the model writes that script with
/// no prompt either. `awk`/`osascript` are here for the same reason a level
/// down: their program text can `system()` / `do shell script` out.
pub(super) const INTERPRETERS: &[&str] = &[
    // The shell's own text-executing builtins.
    "eval",
    "source",
    ".",
    "awk",
    "gawk",
    "osascript",
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "perl",
    "python",
    "python3",
    "ruby",
    "node",
    "php",
    // Windows interpreters were missing, so `curl x | powershell` — the
    // standard Windows one-line installer shape — was not denied on ANY
    // platform.
    "powershell",
    "pwsh",
    "cmd",
];

/// Whether a program word (lowercased basename) runs whatever the rest of the
/// line names instead of an operation of its own: a [`PREFIX_WORDS`] wrapper
/// or an [`INTERPRETERS`] entry.
pub(super) fn runs_the_rest_of_the_line(program: &str) -> bool {
    PREFIX_WORDS.contains(&program) || INTERPRETERS.contains(&program)
}

/// Remove shell quoting from a single token so a deny/bounds check inspects
/// what `sh -c` will actually execute — not the raw, still-quoted text. Strips
/// every `'` and `"` (the shell removes quotes anywhere in a word, so `r""m`
/// runs as `rm` and `'-rf'` as `-rf`) and, on Unix, every `\` (a backslash
/// escapes the next char, so `\-rf` runs as `-rf` and `\/tmp` as `/tmp`). On
/// Windows `\` is a genuine path separator and is kept.
///
/// This is a deliberate safety over-approximation: dropping these characters
/// can only *expose* a dangerous flag or path, never hide one, so it can never
/// weaken a deny rule or a workspace-bounds check. It MUST be applied to every
/// token a decision depends on — the program word AND its arguments — because a
/// check that cleaned only the program word (as an earlier version did) let
/// quoted flags like `rm '-rf' /` and quoted paths like `cp x '/tmp/out'` slip
/// straight past.
///
/// Shared by both sides: the trust matcher must strip the same quoting the deny
/// floor does, or a quoted redirecting flag (`--con"fig"`) that the shell runs
/// as `--config` rides a trusted identity the deny floor would have cleaned.
pub(super) fn clean_token(token: &str) -> String {
    let strip: &[char] = if cfg!(windows) {
        // `^` is cmd.exe's escape character — the exact Windows counterpart of
        // the `\` handled below. Without stripping it, one caret walked past
        // every rule on this floor (`r^d /s /q C:\Windows`, `de^l /f/s/q C:\*`,
        // `curl x | powershe^ll`) while `cmd /C` ran the real thing — and
        // Windows is the one platform with no sandbox behind this floor.
        &['\'', '"', '^']
    } else {
        &['\'', '"', '\\']
    };
    token.chars().filter(|ch| !strip.contains(ch)).collect()
}

/// The lowercased basename of a token, with shell quoting removed first, so
/// `/usr/bin/sudo`, `'sudo'`, and `s\udo` all resolve to `sudo`. On Windows `\`
/// is a path separator; on Unix it was already dropped by [`clean_token`].
pub(super) fn basename_lower(token: &str) -> String {
    let cleaned = clean_token(token);
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    let base = cleaned
        .rsplit(separators)
        .next()
        .unwrap_or(cleaned.as_str())
        .to_ascii_lowercase();
    strip_executable_extension(&base)
}

/// Drop a Windows executable suffix so `powershell.exe` and `powershell`, or
/// `reg.exe` and `reg`, resolve to the same program word.
///
/// Unconditional rather than `cfg!(windows)` for the same reason the Windows
/// verb rules are: it keeps the floor testable from any host, and on Unix a
/// program genuinely named `rm.exe` is both vanishingly rare and safe to
/// over-approximate — this floor may only ever *expose* a dangerous name, never
/// hide one. Without it, `reg.exe delete`, `takeown.exe`, `diskpart.exe`,
/// `format.com` and `curl x | powershell.exe` all fell through to `_ => None`,
/// which is exactly how Windows documentation and scripts spell them.
fn strip_executable_extension(base: &str) -> String {
    const EXECUTABLE_SUFFIXES: &[&str] = &[".exe", ".com", ".bat", ".cmd"];
    for suffix in EXECUTABLE_SUFFIXES {
        if let Some(stem) = base.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return stem.to_string();
        }
    }
    base.to_string()
}

/// True if a command contains shell redirection, substitution, expansion or
/// grouping (`>`, `<`, `` ` ``, `$`, `{`/`}`, `(`/`)`, `*`, `?`, `[`/`]`). These
/// make the visible text an unreliable
/// description of what will run: a substitution executes an arbitrary inner
/// program (`touch $(curl …)`), a redirection writes a path no program word
/// mentions (`sed … > cfg`), and a `$VAR` expands to content the reviewer
/// never saw. Any such command is excluded from every automatic pass (trust
/// list, accept-edits) and goes to a human — which is what lets the deny floor
/// stay plain-form only instead of chasing obfuscations.
///
/// Brace expansion belongs on this list for exactly the same reason and was
/// the one word-expansion stage missing from it. Every check above this line
/// compares the *written* token against a rule — the redirecting-flag list,
/// the cwd-bounds spelling test, the program basename — and bash rewrites the
/// token before any of them describes what runs. `--con{fig,fig}` reaches
/// cargo as `--config`, so the default-trusted `cargo build` executed an
/// arbitrary program through `build.rustc-wrapper` with no prompt at any tier;
/// `git diff --no-index{,}` printed any file on the host into the transcript
/// the same way; `cp {~/.ssh/id_rsa,./k}` rode the accept-edits allowance,
/// whose operands "stay under the cwd by spelling" only while the spelling is
/// what the shell reads. Treating the punctuation as indirection is the same
/// over-approximation the rest of this list makes: a brace command is never
/// auto-trusted and never a bounded edit, so it lands on a human, and no
/// expander has to be right for the gate to be safe. (The deny floor does
/// expand them — see `shell_deny::builtin_deny` — because under `Yolo` it is
/// the only thing above the sandbox.)
///
/// Both halves of a pair are listed because bash leaves an unbalanced brace
/// alone (`a{b` stays `a{b`): matching either character over-approximates in
/// the safe direction rather than requiring this to parse what bash will
/// expand.
///
/// Subshell parens are here rather than in one caller. `session_identity` used
/// to spell its own `contains(['(', ')'])` beside this call, which left the
/// module's stated "one view of the shell" invariant with a seam right at the
/// place a new construct has to be added — and a construct added to only one
/// of two lists is precisely how the brace hole stayed open. The other callers
/// already refused these commands for unrelated reasons (a peeled `(` is a
/// prefix word, and `(cargo` matches no rule), so folding it in changes no
/// verdict; it changes where the next construct has to be written.
///
/// Pathname expansion (`*`, `?`, `[…]`) is here for the same reason as braces,
/// and was the stage still missing after them: the pattern reaches the program
/// as whatever file names match it in the cwd, so `cargo build --con*` reached
/// cargo as `--config=build.rustc-wrapper=x` once a file of that name existed
/// (and the trusted `cargo test -- --logfile ./<name>` creates a file of any
/// name with no prompt), while `git diff --no-inde? …` reopened the `--no-index`
/// read the flag list had closed. The cost is that `git diff -- src/*.rs` asks.
/// Which characters belong here is no longer the author's call:
/// `every_punctuation_the_shell_rewrites_is_accounted_for` asks the real shell
/// which characters rewrite a word and fails naming any that no rule reads.
#[must_use]
pub(super) fn has_shell_indirection(command: &str) -> bool {
    command.contains(['>', '<', '`', '$', '{', '}', '(', ')', '*', '?', '[', ']'])
}

/// Whether a (cleaned) token names a path that leaves the current directory by
/// its spelling alone: absolute (`/etc/x`, `\x`), home-relative (`~/.ssh`,
/// `~user`), drive-lettered (`C:\x`) or climbing through a `..` path component
/// (`../x`, `a/../b`, `..`). `..` counts only as a whole component: `main..HEAD`
/// is a revision range and `my..dir` a file name, and reading the two dots
/// anywhere in the word turned `git diff main..HEAD` into a prompt.
///
/// The sandbox bounds the *write* side of such a path; the *read* side it
/// leaves open (`(allow file-read*)`), so this spelling is the read fence — for
/// the accept-edits allowance (`cp ~/.ssh/id_rsa ./k`), for the trust gate and
/// the session key (`git diff /dev/null ~/.ssh/id_rsa` on the default-trusted
/// `git diff`), and for the safety notes that warn the human. One predicate, so
/// the three never disagree about what "outside" looks like.
pub(super) fn escapes_cwd_by_spelling(token: &str) -> bool {
    let bytes = token.as_bytes();
    token.starts_with(['/', '~', '\\'])
        || token.split(['/', '\\']).any(|component| component == "..")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Whether an argument token (quoting already stripped) names a path outside
/// the cwd: a positional operand judged as is, a `--flag=value` judged by its
/// value (`--target-directory=/tmp` is a target like any other), a bare flag
/// (`-r`, `--`) never. Judging only the words that do not start with `-` let
/// the `=value` spelling of a target through while `-t /tmp` was refused.
pub(super) fn operand_leaves_cwd(cleaned: &str) -> bool {
    let operand = if cleaned.starts_with('-') {
        match cleaned.split_once('=') {
            Some((_, value)) => value,
            None => return false,
        }
    } else {
        cleaned
    };
    escapes_cwd_by_spelling(operand)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Candidate words that might hand execution to the rest of the line.
    /// Deliberately broader than either list above — the point is that the
    /// SHELL decides which of these belong, not the author.
    ///
    /// Absent on purpose: `sudo`/`su`/`doas` (the deny floor refuses them
    /// outright, and a host with passwordless sudo would run the recorder),
    /// and `ssh`/`watch`-style words are covered but bounded by the deadline
    /// below because they never exit on their own.
    #[cfg(unix)]
    const WRAPPER_CANDIDATES: &[&str] = &[
        "eval",
        "source",
        ".",
        "exec",
        "command",
        "builtin",
        "env",
        "time",
        "nohup",
        "nice",
        "setsid",
        "stdbuf",
        "timeout",
        "ionice",
        "taskset",
        "unshare",
        "chroot",
        "flock",
        "caffeinate",
        "xcrun",
        "arch",
        "watch",
        "script",
        "proxychains",
        "parallel",
        "xargs",
        "busybox",
        "runuser",
        "sh",
        "bash",
        "zsh",
        "dash",
        "ksh",
        "fish",
        "perl",
        "python",
        "python3",
        "ruby",
        "node",
        "php",
        "awk",
        "gawk",
        "osascript",
        "tclsh",
        "lua",
        "julia",
        "Rscript",
        "deno",
        "bun",
        "if",
        "then",
        "else",
        "while",
        "do",
        "!",
    ];

    #[test]
    fn basename_lower_strips_paths_quotes_and_executable_suffixes() {
        assert_eq!(basename_lower("powershell.exe"), "powershell");
        assert_eq!(basename_lower("C:/Windows/System32/cmd.exe"), "cmd");
        assert_eq!(basename_lower("REG.EXE"), "reg");
        assert_eq!(basename_lower("format.com"), "format");
        assert_eq!(basename_lower("takeown.exe"), "takeown");
        // A dot that is not an executable suffix stays part of the name.
        assert_eq!(basename_lower("my.script"), "my.script");
        // A bare suffix is a real (if odd) filename, not an empty stem.
        assert_eq!(basename_lower(".exe"), ".exe");
    }
    /// Whether `sh` hands the rest of the line to the recorder when the line
    /// opens with `word`. Bounded: some candidates (`watch`) never exit.
    #[cfg(unix)]
    fn shell_runs_the_tail(word: &str, index: usize, dir: &std::path::Path) -> bool {
        use std::io::Write;
        let marker = dir.join(format!("ran{index}"));
        let recorder = dir.join(format!("rec{index}"));
        let mut file = std::fs::File::create(&recorder).unwrap();
        writeln!(file, "#!/bin/sh\n: > {}", marker.display()).unwrap();
        drop(file);
        std::fs::set_permissions(
            &recorder,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let Ok(mut child) = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{word} {}", recorder.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        marker.exists()
    }

    /// The completeness property the two lists above exist to satisfy, checked
    /// against the real shell instead of against the author's memory.
    ///
    /// If `sh` runs the rest of the line, the identity side MUST NOT collapse
    /// that line to the bare word — otherwise one session "a" on
    /// `source .venv/bin/activate` silently covers `source ./anything.sh` for
    /// the rest of the session. Hand-maintained lists kept missing members of
    /// this class one round at a time (`sh`/`time` one release, `eval`/
    /// `source`/`.` the next); this test enumerates the class instead.
    ///
    /// It is one-directional on purpose: a word in the lists that this host's
    /// shell does not treat as a wrapper is a harmless over-approximation
    /// (`busybox` and the Windows interpreters are not installed here), so
    /// only the unsafe direction is asserted.
    #[cfg(unix)]
    #[test]
    fn every_word_the_shell_runs_the_tail_for_is_known() {
        let dir = std::env::temp_dir().join(format!("dc-wrapper-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut missing = Vec::new();
        for (index, word) in WRAPPER_CANDIDATES.iter().enumerate() {
            if shell_runs_the_tail(word, index, &dir)
                && !runs_the_rest_of_the_line(&basename_lower(word))
            {
                missing.push(*word);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            missing.is_empty(),
            "the shell hands the rest of the line to these words, but the identity side still \
             collapses such a line to the bare word — a session consent on one of them would \
             cover every command reachable through it: {missing:?}"
        );
    }

    /// The rules that read a character the shell rewrites a word for (see the
    /// test below): expansion, substitution, redirection and grouping are
    /// indirection; `;`/`|`/`&` split segments; quotes and the backslash are
    /// stripped by `clean_token`; a `~`-led operand is refused by
    /// `escapes_cwd_by_spelling`; and `#` only ever makes the shell run a
    /// *prefix* of what the gate read — the safe direction, so it needs no rule.
    fn rewriting_character_is_read(c: char) -> bool {
        has_shell_indirection(&c.to_string())
            || matches!(c, ';' | '|' | '&')
            || matches!(c, '\'' | '"' | '\\')
            || c == '~'
            || c == '#'
    }

    /// The characters the shell rewrites a word for, enumerated against the
    /// real shell instead of remembered. For every ASCII punctuation character,
    /// words built around it — alone, at either end, in the middle, doubled
    /// (`` `a` ``, `'a'`, `$a$`), and closed by `}` or `]` (`a{x,y}`, `a[x,y]`,
    /// `a{1..3}`) — are handed to `sh -c 'printf "%s\n" …'` in a directory seeded
    /// with names a pattern can match. If what comes back is not the words as
    /// written — expanded, split, dropped, or a failed parse — that character
    /// rewrites text, and some rule must read it (`rewriting_character_is_read`).
    /// Only `}` and `]` serve as closers because they are the two the shell
    /// leaves alone when unpaired: any other closer would break the batch on
    /// its own and charge the fault to the character under test.
    ///
    /// Hand-maintained, the indirection list missed one stage per review round:
    /// quote removal, then brace expansion, then pathname expansion. The
    /// alphabet is finite, so this is enumeration, not sampling: a
    /// single-character trigger no rule reads cannot exist on this host without
    /// failing here. One-directional like the wrapper test above — a listed
    /// character the host's shell does not rewrite (dash and braces) only ever
    /// costs a prompt.
    #[cfg(unix)]
    #[test]
    fn every_punctuation_the_shell_rewrites_is_accounted_for() {
        let dir = std::env::temp_dir().join(format!("dc-punct-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["ax", "ay", "a1", "ab"] {
            std::fs::File::create(dir.join(name)).unwrap();
        }
        let punctuation: Vec<char> = (b'!'..=b'~')
            .filter(u8::is_ascii_punctuation)
            .map(char::from)
            .collect();
        let mut unaccounted = Vec::new();
        for &c in &punctuation {
            let mut words = vec![
                c.to_string(),
                format!("a{c}"),
                format!("{c}a"),
                format!("a{c}b"),
                format!("{c}a{c}"),
            ];
            for d in ['}', ']', c] {
                words.push(format!("a{c}x,y{d}"));
                words.push(format!("a{c}1..3{d}"));
            }
            let script = format!("printf '%s\\n' {}", words.join(" "));
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .current_dir(&dir)
                .env("HOME", &dir)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .expect("run sh");
            let unchanged = output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .eq(words.iter().map(String::as_str));
            if !unchanged && !rewriting_character_is_read(c) {
                unaccounted.push(c);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            unaccounted.is_empty(),
            "the shell rewrites a word around these characters and no rule reads them — a \
             trusted or accept-edits command spelled with one runs as something the gate never \
             saw: {unaccounted:?}"
        );
    }
}
