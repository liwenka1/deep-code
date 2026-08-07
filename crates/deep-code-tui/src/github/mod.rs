//! `deepcode github install` / `status` — one command to wire a repository up
//! to the CI bot.
//!
//! Everything here runs with the user's own `gh` credentials on their own
//! machine. There is no hosted service, no key custody, nothing to keep
//! running: the whole "installation" is one workflow file plus one or three
//! repository secrets.

mod env;
mod workflow;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::cli::{GithubCommand, InstallArgs};
use env::{GhStatus, RepoSlug};
use workflow::{
    API_KEY_SECRET, APP_ID_SECRET, APP_KEY_SECRET, DEFAULT_WORKFLOW_PATH, DEFAULT_WORKFLOW_REF,
    WorkflowSpec,
};

const EXIT_OK: i32 = 0;
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;

pub fn run(command: GithubCommand) -> i32 {
    match command {
        GithubCommand::Install(args) => install(args),
        GithubCommand::Status => status(),
    }
}

fn install(args: InstallArgs) -> i32 {
    let Some(root) = env::git_root() else {
        eprintln!("not inside a git repository — run this from the repo you want the bot in");
        return EXIT_USAGE;
    };

    let workflow_ref = args
        .workflow_ref
        .clone()
        .unwrap_or_else(|| DEFAULT_WORKFLOW_REF.to_string());
    let spec = WorkflowSpec {
        workflow_ref,
        with_app: args.with_app || args.app_id.is_some(),
        lang: args.lang.clone().unwrap_or_else(|| "zh".to_string()),
        permission_mode: args
            .permission_mode
            .clone()
            .unwrap_or_else(|| "accept_edits".to_string()),
        generator_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let rendered = workflow::render(&spec);

    if args.print_only {
        print!("{rendered}");
        return EXIT_OK;
    }

    let target = root.join(
        args.path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_WORKFLOW_PATH)),
    );
    match write_workflow(&target, &rendered, args.force) {
        Ok(WriteOutcome::Created) => println!("✓ wrote {}", display(&root, &target)),
        Ok(WriteOutcome::Updated) => println!("✓ updated {}", display(&root, &target)),
        Ok(WriteOutcome::Unchanged) => {
            println!("· {} already up to date", display(&root, &target));
        }
        Err(WriteError::Exists) => {
            eprintln!(
                "{} already exists and differs. Re-run with --force to replace it, \
                 or --print to see what would be written.",
                display(&root, &target)
            );
            return EXIT_FAILURE;
        }
        Err(WriteError::Io(error)) => {
            eprintln!("cannot write {}: {error}", display(&root, &target));
            return EXIT_FAILURE;
        }
    }

    let repo = env::detect_repo();
    let gh = env::gh_status();
    let secrets_done = if args.skip_secrets {
        println!("· skipping secrets (--skip-secrets)");
        false
    } else {
        configure_secrets(&args, repo.as_ref(), gh, &spec)
    };

    print_next_steps(&spec, repo.as_ref(), gh, secrets_done, &root, &target);
    EXIT_OK
}

/// Push the secrets the generated workflow expects. Returns whether the API
/// key ended up configured, so the closing summary can tell the truth.
fn configure_secrets(
    args: &InstallArgs,
    repo: Option<&RepoSlug>,
    gh: GhStatus,
    spec: &WorkflowSpec,
) -> bool {
    let Some(repo) = repo else {
        eprintln!("· no GitHub remote found; set the secrets yourself (listed below)");
        return false;
    };
    match gh {
        GhStatus::NotInstalled => {
            eprintln!("· gh CLI not found; set the secrets yourself (listed below)");
            return false;
        }
        GhStatus::NotAuthenticated => {
            eprintln!(
                "· gh is not logged in (`gh auth login`); set the secrets yourself (listed below)"
            );
            return false;
        }
        GhStatus::Ready => {}
    }

    let mut api_key_ok = false;
    match resolve_api_key(args) {
        Some(key) => match env::set_secret(repo, API_KEY_SECRET, &key) {
            Ok(()) => {
                println!("✓ set {API_KEY_SECRET} on {}", repo.full());
                api_key_ok = true;
            }
            Err(error) => eprintln!("· could not set {API_KEY_SECRET}: {error}"),
        },
        None => eprintln!("· no DeepSeek API key found; set {API_KEY_SECRET} yourself"),
    }

    if spec.with_app {
        match resolve_app_credentials(args) {
            Ok(Some((app_id, private_key))) => {
                for (name, value) in [(APP_ID_SECRET, app_id), (APP_KEY_SECRET, private_key)] {
                    match env::set_secret(repo, name, &value) {
                        Ok(()) => println!("✓ set {name} on {}", repo.full()),
                        Err(error) => eprintln!("· could not set {name}: {error}"),
                    }
                }
            }
            Ok(None) => eprintln!("· App secrets not provided; set them yourself (listed below)"),
            Err(error) => eprintln!("· {error}"),
        }
    }
    api_key_ok
}

/// The key already on this machine, preferred over asking: the common case is
/// installing from a workstation where deepcode is configured.
fn resolve_api_key(args: &InstallArgs) -> Option<String> {
    if let Some(key) = args.api_key.clone().filter(|key| !key.trim().is_empty()) {
        return Some(key);
    }
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }
    let workspace = std::env::current_dir().ok()?;
    deep_code_agent::AgentConfig::load(&workspace)
        .config
        .api_key
        .filter(|key| !key.trim().is_empty())
}

type AppCredentials = Option<(String, String)>;

fn resolve_app_credentials(args: &InstallArgs) -> Result<AppCredentials, String> {
    if let (Some(id), Some(key_file)) = (args.app_id.as_ref(), args.app_key_file.as_ref()) {
        let pem = std::fs::read_to_string(key_file)
            .map_err(|error| format!("cannot read {}: {error}", key_file.display()))?;
        return Ok(Some((id.clone(), pem)));
    }
    if args.app_id.is_some() != args.app_key_file.is_some() {
        return Err("--app-id and --app-private-key must be given together".to_string());
    }

    // Interactive fallback. A non-TTY (CI, a pipe) cannot answer prompts, so
    // say what to pass instead of blocking on a read that never returns.
    if !std::io::stdin().is_terminal() {
        return Err(
            "no terminal for the App prompts — pass --app-id and --app-private-key instead"
                .to_string(),
        );
    }

    print_app_instructions();
    let app_id = prompt("App ID: ")?;
    if app_id.is_empty() {
        return Ok(None);
    }
    let key_path = prompt("Path to the downloaded .pem private key: ")?;
    if key_path.is_empty() {
        return Ok(None);
    }
    let expanded = expand_tilde(&key_path);
    let pem = std::fs::read_to_string(&expanded)
        .map_err(|error| format!("cannot read {}: {error}", expanded.display()))?;
    Ok(Some((app_id, pem)))
}

fn print_app_instructions() {
    println!();
    println!("Create the GitHub App (about two minutes, all in the browser):");
    println!("  1. open https://github.com/settings/apps/new");
    println!("  2. name it (e.g. `deepcode-agent`), Homepage URL can be this repo");
    println!("  3. UNCHECK Webhook → Active");
    println!("  4. Repository permissions → Contents, Issues, Pull requests: Read and write");
    println!("  5. Create, then 'Generate a private key' and keep the .pem it downloads");
    println!("  6. left sidebar → Install App → install it on this repository only");
    println!();
    println!("Then paste the two values here (blank to skip and do it later):");
}

fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("cannot write the prompt: {error}"))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("cannot read the answer: {error}"))?;
    Ok(line.trim().to_string())
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

fn print_next_steps(
    spec: &WorkflowSpec,
    repo: Option<&RepoSlug>,
    gh: GhStatus,
    api_key_ok: bool,
    root: &Path,
    target: &Path,
) {
    let mut manual: Vec<String> = Vec::new();
    if !api_key_ok {
        manual.push(format!(
            "set the {API_KEY_SECRET} secret (Settings → Secrets and variables → Actions)"
        ));
    }
    if spec.with_app && matches!(gh, GhStatus::Ready) && repo.is_some() {
        // Already reported per-secret above; nothing to repeat here.
    } else if spec.with_app {
        manual.push(format!("set {APP_ID_SECRET} and {APP_KEY_SECRET}"));
    }

    println!();
    println!("Next:");
    let path = display(root, target);
    println!("  · commit {path} and push it");
    for step in &manual {
        println!("  · {step}");
    }
    println!("  · comment `/deepcode <instruction>` on any issue to try it");
    println!();
    println!(
        "The bot runs the pipeline at {}. Only the repository OWNER can trigger it;",
        spec.workflow_ref
    );
    println!("widening `allowed-associations` lets anyone who can comment run shell in this CI.");
    if !spec.with_app {
        println!();
        println!(
            "Tip: `--with-app` gives the bot its own identity — commits count toward contributors,"
        );
        println!(
            "     and its pushes trigger your other workflows (a GITHUB_TOKEN push never does)."
        );
    }
}

fn status() -> i32 {
    let Some(root) = env::git_root() else {
        eprintln!("not inside a git repository");
        return EXIT_USAGE;
    };
    let target = root.join(DEFAULT_WORKFLOW_PATH);

    println!("workflow: {}", display(&root, &target));
    match std::fs::read_to_string(&target) {
        Ok(content) => {
            let pinned = content
                .lines()
                .find_map(|line| line.trim().strip_prefix("uses:"))
                .map(str::trim)
                .unwrap_or("(no `uses:` line — not a generated caller?)");
            println!("  ✓ present, pinned at {pinned}");
            if content.contains("app-private-key") {
                println!("  ✓ GitHub App wiring present");
            } else {
                println!(
                    "  · no App wiring (runs as github-actions[bot]; its pushes do not trigger CI)"
                );
            }
        }
        Err(_) => println!("  ✗ not installed — run `deepcode github install`"),
    }

    match env::detect_repo() {
        Some(repo) => {
            println!("repository: {}", repo.full());
            match env::gh_status() {
                GhStatus::Ready => match env::list_secrets(&repo) {
                    Some(names) => {
                        for secret in [API_KEY_SECRET, APP_ID_SECRET, APP_KEY_SECRET] {
                            let present = names.iter().any(|name| name == secret);
                            println!("  {} {secret}", if present { '✓' } else { '·' });
                        }
                    }
                    None => println!("  · cannot list secrets (insufficient gh permissions?)"),
                },
                GhStatus::NotAuthenticated => println!("  · gh is not logged in (`gh auth login`)"),
                GhStatus::NotInstalled => println!("  · gh CLI not installed"),
            }
        }
        None => println!("repository: (no GitHub remote detected)"),
    }
    EXIT_OK
}

enum WriteOutcome {
    Created,
    Updated,
    Unchanged,
}

enum WriteError {
    Exists,
    Io(std::io::Error),
}

/// Write unless it would clobber someone's edits. An identical file counts as
/// success (re-running the command is how you upgrade), and a *generated* file
/// that merely drifted to a new version is replaced only with `--force`.
fn write_workflow(target: &Path, content: &str, force: bool) -> Result<WriteOutcome, WriteError> {
    let existing = std::fs::read_to_string(target).ok();
    match existing {
        Some(current) if current == content => return Ok(WriteOutcome::Unchanged),
        Some(_) if !force => return Err(WriteError::Exists),
        _ => {}
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(WriteError::Io)?;
    }
    std::fs::write(target, content).map_err(WriteError::Io)?;
    Ok(if existing.is_some() {
        WriteOutcome::Updated
    } else {
        WriteOutcome::Created
    })
}

fn display(root: &Path, target: &Path) -> String {
    target
        .strip_prefix(root)
        .unwrap_or(target)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_is_idempotent_and_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".github/workflows/deepcode.yml");

        assert!(matches!(
            write_workflow(&target, "one", false),
            Ok(WriteOutcome::Created)
        ));
        // Re-running the installer must not fail just because it already ran.
        assert!(matches!(
            write_workflow(&target, "one", false),
            Ok(WriteOutcome::Unchanged)
        ));
        // Different content without --force would silently discard edits.
        assert!(matches!(
            write_workflow(&target, "two", false),
            Err(WriteError::Exists)
        ));
        assert!(matches!(
            write_workflow(&target, "two", true),
            Ok(WriteOutcome::Updated)
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "two");
    }

    #[test]
    fn tilde_expands_only_at_the_start() {
        unsafe { std::env::set_var("HOME", "/home/test") };
        assert_eq!(
            expand_tilde("~/key.pem"),
            PathBuf::from("/home/test/key.pem")
        );
        assert_eq!(expand_tilde("/abs/key.pem"), PathBuf::from("/abs/key.pem"));
        assert_eq!(expand_tilde("rel/~/x.pem"), PathBuf::from("rel/~/x.pem"));
    }

    #[test]
    fn display_is_repo_relative() {
        let root = Path::new("/repo");
        assert_eq!(
            display(root, Path::new("/repo/.github/workflows/deepcode.yml")),
            ".github/workflows/deepcode.yml"
        );
        // A path outside the root stays absolute rather than being mangled.
        assert_eq!(
            display(root, Path::new("/elsewhere/x.yml")),
            "/elsewhere/x.yml"
        );
    }
}
