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
//! Two readings of a word live here, and which one a rule wants is not a
//! preference. [`clean_token`] is the *text* reading: it deletes quoting
//! wherever it sits, over-approximating on purpose, and it is what the deny
//! floor needs because the floor also covers text a human approved and a shell
//! will re-read. [`executed_words`] is the *argv* reading: the words the
//! executor really passes to `execve`. Any rule whose promise is about what the
//! program receives — the trust fences, the session key — must read that one;
//! the two are equal only by accident, and each accident was a bug.
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

/// The words `command` runs as, when it is one simple command the executor
/// could run unattended: the argv [`parse_unattended`] produced, program word
/// first. `None` for anything else — a sequence, or a line the executor would
/// refuse.
///
/// This is what every rule that judges *arguments* must read. The alternative
/// reading, [`clean_token`] over whitespace-split tokens, is a second grammar
/// for the same text, and the two are equal only by accident: it deletes
/// quotes wherever they sit, so `'--'` stayed a quoted word to the rules while
/// the executor passed a real `--` to the program, and it keeps a Windows `\`
/// the parser eats. Both readings existed because the rules were written when
/// the text went to `sh -c`; the executor now runs this argv, so the rules read
/// it too.
#[must_use]
pub(super) fn executed_words(command: &str) -> Option<Vec<String>> {
    let mut commands = parse_unattended(command)?;
    if commands.len() != 1 {
        return None;
    }
    Some(commands.pop()?.argv)
}

/// How a command in an unattended sequence is gated on the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunIf {
    /// Runs regardless: the first command, or one after `;` or a newline.
    Always,
    /// Runs only if the previous command exited 0 (`&&`).
    PreviousSucceeded,
}

/// One simple command of an unattended sequence: the exact argv the executor
/// hands to `execve`, and whether it is gated on the previous command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedCommand {
    pub argv: Vec<String>,
    pub run_if: RunIf,
}

/// The argv sequence of a command line that may run *unattended* — on the
/// strength of the policy's own parse, with no human reading the text — or
/// `None` when the line uses anything a shell would have to interpret.
///
/// This is the other half of the trust gate's promise. Every rule in this
/// module and in `command_shape` judges the *written* words; until now the
/// words then went to `sh -c`, which reads them by its own grammar, and every
/// difference between the two grammars was a way to run something the gate
/// never saw — quote splicing, brace expansion, globbing, an implicit
/// `--no-index`: ten review rounds of them, one table entry at a time. A
/// command that clears the gate is now executed as the argv produced here, so
/// the words the gate judged are the words that run, by construction rather
/// than by the completeness of any list.
///
/// Accepted grammar — the subset whose meaning this parser and `sh` agree on
/// exactly (`unattended_parse_matches_sh_word_splitting` checks that against
/// the real shell): words split on unquoted blanks; `'…'` literal; `"…"`
/// literal except `\"` and `\\`; a backslash escapes the next character (a
/// backslash-newline is a continuation); `#` opening a word comments out the
/// rest of the line; commands chain by `;`, a newline, or `&&`. Everything else
/// is `None` — `|`, `||`, a lone `&`, redirection, substitution, any expansion
/// ([`has_shell_indirection`]), an unterminated quote, an empty command, a
/// `~`-led word (the shell would expand it; the gate refuses such an operand
/// anyway), a program word carrying `=` (an assignment prefix) — so the gate
/// does not auto-approve it and the executor does not run it unattended.
/// Whether `\` escapes the character after it.
///
/// It does in `sh`, and it does not on Windows, where it is the path separator
/// and `CreateProcess` — which is what an unattended command reaches, with no
/// `cmd.exe` in between — never had a backslash escape. Reading it as an escape
/// there silently ate every ordinary path spelling: `git diff src\main.rs` ran
/// as `git diff srcmain.rs` on the default-trusted `git diff`, and `.\.`
/// collapsed to `..`, which is a *different directory* than the operand fence
/// read. [`clean_token`] splits on this same platform line, for the same
/// reason.
///
/// The one Windows position where `\` is *not* an ordinary character is
/// immediately before a quote: `CommandLineToArgvW` counts the backslash run
/// there, so `\"` is a literal quote to the program while `\\"` is a backslash
/// plus a real quote — and `cmd.exe` counts it differently again. This parser
/// does not guess between them; [`backslash_hugs_a_quote`] refuses such a line,
/// which costs a prompt and keeps the fence honest.
const BACKSLASH_ESCAPES: bool = !cfg!(windows);

/// Whether the `\` just consumed sits immediately before a quote — the one
/// spelling whose meaning Windows argument parsers disagree about (see
/// [`BACKSLASH_ESCAPES`]). Only ever consulted on Windows; on Unix the escape
/// arm above has already claimed the backslash.
fn backslash_hugs_a_quote(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    chars.peek().is_some_and(|next| matches!(next, '"' | '\''))
}

#[must_use]
pub fn parse_unattended(command: &str) -> Option<Vec<UnattendedCommand>> {
    if has_shell_indirection(command) {
        return None;
    }
    let mut commands: Vec<UnattendedCommand> = Vec::new();
    let mut argv: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    // How the command being collected is gated: set by the separator that
    // opened it, `Always` for the first.
    let mut gate = RunIf::Always;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' if BACKSLASH_ESCAPES => match chars.next() {
                // A trailing backslash is an incomplete line to the shell.
                None => return None,
                Some('\n') => {}
                Some(next) => word.get_or_insert_with(String::new).push(next),
            },
            '\\' if backslash_hugs_a_quote(&mut chars) => return None,
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        None => return None,
                        Some('\'') => break,
                        Some(ch) => w.push(ch),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        None => return None,
                        Some('"') => break,
                        Some('\\') if BACKSLASH_ESCAPES => match chars.next() {
                            None => return None,
                            Some(escaped @ ('"' | '\\')) => w.push(escaped),
                            Some('\n') => {}
                            Some(other) => {
                                w.push('\\');
                                w.push(other);
                            }
                        },
                        Some('\\') if backslash_hugs_a_quote(&mut chars) => return None,
                        Some(ch) => w.push(ch),
                    }
                }
            }
            ' ' | '\t' => {
                if let Some(w) = word.take() {
                    argv.push(w);
                }
            }
            '\n' | ';' => {
                if let Some(w) = word.take() {
                    argv.push(w);
                }
                if argv.is_empty() {
                    // `;` with nothing before it is a syntax error to the
                    // shell; a blank line is not.
                    if c == ';' {
                        return None;
                    }
                    continue;
                }
                commands.push(UnattendedCommand {
                    argv: std::mem::take(&mut argv),
                    run_if: gate,
                });
                gate = RunIf::Always;
            }
            '&' => {
                chars.next_if_eq(&'&')?;
                if let Some(w) = word.take() {
                    argv.push(w);
                }
                if argv.is_empty() {
                    return None;
                }
                commands.push(UnattendedCommand {
                    argv: std::mem::take(&mut argv),
                    run_if: gate,
                });
                gate = RunIf::PreviousSucceeded;
            }
            '|' => return None,
            '#' if word.is_none() => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            other => word.get_or_insert_with(String::new).push(other),
        }
    }
    if let Some(w) = word.take() {
        argv.push(w);
    }
    if !argv.is_empty() {
        commands.push(UnattendedCommand { argv, run_if: gate });
    } else if gate == RunIf::PreviousSucceeded {
        // `a &&` with nothing after it.
        return None;
    }
    if commands.is_empty() {
        return None;
    }
    for command in &commands {
        let program = command.argv.first()?;
        if program.is_empty() || program.contains('=') {
            return None;
        }
        if command.argv.iter().any(|word| word.starts_with('~')) {
            return None;
        }
    }
    Some(commands)
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
    /// Unix-only like the test that reads it: on Windows the pair is dead code
    /// and `-D warnings` refuses to compile it.
    #[cfg(unix)]
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

    fn argvs(command: &str) -> Vec<Vec<String>> {
        parse_unattended(command)
            .expect("parses")
            .into_iter()
            .map(|cmd| cmd.argv)
            .collect()
    }

    fn words(argvs: &[&[&str]]) -> Vec<Vec<String>> {
        argvs
            .iter()
            .map(|argv| argv.iter().map(|word| (*word).to_string()).collect())
            .collect()
    }

    fn gates(command: &str) -> Vec<RunIf> {
        parse_unattended(command)
            .expect("parses")
            .iter()
            .map(|cmd| cmd.run_if)
            .collect()
    }

    /// `\` escapes on Unix (that is what `sh` does, pinned against the real
    /// shell by `unattended_parse_matches_sh_word_splitting`) and is an
    /// ordinary path separator on Windows, where the word reaches
    /// `CreateProcess` with no shell in between. Reading it as an escape there
    /// ran `git diff src\main.rs` as `git diff srcmain.rs` on the
    /// default-trusted `git diff`, and collapsed `.\.` — the current
    /// directory — into `..`, its parent, which is a path the operand fence
    /// would have refused.
    #[test]
    fn backslash_escapes_on_unix_and_separates_paths_on_windows() {
        // Both arms end at the same invariant: the word the fence judges is the
        // word the executor opens. The platforms differ on what that word IS.
        #[cfg(unix)]
        {
            assert_eq!(
                argvs(r"git diff src\main.rs"),
                words(&[&["git", "diff", "srcmain.rs"]])
            );
            assert_eq!(argvs(r"mkdir .\."), words(&[&["mkdir", ".."]]));
            assert!(operand_leaves_cwd(".."));
            // A backslash escapes a blank, continues a line, and a trailing one
            // leaves a line only a shell could finish.
            assert_eq!(
                argvs(r"cargo test e\ f"),
                words(&[&["cargo", "test", "e f"]])
            );
            assert_eq!(
                argvs("cargo build \\\n  --release\n\n"),
                words(&[&["cargo", "build", "--release"]])
            );
            assert_eq!(parse_unattended("echo done\\"), None);
            assert_eq!(
                argvs("git commit -m \"say \\\"hi\\\" \\\\ ok\""),
                words(&[&["git", "commit", "-m", "say \"hi\" \\ ok"]])
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                argvs(r"git diff src\main.rs"),
                words(&[&["git", "diff", r"src\main.rs"]])
            );
            assert_eq!(argvs(r"mkdir .\."), words(&[&["mkdir", r".\."]]));
            // `.\.` really is the current directory here, and the fence agrees.
            assert!(!operand_leaves_cwd(r".\."));
            // Not an escape: two words, and a trailing one is just a path.
            assert_eq!(
                argvs(r"cargo test e\ f"),
                words(&[&["cargo", "test", r"e\", "f"]])
            );
            assert_eq!(argvs(r"mkdir src\"), words(&[&["mkdir", r"src\"]]));
            // Hugging a quote is the one spelling Windows argument parsers
            // disagree about, so it is refused rather than guessed at.
            assert_eq!(
                parse_unattended("git commit -m \"say \\\"hi\\\" ok\""),
                None
            );
            assert_eq!(parse_unattended(r"echo \'x'"), None);
        }
    }

    #[test]
    fn unattended_parse_splits_words_like_the_shell() {
        assert_eq!(
            argvs("cargo test --all"),
            words(&[&["cargo", "test", "--all"]])
        );
        assert_eq!(
            argvs("cargo test --features \"a b\" 'c d'"),
            words(&[&["cargo", "test", "--features", "a b", "c d"]])
        );
        // Backslash spellings are platform-split; see
        // `backslash_escapes_on_unix_and_separates_paths_on_windows`.
        // Quote removal glues adjacent quoted pieces into one word; a quoted
        // empty string is a real (empty) argument.
        assert_eq!(
            argvs("printf a\"b\"'c' '' \"\""),
            words(&[&["printf", "abc", "", ""]])
        );
        // Inside double quotes a backslash before anything but `"`/`\` stays.
        assert_eq!(
            argvs("printf '%s\\n' \"a\\tb\""),
            words(&[&["printf", "%s\\n", "a\\tb"]])
        );
        // `#` opens a comment only at the start of a word.
        assert_eq!(
            argvs("cargo build # not run"),
            words(&[&["cargo", "build"]])
        );
        assert_eq!(argvs("cargo build#x"), words(&[&["cargo", "build#x"]]));
        // Blank lines are nothing.
        assert_eq!(
            argvs("cargo build\n\n  --release\n"),
            words(&[&["cargo", "build"], &["--release"]])
        );
        // Tabs and other blanks separate words too; a trailing `;` is fine.
        assert_eq!(argvs("git\tstatus ;"), words(&[&["git", "status"]]));
    }

    #[test]
    fn unattended_parse_chains_with_the_two_sequencing_operators() {
        assert_eq!(
            argvs("cargo build && cargo test; echo done\ngit status"),
            words(&[
                &["cargo", "build"],
                &["cargo", "test"],
                &["echo", "done"],
                &["git", "status"],
            ])
        );
        assert_eq!(
            gates("cargo build && cargo test; echo done\ngit status"),
            vec![
                RunIf::Always,
                RunIf::PreviousSucceeded,
                RunIf::Always,
                RunIf::Always
            ]
        );
        // A newline after `&&` is allowed, and the gate survives it.
        assert_eq!(
            gates("cargo build &&\ncargo test"),
            vec![RunIf::Always, RunIf::PreviousSucceeded]
        );
    }

    /// Everything a shell would have to interpret beyond quoting and the two
    /// sequencing operators is refused, so it can neither be auto-approved nor
    /// run unattended.
    #[test]
    fn unattended_parse_refuses_what_only_a_shell_can_read() {
        for command in [
            "",
            "   ",
            "a | b",
            "a || b",
            "a &",
            "a && b &",
            "a &&",
            "&& a",
            "; a",
            "a ;; b",
            "a; ; b",
            "echo \"unterminated",
            "echo 'unterminated",
            "echo $HOME",
            "echo `id`",
            "echo *.rs",
            "echo a?",
            "echo [a]",
            "echo {a,b}",
            "echo (a)",
            "echo a > b",
            "echo a < b",
            "cd ~/x",
            "echo ~",
            "FOO=1 cargo test",
            "'' cargo test",
        ] {
            assert_eq!(parse_unattended(command), None, "{command:?}");
        }
        // A trailing backslash is an unfinished line to `sh` only: on Windows
        // it is the last character of a path. Platform-split alongside every
        // other backslash spelling, in
        // `backslash_escapes_on_unix_and_separates_paths_on_windows`.
        #[cfg(unix)]
        assert_eq!(parse_unattended("echo done\\"), None);
    }

    /// The accepted grammar must mean to this parser exactly what it means to
    /// `sh`, or the words the gate judged are still not the words that run —
    /// the very gap this parser exists to close. Each spelling is run through
    /// the real shell as arguments to `printf '%s\n'`, one line per word, and
    /// compared with the parser's argv for the same text.
    #[cfg(unix)]
    #[test]
    fn unattended_parse_matches_sh_word_splitting() {
        for spelling in [
            "a b c",
            "\"a b\" c",
            "'c d' e",
            "e\\ f g",
            "\"q\\\"x\" y",
            "'it'\"'\"'s' z",
            "\"\" '' end",
            "a\"b\"c d",
            "x\\\\y z",
            "--flag=v --other=\"w x\"",
            "a\\tb c",
            "\"back\\\\slash\" \"quote\\\"d\" \"tab\\t\"",
            "tab\there",
            "line\\\ncontinued next",
            "trailing ;",
            "first # a comment",
            "hash#inside word",
            "'single \"double\" inside'",
            "\"double 'single' inside\"",
        ] {
            let command = format!("printf '%s\\n' {spelling}");
            let ours: Vec<String> = parse_unattended(&command)
                .unwrap_or_else(|| panic!("{command:?} must parse"))
                .into_iter()
                .flat_map(|cmd| cmd.argv.into_iter().skip(2))
                .collect();
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .stdin(std::process::Stdio::null())
                .output()
                .expect("run sh");
            assert!(output.status.success(), "{command:?} failed in sh");
            let theirs: Vec<&str> = std::str::from_utf8(&output.stdout)
                .expect("utf8")
                .lines()
                .collect();
            assert_eq!(ours, theirs, "{command:?} split differently from sh");
        }
    }
}
