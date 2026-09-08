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
pub(super) const PREFIX_WORDS: &[&str] = &[
    "if", "then", "else", "elif", "while", "until", "do", "for", "case", "!", "time", "exec",
    "command", "builtin", "env", "nohup", "nice", "busybox", "xargs",
];

/// Interpreters that execute whatever text they are handed — a script path, a
/// `-c` string, or piped stdin. The deny floor uses the list to refuse a
/// network fetch piped into one; the identity side uses it the way it uses
/// [`PREFIX_WORDS`]: `sh -c '<anything>'` and `python <any script>` name no
/// operation of their own, so a consent on one must not cover another.
pub(super) const INTERPRETERS: &[&str] = &[
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

/// True if a command contains shell redirection, substitution, or expansion
/// (`>`, `<`, `` ` ``, `$`, `{`/`}`). These make the visible text an unreliable
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
#[must_use]
pub(super) fn has_shell_indirection(command: &str) -> bool {
    command.contains(['>', '<', '`', '$', '{', '}'])
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
