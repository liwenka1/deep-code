//! Command identity extraction for the shell auto-allow gate.
//!
//! A trusted-prefix rule like `"git status"` must cover flag variations
//! (`git status -s`, `git status --porcelain`) without leaking trust to
//! sibling subcommands (`git push`). To decide where a command's *identity*
//! ends and its *arguments* begin, deep-code models each known program as a
//! [`Shape`]: how many positional words (program word included) name the
//! operation, with per-subcommand overrides for tools whose subcommands take
//! an operation-defining argument (`npm run <script>`, `docker volume <verb>`).
//!
//! Unknown programs collapse to their bare program word; unknown subcommands
//! of a *known* program keep the program's base depth, so a session approval
//! of `git frobnicate` never widens to other `git` subcommands.
//!
//! Flags are only skipped *after* the identity is complete. A flag sitting in
//! an identity position — `git --exec-path=/tmp/evil status` — can redirect
//! what actually executes, so such a command's identity degrades to its full
//! literal text: it matches no subcommand rule and a session approval of it
//! covers nothing but the byte-identical command.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Identity depth profile of one program.
///
/// `base` counts positional words — the program word itself included — that
/// form the operation name. `overrides` lists subcommands whose identity is
/// deeper or shallower than the program default.
#[derive(Debug, Clone, Copy)]
struct Shape {
    base: u8,
    overrides: &'static [(&'static str, u8)],
}

impl Shape {
    const fn flat() -> Self {
        Self {
            base: 1,
            overrides: &[],
        }
    }

    const fn sub() -> Self {
        Self {
            base: 2,
            overrides: &[],
        }
    }

    const fn sub_with(overrides: &'static [(&'static str, u8)]) -> Self {
        Self { base: 2, overrides }
    }

    /// Identity depth for a concrete subcommand (or `None` when the command
    /// line has no subcommand word yet).
    fn depth_for(&self, subcommand: Option<&str>) -> u8 {
        let Some(sub) = subcommand else {
            return self.base;
        };
        self.overrides
            .iter()
            .find_map(|&(name, depth)| (name == sub).then_some(depth))
            .unwrap_or(self.base)
    }
}

/// Programs whose invocations are named by more than the bare program word.
///
/// Anything absent from this map identifies as its program word alone
/// (`ls -la` → `ls`), which is the conservative default: a rule for such a
/// program either names it exactly or spells out a longer literal prefix.
static PROGRAM_SHAPES: LazyLock<HashMap<&'static str, Shape>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    // VCS and package managers: `<tool> <subcommand>` names the operation.
    m.insert("git", Shape::sub());
    m.insert("cargo", Shape::sub());
    m.insert("pip", Shape::sub());
    m.insert("pip3", Shape::sub());
    m.insert("deno", Shape::sub());
    m.insert("npm", Shape::sub_with(&[("run", 3)]));
    m.insert("yarn", Shape::sub_with(&[("run", 3), ("workspace", 3)]));
    m.insert("pnpm", Shape::sub_with(&[("run", 3)]));
    m.insert("bun", Shape::sub_with(&[("run", 3)]));
    // `npx <package>` — the package is the operation.
    m.insert("npx", Shape::sub());
    // Infra CLIs with noun-verb grammars: some subcommands open a group whose
    // next word completes the operation (`docker volume rm`, `go mod tidy`).
    m.insert(
        "docker",
        Shape::sub_with(&[
            ("compose", 3),
            ("container", 3),
            ("image", 3),
            ("network", 3),
            ("system", 3),
            ("volume", 3),
        ]),
    );
    m.insert(
        "kubectl",
        Shape::sub_with(&[
            ("create", 3),
            ("delete", 3),
            ("describe", 3),
            ("get", 3),
            ("rollout", 3),
            ("top", 3),
        ]),
    );
    m.insert("go", Shape::sub_with(&[("mod", 3), ("work", 3)]));
    m.insert(
        "rustup",
        Shape::sub_with(&[("target", 3), ("toolchain", 3)]),
    );
    m.insert(
        "terraform",
        Shape::sub_with(&[("state", 3), ("workspace", 3)]),
    );
    m.insert("helm", Shape::sub_with(&[("repo", 3)]));
    // `gh <noun> <verb>` throughout.
    m.insert(
        "gh",
        Shape {
            base: 3,
            overrides: &[],
        },
    );
    // `aws <service> <operation>`, except `aws configure` which is complete.
    m.insert(
        "aws",
        Shape {
            base: 3,
            overrides: &[("configure", 2)],
        },
    );
    // Build drivers where the bare program is the operation and every
    // positional word after it is a target, not a subcommand.
    m.insert("make", Shape::flat());
    m.insert("cmake", Shape::flat());
    m
});

/// Whether a flag can make an otherwise-trusted command run a different program
/// or write a caller-named path. Matched with or without an `=value` suffix, so
/// both `--config=x` and `--config x` are caught.
///
/// Deliberately narrow: only flags that change *what executes* or *what is
/// written*, so trusting `cargo build` still covers `--release`, `--features …`
/// and friends without a prompt.
fn redirects_execution(token: &str) -> bool {
    const REDIRECTING: &[&str] = &[
        // cargo: sets any config key, including `build.rustc-wrapper` and
        // `target.<triple>.runner` — literally "run this program instead".
        "--config",
        // git: relocates the helper binaries, or names a program to run.
        "--exec-path",
        "--upload-pack",
        "--receive-pack",
        // git: runs the external diff driver named by the repo's own config.
        "--ext-diff",
        // Writes a caller-named path (`git diff --output=<path>`).
        "--output",
        "--output-file",
        // cargo: writes the whole build tree into a caller-named directory.
        "--target-dir",
    ];
    // `sh -c` / `cmd /C` strip shell quoting before the flag reaches the
    // program, so `--con"fig"`, `'--config'` and (on Unix) `--config\` all run
    // as `--config`. Match on the same de-quoted token the shell will, reusing
    // the deny floor's stripper — the exact-string compare below otherwise
    // missed every quoted/backslashed spelling, and an auto-trusted `cargo`/`git`
    // executed an arbitrary program (`build.rustc-wrapper`, `--target-dir`,
    // `git --output`) with no prompt at any permission tier.
    let cleaned = super::shell_lex::clean_token(token);
    let name = cleaned.split('=').next().unwrap_or(&cleaned);
    REDIRECTING.contains(&name.to_ascii_lowercase().as_str())
}

/// The tokens that can still be flags: everything before a literal `--`.
///
/// The programs whose flags populate [`redirects_execution`] (cargo, git) all
/// treat `--` as the end of options, so a redirecting spelling after it is
/// data — a pathspec, or an argument handed to the test binary — not a flag.
/// Scanning past it would only degrade the identity of harmless commands
/// (`cargo test -- --output x`) into spurious prompts.
fn option_tokens<'a, 'b>(tokens: &'a [&'b str]) -> impl Iterator<Item = &'a &'b str> {
    tokens.iter().take_while(|token| **token != "--")
}

/// The identity of a tokenized command line: the positional words that name
/// the operation, lowercased and joined by single spaces.
///
/// Flag tokens (leading `-`) that come *after* a complete identity are
/// skipped — they vary freely without changing what the command is. A flag
/// *inside* the identity (between program and subcommand) degrades the
/// identity to the full literal command line; see the module docs. One
/// deliberate carve-out: for `python -m <module>` (and `python3`), the module
/// named by `-m` is the operation, so the identity keeps all three words.
#[must_use]
pub fn identity(tokens: &[&str]) -> String {
    let Some(first) = tokens.first() else {
        return String::new();
    };
    let program = first.to_ascii_lowercase();
    if program.starts_with('-') {
        return String::new();
    }

    // `python -m json.tool …` → the interpreter flag selects the program.
    if matches!(program.as_str(), "python" | "python3")
        && let [_, "-m", module, ..] = tokens
    {
        return format!("{program} -m {}", module.to_ascii_lowercase());
    }

    // Scan the whole line, not just the identity window. The loop below stops
    // looking at tokens the moment the identity is complete, so a redirecting
    // flag *after* the subcommand was invisible — and since a trusted rule
    // matches on identity alone, it rode in for free:
    // `cargo test --config 'build.rustc-wrapper="/tmp/x"'` kept the identity
    // `cargo test`, matched the default trust list, and executed an arbitrary
    // program with no prompt at any tier; `git diff --output=<path>` created or
    // overwrote an arbitrary file the same way. Neither contains `$`, `>`, `<`
    // or a backtick, so the structural-indirection gate did not catch them.
    if option_tokens(&tokens[1..]).any(|token| redirects_execution(token)) {
        return squeeze(&tokens.join(" "));
    }

    let shape = PROGRAM_SHAPES.get(program.as_str());
    let mut words = vec![program];
    for raw in &tokens[1..] {
        let needed = match shape {
            Some(shape) => usize::from(shape.depth_for(words.get(1).map(String::as_str))),
            None => 1,
        };
        if words.len() >= needed {
            break;
        }
        if raw.starts_with('-') {
            // A flag in identity position can redirect what executes
            // (`git --exec-path=…`, `git -c core.sshCommand=…`). Degrade to
            // the literal command line so only an identical rule matches.
            return squeeze(&tokens.join(" "));
        }
        words.push(raw.to_ascii_lowercase());
    }
    words.join(" ")
}

/// Whether the trusted-prefix `rule` covers the concrete `command`.
///
/// A rule matches when it equals the command's [identity](identity) —
/// flag-insensitive, subcommand-exact — or, for rules spelled out past the
/// identity depth, when it is a whole-word prefix of the command line
/// (`"git log --oneline"` covers `git log --oneline -5` but `"ls"` never
/// covers `lsof`).
#[must_use]
pub fn rule_covers(rule: &str, command: &str) -> bool {
    let rule = squeeze(rule);
    if rule.is_empty() {
        return false;
    }

    let tokens: Vec<&str> = command.split_whitespace().collect();

    // A redirecting flag must be spelled out in the rule itself. Degrading the
    // identity is not enough on its own, because the whole-word prefix branch
    // below matches the *raw* command line: `cargo test` covered
    // `cargo test --config 'build.rustc-wrapper="/tmp/x"'` through that branch
    // even once identity matching rejected it, so an auto-trusted command could
    // run an arbitrary program with no prompt at any permission tier.
    if option_tokens(&tokens)
        .any(|token| redirects_execution(token) && !rule_spells_out_flag(&rule, flag_name(token)))
    {
        return false;
    }

    if identity(&tokens) == rule {
        return true;
    }

    let command = squeeze(command);
    command == rule
        || (command.starts_with(&rule) && command.as_bytes().get(rule.len()) == Some(&b' '))
}

/// A flag token without its `=value` suffix, lowercased. Shell quoting is
/// stripped first (see [`redirects_execution`]) so a rule spelling out a
/// redirecting flag still matches its quoted command-line spelling.
fn flag_name(token: &str) -> String {
    let cleaned = super::shell_lex::clean_token(token);
    cleaned
        .split('=')
        .next()
        .unwrap_or(&cleaned)
        .to_ascii_lowercase()
}

/// Whether the (already squeezed) `rule` names `flag` explicitly, so an operator
/// who wants to trust a redirecting flag still can — by writing it out.
fn rule_spells_out_flag(rule: &str, flag: String) -> bool {
    rule.split_whitespace().any(|word| flag_name(word) == flag)
}

/// Lowercase and collapse runs of whitespace to single spaces.
fn squeeze(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_of(command: &str) -> String {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        identity(&tokens)
    }

    fn covers(rule: &str, command: &str) -> bool {
        rule_covers(rule, command)
    }

    #[test]
    fn identity_ends_at_subcommand_for_vcs_tools() {
        assert_eq!(identity_of("git status -s"), "git status");
        assert_eq!(identity_of("git push origin main --force"), "git push");
        assert_eq!(
            identity_of("cargo check --workspace --all-features"),
            "cargo check"
        );
    }

    #[test]
    fn identity_keeps_operation_argument_for_script_runners() {
        assert_eq!(
            identity_of("npm run build:prod --silent"),
            "npm run build:prod"
        );
        assert_eq!(identity_of("pnpm run lint"), "pnpm run lint");
        assert_eq!(identity_of("npm install lodash"), "npm install");
    }

    #[test]
    fn identity_descends_into_noun_verb_groups() {
        assert_eq!(identity_of("docker volume prune -f"), "docker volume prune");
        assert_eq!(identity_of("docker ps -a"), "docker ps");
        assert_eq!(identity_of("go mod tidy"), "go mod tidy");
        assert_eq!(identity_of("kubectl get pods -n prod"), "kubectl get pods");
        assert_eq!(identity_of("gh pr view 42"), "gh pr view");
    }

    #[test]
    fn identity_of_aws_configure_stops_early() {
        assert_eq!(identity_of("aws s3 sync . s3://bucket"), "aws s3 sync");
        assert_eq!(identity_of("aws configure list"), "aws configure");
    }

    #[test]
    fn identity_of_build_drivers_is_the_bare_program() {
        assert_eq!(identity_of("make -j8 release"), "make");
        assert_eq!(identity_of("cmake --build build"), "cmake");
    }

    #[test]
    fn identity_of_unknown_program_is_its_first_word() {
        assert_eq!(identity_of("ls -la /tmp"), "ls");
        assert_eq!(identity_of("rg pattern src/"), "rg");
    }

    #[test]
    fn identity_of_unknown_git_subcommand_stays_specific() {
        // Approving one exotic subcommand must not widen to all of `git`.
        assert_eq!(identity_of("git frobnicate --dry-run"), "git frobnicate");
    }

    #[test]
    fn identity_resolves_python_module_invocations() {
        assert_eq!(
            identity_of("python -m json.tool data.json"),
            "python -m json.tool"
        );
        assert_eq!(
            identity_of("python3 -m http.server 8000"),
            "python3 -m http.server"
        );
        assert_eq!(identity_of("python script.py"), "python");
    }

    #[test]
    fn identity_is_case_insensitive_and_empty_safe() {
        assert_eq!(identity_of("GIT STATUS"), "git status");
        assert_eq!(identity_of(""), "");
        assert_eq!(identity_of("--version"), "");
    }

    #[test]
    fn rule_ignores_flags_but_not_subcommands() {
        assert!(covers("git status", "git status --porcelain -b"));
        assert!(!covers("git status", "git push"));
        assert!(!covers("git status", "git checkout -b topic"));
    }

    #[test]
    fn rule_for_script_runner_pins_the_script() {
        assert!(covers("npm run dev", "npm run dev"));
        assert!(!covers("npm run dev", "npm run build"));
    }

    #[test]
    fn rule_spelled_past_identity_depth_matches_word_prefix() {
        assert!(covers("git log --oneline", "git log --oneline -5"));
        assert!(!covers("git log --oneline", "git log -p"));
    }

    #[test]
    fn rule_never_matches_inside_a_word() {
        assert!(covers("ls", "ls -la"));
        assert!(!covers("ls", "lsof -i :8080"));
    }

    #[test]
    fn rule_matching_normalizes_whitespace_and_case() {
        assert!(covers("  Git   Status ", "git    status  -s"));
    }

    #[test]
    fn empty_rule_covers_nothing() {
        assert!(!covers("", "git status"));
        assert!(!covers("   ", "git status"));
    }

    #[test]
    fn flag_in_identity_position_degrades_to_literal_command() {
        // `--exec-path` redirects git to an attacker-controlled helper dir;
        // it must never hide under a subcommand rule.
        assert_eq!(
            identity_of("git --exec-path=/tmp/evil frobnicate"),
            "git --exec-path=/tmp/evil frobnicate"
        );
        assert!(!covers(
            "git frobnicate",
            "git --exec-path=/tmp/evil frobnicate"
        ));
        assert!(!covers("git status", "git -c core.sshCommand=evil status"));
        assert!(!covers(
            "git push",
            "git --upload-pack=/tmp/evil push origin"
        ));
        // The degraded identity still matches a byte-identical rule.
        assert!(covers(
            "git --exec-path=/tmp/evil frobnicate",
            "git --exec-path=/tmp/evil frobnicate"
        ));
    }

    #[test]
    fn flags_after_a_complete_identity_stay_invisible() {
        assert!(covers("make", "make -j8 release"));
        assert!(covers("cargo build", "cargo build --release"));
        assert_eq!(identity_of("cargo build --release"), "cargo build");
        // Inert flags must stay inert, or trusting `cargo build` becomes useless.
        assert!(covers(
            "cargo test",
            "cargo test --features full -- --nocapture"
        ));
        assert!(covers("git diff", "git diff --stat --cached"));
    }

    /// A flag that redirects *what executes* or *what is written* must not ride
    /// in on a trusted identity. The identity loop stops as soon as the identity
    /// is complete, so these were invisible: `cargo test --config …` kept the
    /// identity `cargo test`, matched the built-in trust list, and ran an
    /// arbitrary program with no prompt at any permission tier.
    #[test]
    fn redirecting_flags_after_the_subcommand_break_the_identity() {
        for command in [
            "cargo test --config 'build.rustc-wrapper=\"/tmp/x/wrap\"'",
            "cargo build --config target.x86_64-unknown-linux-gnu.runner=/tmp/r",
            "cargo build --target-dir=/tmp/spray",
            "git diff --output=/tmp/leak",
            "git diff --ext-diff",
            "git log --ext-diff",
        ] {
            assert_ne!(
                identity_of(command),
                command
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" "),
                "{command:?} must not keep its two-word identity"
            );
        }
        assert!(!covers(
            "cargo test",
            "cargo test --config 'build.rustc-wrapper=\"/tmp/x\"'"
        ));
        assert!(!covers("git diff", "git diff --output=/tmp/leak"));
        // A byte-identical rule still matches the degraded identity.
        assert!(covers("git diff --ext-diff", "git diff --ext-diff"));
    }

    /// `--` ends the options for every program on the redirecting list (cargo,
    /// git), so a redirecting spelling after it is data, not a flag: cargo hands
    /// it to the test binary, git reads it as pathspec. Degrading on it would
    /// only turn harmless commands into spurious prompts.
    #[test]
    fn positionals_after_a_double_dash_are_not_flags() {
        assert_eq!(identity_of("cargo test -- --output /tmp/x"), "cargo test");
        assert!(covers("cargo test", "cargo test -- --config whatever"));
        assert!(covers("git diff", "git diff -- --output=odd-filename"));
        // Before the `--` it is still a flag and still breaks the trust.
        assert!(!covers("cargo test", "cargo test --config evil -- x"));
    }

    #[test]
    fn python_module_special_case_is_case_insensitive() {
        // macOS resolves PYTHON to python on its case-insensitive default
        // filesystem; the identity must not depend on the spelling.
        assert_eq!(
            identity_of("PYTHON -m http.server"),
            "python -m http.server"
        );
        // A session approval of `python script.py` (key "python") must not
        // cover the module form, whatever its spelling.
        assert_ne!(
            identity_of("python script.py"),
            identity_of("PYTHON -m http.server")
        );
    }

    /// A redirecting flag spelled with shell quoting or a backslash — which the
    /// shell strips before it reaches the program — must break the trust match
    /// exactly as its plain spelling does. Without the `clean_token` reuse in
    /// `redirects_execution`, `cargo build --con"fig" build.rustc-wrapper=…` kept
    /// the identity `cargo build`, matched the built-in trust list, and ran an
    /// arbitrary program with no prompt at any tier (the quote-splice bypass).
    #[test]
    fn quoted_or_escaped_redirecting_flag_still_breaks_trust() {
        assert!(!covers(
            "cargo build",
            "cargo build --con\"fig\" build.rustc-wrapper=/tmp/x"
        ));
        assert!(!covers(
            "cargo build",
            "cargo build --con'fig' build.rustc-wrapper=/tmp/x"
        ));
        assert!(!covers(
            "cargo build",
            "cargo build '--config' build.rustc-wrapper=/tmp/x"
        ));
        assert!(!covers(
            "cargo build",
            "cargo build --target\"-dir\" /tmp/spray"
        ));
        assert!(!covers("git diff", "git diff --out\"put\"=/tmp/leak"));
        // The degraded identity must not collapse to the two-word trusted form.
        assert_ne!(
            identity_of("cargo build --con\"fig\" build.rustc-wrapper=/tmp/x"),
            "cargo build"
        );
        // Backslash-escaped on Unix: `--config\ ` splits to a token carrying the
        // trailing backslash, which the shell would drop. Windows keeps `\` as a
        // path separator (cmd.exe escapes with `^`, not `\`), so it is not a
        // bypass there and the token stays untrusted for a different reason.
        #[cfg(not(windows))]
        assert!(!covers(
            "cargo build",
            "cargo build --config\\ build.rustc-wrapper=/tmp/x"
        ));
        // A benign quoted *argument* (not a redirecting flag) must stay trusted,
        // or the fix would turn ordinary quoted commands into spurious prompts.
        assert!(covers("cargo build", "cargo build --features \"foo\""));
        assert!(covers("git diff", "git diff \"HEAD~1\""));
    }
}
