//! CLI argument parsing for the `deep-code` binary.

use std::env;
use std::path::PathBuf;

use deep_code_agent::{
    AgentConfig, JsonSessionStore, Lang, PermissionMode, SessionId, SessionStore,
    format_sessions_storage_note, now_ms,
};

use crate::headless::OutputFormat;

/// What the interactive TUI should open with. A bare `deep-code` starts
/// fresh; resuming is opt-in, so stale context never leaks in by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupIntent {
    /// Start a new session (default, and `--new`).
    New,
    /// Resume the most recent non-empty session (`-c` / `--continue`).
    ContinueLatest,
    /// Show the session picker (`-r` / `--resume` with no id).
    ResumePicker,
    /// Resume a specific session id (`--resume <id>`, `session resume <id>`).
    ResumeId(String),
}

/// Headless one-shot mode (`-p`): run one prompt to completion without a
/// terminal UI. Parsed here; executed by `crate::headless::run_print`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintArgs {
    /// Positional prompt; `None` falls back to piped stdin (both may combine,
    /// see `headless::input`).
    pub prompt: Option<String>,
    /// Which session to run in: `--new` (default), `-c`, or `--resume <id>`.
    /// The interactive picker is rejected at parse time.
    pub intent: StartupIntent,
    pub output: OutputFormat,
    /// Session permission mode override for this run.
    pub permission_mode: Option<PermissionMode>,
    /// Wall-clock budget; on expiry the turn is cancelled and the exit code
    /// is 124.
    pub timeout_secs: Option<u64>,
    /// Mirror tool activity to stderr.
    pub verbose: bool,
    /// Extra writable roots (`--add-dir`, repeatable), canonical.
    pub add_dirs: Vec<PathBuf>,
}

/// `deepcode github install` — write the CI caller workflow and push the
/// secrets it needs, using the user's own `gh` credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallArgs {
    /// Wire up the optional GitHub App identity (prompts unless the two
    /// `--app-*` flags are given).
    pub with_app: bool,
    pub app_id: Option<String>,
    pub app_key_file: Option<PathBuf>,
    /// Ref of the reusable pipeline. Defaults to `main`; pass a tag to pin.
    pub workflow_ref: Option<String>,
    pub lang: Option<String>,
    pub permission_mode: Option<String>,
    /// Where to write, relative to the repository root.
    pub path: Option<PathBuf>,
    /// Replace an existing file whose contents differ.
    pub force: bool,
    /// Print the workflow instead of writing anything.
    pub print_only: bool,
    /// Write the workflow but touch no repository secrets.
    pub skip_secrets: bool,
    /// Key to store; defaults to the environment or the local config.
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubCommand {
    Install(InstallArgs),
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    Tui {
        intent: StartupIntent,
        /// Extra writable roots (`--add-dir`, repeatable), canonical.
        add_dirs: Vec<PathBuf>,
    },
    Print(PrintArgs),
    Github(GithubCommand),
    Doctor {
        json: bool,
    },
    Serve {
        host: String,
        port: u16,
        auth_token: Option<String>,
        resume: Option<String>,
        /// Headless/unattended: deny (never park) any approval that slips past
        /// auto-allow, so a gated tool can't hang the turn waiting for a
        /// callback no one will send. `--approval-mode autonomous` or env.
        autonomous_approvals: bool,
        /// Extra writable roots (`--add-dir`, repeatable), canonical.
        add_dirs: Vec<PathBuf>,
    },
    SessionList,
    SessionDelete {
        id: String,
    },
    SessionExport {
        id: String,
    },
    Eval {
        subset: String,
        split: String,
        sample: Option<usize>,
        parallel: usize,
        json: bool,
        markdown: bool,
        timeout_secs: u64,
        out_dir: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    pub mode: RunMode,
}

/// Whether the argv asks for help, in any position.
///
/// Split out from [`parse_args`] only because that function exits the process,
/// which makes the behaviour untestable in-crate.
fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

/// Whether the argv asks for headless print mode, in any position — `-p` may
/// come before or after the prompt or the resume flags. Checked only after
/// the subcommand match, so `serve`/`eval`/… keep owning their own flags.
fn wants_print(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "-p" || arg == "--print")
}

pub fn parse_args() -> CliArgs {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return CliArgs {
            mode: RunMode::Tui {
                intent: StartupIntent::New,
                add_dirs: Vec::new(),
            },
        };
    }

    // A help flag ANYWHERE means help. Recognizing it only as `argv[1]` meant
    // `doctor --help`, `serve --help`, `session --help` and `-c --help` each fell
    // into their own unknown-argument branch and exited 2 to stderr — the exact
    // defect that was fixed for the bare `--help`, just one level down. Safe as a
    // flat scan: no flag here accepts `--help`/`-h` as its value, and `-h` has no
    // other meaning in any subcommand.
    if wants_help(&args) {
        println!("{}", usage_text());
        std::process::exit(0);
    }

    match args[0].as_str() {
        "--version" | "-V" => {
            println!("{} {}", program_name(), env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        // Asked-for help goes to stdout and exits 0 — it is not an error. It
        // used to fall through to the unknown-argument branch below, so the
        // first thing a new user types printed "Unknown arguments: --help" to
        // stderr and exited 2.
        "--help" | "-h" | "help" => {
            println!("{}", usage_text());
            std::process::exit(0);
        }
        "session" => {
            args.remove(0);
            return parse_session_command(args);
        }
        "doctor" => {
            args.remove(0);
            return parse_doctor_command(args);
        }
        "serve" => {
            args.remove(0);
            return parse_serve_command(args);
        }
        "eval" => {
            args.remove(0);
            return parse_eval_command(args);
        }
        "github" => {
            args.remove(0);
            return parse_github_command(args);
        }
        _ => {}
    }

    if wants_print(&args) {
        return parse_print_args(args);
    }

    parse_tui_args(args)
}

/// Parse headless mode: `-p [PROMPT] [--output-format …] [--permission-mode …]
/// [--timeout SECS] [--verbose] [--new | -c | --resume <id>]`.
///
/// The prompt is positional (before or after the flags); with none, stdin is
/// the prompt. `-r`/`--resume` without an id is rejected here: the picker is
/// interactive and headless must never block on a keyboard.
fn parse_print_args(mut args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let usage = format!(
        "Usage: {prog} -p [PROMPT] [--output-format text|json|stream-json] \
[--permission-mode default|accept_edits|auto|yolo] [--timeout SECS] [--verbose] \
[--add-dir DIR]... [--new | -c | --resume <id>]"
    );

    let mut prompt: Option<String> = None;
    let mut intent = StartupIntent::New;
    let mut output = OutputFormat::Text;
    let mut permission_mode = None;
    let mut timeout_secs = None;
    let mut verbose = false;
    let mut add_dirs: Vec<PathBuf> = Vec::new();

    while let Some(arg) = args.first().cloned() {
        args.remove(0);
        match arg.as_str() {
            "-p" | "--print" => {}
            "--new" => intent = StartupIntent::New,
            "--continue" | "-c" => intent = StartupIntent::ContinueLatest,
            "--resume" | "-r" => {
                if let Some(id) = args.first().cloned()
                    && !id.starts_with('-')
                {
                    args.remove(0);
                    intent = StartupIntent::ResumeId(id);
                } else {
                    eprintln!(
                        "--resume needs an explicit session id in headless mode (the picker is interactive)"
                    );
                    std::process::exit(2);
                }
            }
            value if value.starts_with("--resume=") => {
                let id = value.trim_start_matches("--resume=");
                if id.is_empty() {
                    eprintln!(
                        "--resume needs an explicit session id in headless mode (the picker is interactive)"
                    );
                    std::process::exit(2);
                }
                intent = StartupIntent::ResumeId(id.to_string());
            }
            "--output-format" => {
                let value = require_value(&mut args, "--output-format");
                output = OutputFormat::parse(&value).unwrap_or_else(|| {
                    eprintln!(
                        "Invalid --output-format '{value}'. Use 'text', 'json', or 'stream-json'."
                    );
                    std::process::exit(2);
                });
            }
            "--permission-mode" => {
                let value = require_value(&mut args, "--permission-mode");
                permission_mode = Some(PermissionMode::parse(&value).unwrap_or_else(|| {
                    eprintln!(
                        "Invalid --permission-mode '{value}'. Use 'default', 'accept_edits', 'auto', or 'yolo'."
                    );
                    std::process::exit(2);
                }));
            }
            "--timeout" => {
                let value = require_value(&mut args, "--timeout");
                timeout_secs = Some(
                    value
                        .parse::<u64>()
                        .ok()
                        .filter(|secs| *secs > 0)
                        .unwrap_or_else(|| {
                            eprintln!("Invalid --timeout value '{value}' (whole seconds > 0)");
                            std::process::exit(2);
                        }),
                );
            }
            "--verbose" => verbose = true,
            "--add-dir" => {
                let value = require_value(&mut args, "--add-dir");
                push_add_dir(&mut add_dirs, &value);
            }
            value if value.starts_with("--add-dir=") => {
                push_add_dir(&mut add_dirs, value.trim_start_matches("--add-dir="));
            }
            other if !other.starts_with('-') => {
                if prompt.is_some() {
                    eprintln!("More than one prompt argument. Quote the prompt: -p \"…\"");
                    eprintln!("{usage}");
                    std::process::exit(2);
                }
                prompt = Some(other.to_string());
            }
            other => {
                eprintln!("Unknown print argument '{other}'.");
                eprintln!("{usage}");
                std::process::exit(2);
            }
        }
    }

    CliArgs {
        mode: RunMode::Print(PrintArgs {
            prompt,
            intent,
            output,
            permission_mode,
            timeout_secs,
            verbose,
            add_dirs,
        }),
    }
}

/// Resolve and collect one `--add-dir` grant. Canonicalized here — at the
/// moment the human states their intent — so every later layer (session
/// record, sandbox profile, system prompt) sees a single spelling, and a
/// bad path refuses the launch instead of surfacing later as a mid-task
/// tool denial the model cannot act on.
fn push_add_dir(add_dirs: &mut Vec<PathBuf>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        eprintln!("--add-dir needs a directory path");
        std::process::exit(2);
    }
    let canonical = match PathBuf::from(trimmed).canonicalize() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("--add-dir {trimmed} cannot be resolved: {error}");
            std::process::exit(2);
        }
    };
    if !canonical.is_dir() {
        eprintln!("--add-dir {trimmed} is not a directory");
        std::process::exit(2);
    }
    if !add_dirs.contains(&canonical) {
        add_dirs.push(canonical);
    }
}

/// Parse the TUI flags (`--new`, `-c`/`--continue`, `-r`/`--resume [id]`) into
/// a [`StartupIntent`]. Last intent wins; unknown args abort with usage.
fn parse_tui_args(args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let mut intent = StartupIntent::New;
    let mut add_dirs: Vec<PathBuf> = Vec::new();
    let mut positional = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--new" => intent = StartupIntent::New,
            "--continue" | "-c" => intent = StartupIntent::ContinueLatest,
            "--resume" | "-r" => {
                if index + 1 < args.len() && !args[index + 1].starts_with('-') {
                    index += 1;
                    intent = StartupIntent::ResumeId(args[index].clone());
                } else {
                    intent = StartupIntent::ResumePicker;
                }
            }
            value if value.starts_with("--resume=") => {
                let id = value.trim_start_matches("--resume=");
                intent = if id.is_empty() {
                    StartupIntent::ResumePicker
                } else {
                    StartupIntent::ResumeId(id.to_string())
                };
            }
            "--add-dir" => {
                if index + 1 < args.len() {
                    index += 1;
                    push_add_dir(&mut add_dirs, &args[index]);
                } else {
                    eprintln!("--add-dir needs a directory path");
                    std::process::exit(2);
                }
            }
            value if value.starts_with("--add-dir=") => {
                push_add_dir(&mut add_dirs, value.trim_start_matches("--add-dir="));
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    if !positional.is_empty() {
        eprintln!(
            "Unknown arguments: {}. Try `{prog} --help`.",
            positional.join(" ")
        );
        print_usage();
        std::process::exit(2);
    }

    CliArgs {
        mode: RunMode::Tui { intent, add_dirs },
    }
}

fn parse_doctor_command(mut args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let mut json = false;
    while let Some(arg) = args.first().cloned() {
        match arg.as_str() {
            "--json" => {
                json = true;
                args.remove(0);
            }
            other => {
                eprintln!("Unknown doctor argument '{other}'. Usage: {prog} doctor [--json]");
                std::process::exit(2);
            }
        }
    }
    CliArgs {
        mode: RunMode::Doctor { json },
    }
}

fn parse_serve_command(mut args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let mut http = false;
    let mut host = "127.0.0.1".to_string();
    let mut port = 7878u16;
    let mut auth_token = None;
    let mut resume = None;
    // None = not given on CLI → fall back to env below.
    let mut approval_mode: Option<bool> = None;
    let mut add_dirs: Vec<PathBuf> = Vec::new();

    while let Some(arg) = args.first().cloned() {
        match arg.as_str() {
            "--add-dir" => {
                args.remove(0);
                let value = require_value(&mut args, "--add-dir");
                push_add_dir(&mut add_dirs, &value);
            }
            // The `=` spelling too: tui and -p accept it, and a flag that
            // parses in two entry points but errors in the third reads like a
            // typo on the user's side rather than the truth (an omission here).
            value if value.starts_with("--add-dir=") => {
                args.remove(0);
                push_add_dir(&mut add_dirs, value.trim_start_matches("--add-dir="));
            }
            "--http" => {
                http = true;
                args.remove(0);
            }
            "--approval-mode" => {
                args.remove(0);
                let value = require_value(&mut args, "--approval-mode");
                approval_mode = Some(match value.as_str() {
                    "autonomous" => true,
                    "interactive" => false,
                    other => {
                        eprintln!(
                            "Invalid --approval-mode '{other}'. Use 'interactive' or 'autonomous'."
                        );
                        std::process::exit(2);
                    }
                });
            }
            "--host" => {
                args.remove(0);
                host = require_value(&mut args, "--host");
            }
            "--port" => {
                args.remove(0);
                port = require_value(&mut args, "--port")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("Invalid --port value");
                        std::process::exit(2);
                    });
            }
            "--auth-token" => {
                args.remove(0);
                auth_token = Some(require_value(&mut args, "--auth-token"));
            }
            "--resume" => {
                args.remove(0);
                resume = Some(require_value(&mut args, "--resume"));
            }
            other => {
                eprintln!(
                    "Unknown serve argument '{other}'. Usage: {prog} serve --http [--host HOST] [--port PORT] [--auth-token TOKEN] [--resume ID] [--approval-mode interactive|autonomous] [--add-dir DIR]..."
                );
                std::process::exit(2);
            }
        }
    }

    if !http {
        eprintln!("Usage: {prog} serve --http [--host 127.0.0.1] [--port 7878]");
        std::process::exit(2);
    }

    // CLI flag wins; otherwise DEEP_CODE_APPROVAL_MODE=autonomous; else interactive.
    let autonomous_approvals = approval_mode.unwrap_or_else(|| {
        std::env::var("DEEP_CODE_APPROVAL_MODE")
            .map(|value| value.eq_ignore_ascii_case("autonomous"))
            .unwrap_or(false)
    });

    CliArgs {
        mode: RunMode::Serve {
            host,
            port,
            auth_token,
            resume,
            autonomous_approvals,
            add_dirs,
        },
    }
}

fn parse_eval_command(mut args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let mut subset = "lite".to_string();
    // dev(23 题)是默认:便宜、适合联调,避免误触 300 题的 test 全量。
    let mut split = "dev".to_string();
    let mut sample = None;
    let mut parallel = 1;
    let mut json = false;
    let mut markdown = false;
    let mut timeout_secs = 300;
    let mut out_dir = PathBuf::from("eval-out");

    while let Some(arg) = args.first().cloned() {
        match arg.as_str() {
            "--subset" => {
                args.remove(0);
                subset = require_value(&mut args, "--subset");
            }
            "--split" => {
                args.remove(0);
                split = require_value(&mut args, "--split");
            }
            "--out" => {
                args.remove(0);
                out_dir = PathBuf::from(require_value(&mut args, "--out"));
            }
            "--sample" => {
                args.remove(0);
                sample = Some(
                    require_value(&mut args, "--sample")
                        .parse()
                        .unwrap_or_else(|_| {
                            eprintln!("Invalid --sample value");
                            std::process::exit(2);
                        }),
                );
            }
            "--parallel" => {
                args.remove(0);
                parallel = require_value(&mut args, "--parallel")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("Invalid --parallel value");
                        std::process::exit(2);
                    });
            }
            "--json" => {
                json = true;
                args.remove(0);
            }
            "--markdown" | "--to-markdown" => {
                markdown = true;
                args.remove(0);
            }
            "--timeout" => {
                args.remove(0);
                timeout_secs = require_value(&mut args, "--timeout")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("Invalid --timeout value");
                        std::process::exit(2);
                    });
            }
            other => {
                eprintln!(
                    "Unknown eval argument '{other}'. Usage: {prog} eval \
[--subset lite|verified] [--split dev|test] [--sample N] [--parallel N] \
[--json] [--markdown] [--timeout SECS] [--out DIR]"
                );
                std::process::exit(2);
            }
        }
    }

    CliArgs {
        mode: RunMode::Eval {
            subset,
            split,
            sample,
            parallel,
            json,
            markdown,
            timeout_secs,
            out_dir,
        },
    }
}

/// Parse `github install [flags]` / `github status`.
fn parse_github_command(mut args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let usage = format!(
        "Usage: {prog} github install [--with-app] [--app-id ID --app-private-key FILE] \
[--ref REF] [--lang zh|en] [--permission-mode MODE] [--path FILE] [--force] [--print] \
[--skip-secrets]\n       {prog} github status"
    );

    let Some(subcommand) = args.first().cloned() else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    args.remove(0);

    match subcommand.as_str() {
        "status" => {
            if !args.is_empty() {
                eprintln!("Usage: {prog} github status");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::Github(GithubCommand::Status),
            }
        }
        "install" => {
            let mut install = InstallArgs::default();
            while let Some(arg) = args.first().cloned() {
                args.remove(0);
                match arg.as_str() {
                    "--with-app" => install.with_app = true,
                    "--force" => install.force = true,
                    "--print" | "--dry-run" => install.print_only = true,
                    "--skip-secrets" => install.skip_secrets = true,
                    "--app-id" => install.app_id = Some(require_value(&mut args, "--app-id")),
                    "--app-private-key" => {
                        install.app_key_file =
                            Some(PathBuf::from(require_value(&mut args, "--app-private-key")));
                    }
                    "--ref" => install.workflow_ref = Some(require_value(&mut args, "--ref")),
                    "--lang" => install.lang = Some(require_value(&mut args, "--lang")),
                    "--permission-mode" => {
                        install.permission_mode =
                            Some(require_value(&mut args, "--permission-mode"));
                    }
                    "--path" => {
                        install.path = Some(PathBuf::from(require_value(&mut args, "--path")))
                    }
                    "--api-key" => install.api_key = Some(require_value(&mut args, "--api-key")),
                    other => {
                        eprintln!("Unknown install argument '{other}'.");
                        eprintln!("{usage}");
                        std::process::exit(2);
                    }
                }
            }
            CliArgs {
                mode: RunMode::Github(GithubCommand::Install(install)),
            }
        }
        other => {
            eprintln!("Unknown github subcommand '{other}'. Use install or status.");
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}

fn require_value(args: &mut Vec<String>, flag: &str) -> String {
    if args.is_empty() {
        eprintln!("Missing value for {flag}");
        std::process::exit(2);
    }
    args.remove(0)
}

fn parse_session_command(mut args: Vec<String>) -> CliArgs {
    let prog = program_name();
    let Some(subcommand) = args.first().cloned() else {
        eprintln!("Usage: {prog} session <list|resume|delete|export> [id]");
        print_session_usage();
        std::process::exit(2);
    };
    args.remove(0);

    match subcommand.as_str() {
        "list" => {
            if !args.is_empty() {
                eprintln!("Usage: {prog} session list");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::SessionList,
            }
        }
        "resume" => {
            let Some(id) = args.first().cloned() else {
                eprintln!("Usage: {prog} session resume <id>");
                std::process::exit(2);
            };
            if args.len() > 1 {
                eprintln!("Usage: {prog} session resume <id>");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::Tui {
                    intent: StartupIntent::ResumeId(id),
                    add_dirs: Vec::new(),
                },
            }
        }
        "delete" => {
            let Some(id) = args.first().cloned() else {
                eprintln!("Usage: {prog} session delete <id>");
                std::process::exit(2);
            };
            if args.len() > 1 {
                eprintln!("Usage: {prog} session delete <id>");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::SessionDelete { id },
            }
        }
        "export" => {
            let Some(id) = args.first().cloned() else {
                eprintln!("Usage: {prog} session export <id>");
                std::process::exit(2);
            };
            if args.len() > 1 {
                eprintln!("Usage: {prog} session export <id>");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::SessionExport { id },
            }
        }
        other => {
            eprintln!("Unknown session subcommand '{other}'. Use list, resume, delete, or export.");
            std::process::exit(2);
        }
    }
}

pub fn workspace_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|error| {
        eprintln!("failed to resolve workspace: {error}");
        std::process::exit(1);
    })
}

pub fn open_session_store() -> JsonSessionStore {
    match JsonSessionStore::for_workspace(workspace_root()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("session storage unavailable: {error}");
            std::process::exit(1);
        }
    }
}

pub fn run_session_command(mode: RunMode) -> anyhow::Result<()> {
    match mode {
        RunMode::SessionList => {
            let workspace = workspace_root();
            let store = open_session_store();
            println!("# {}", format_sessions_storage_note(&workspace));
            let records = store.list()?;
            if records.is_empty() {
                println!("No saved sessions.");
                return Ok(());
            }
            let lang = Lang::from_env(&AgentConfig::load(&workspace).config.language);
            let now = now_ms();
            for record in records {
                let preview = session_list_preview(&record.preview());
                println!(
                    "{}\t{}\t{} msgs\t{}",
                    record.id.as_str(),
                    crate::startup::relative_time(now, record.updated_at_ms, lang),
                    record.message_count(),
                    preview
                );
            }
        }
        RunMode::SessionDelete { id } => {
            let store = open_session_store();
            store.delete(&SessionId::parse(&id)?)?;
            println!("Deleted session {id}.");
        }
        RunMode::SessionExport { id } => {
            let store = open_session_store();
            println!("{}", store.export(&SessionId::parse(&id)?)?);
        }
        RunMode::Tui { .. }
        | RunMode::Print(_)
        | RunMode::Github(_)
        | RunMode::Doctor { .. }
        | RunMode::Serve { .. }
        | RunMode::Eval { .. } => unreachable!("handled by caller"),
    }
    Ok(())
}

/// The name the user actually invoked us by.
///
/// Distribution makes this necessary: the npm package installs the binary as
/// `deepcode`, while `cargo build` produces `deep-code`. Hardcoding either one
/// tells half the users to run a command that does not exist on their machine.
pub(crate) fn program_name() -> String {
    env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "deepcode".to_string())
}

fn usage_text() -> String {
    let prog = program_name();
    [
        "Commands:".to_string(),
        format!("  {prog}                # 新会话"),
        format!("  {prog} -c             # 续最近会话"),
        format!("  {prog} -r             # 选择历史会话"),
        format!(
            "  {prog} --add-dir DIR  # 额外可写目录(可重复;对 -p/serve 同样可用,随会话保存)"
        ),
        format!("  {prog} -p \"PROMPT\"    # 单发无头模式;无参数则读 stdin,可与 -c/--resume 组合"),
        format!(
            "  {prog} -p [PROMPT] [--output-format text|json|stream-json] [--permission-mode MODE] [--timeout SECS] [--verbose]"
        ),
        format!("  {prog} github install [--with-app]   # 给当前仓库装上 CI bot(--print 预览)"),
        format!("  {prog} github status                 # 查看接入状态"),
        format!("  {prog} doctor [--json]"),
        format!("  {prog} serve --http [--host HOST] [--port PORT]"),
        format!("  {prog} session list|resume|delete|export"),
        format!("  {prog} eval [--subset lite] [--sample N] [--parallel N] [--json] [--markdown]"),
        String::new(),
        format!("  {prog} --help | --version"),
    ]
    .join("\n")
}

/// Usage on the error path: stderr, because the caller exits non-zero.
fn print_usage() {
    eprintln!("{}", usage_text());
}

fn print_session_usage() {
    let prog = program_name();
    let workspace = workspace_root();
    eprintln!("{}", format_sessions_storage_note(&workspace));
    eprintln!("Examples:");
    eprintln!("  {prog} session list");
    eprintln!("  {prog} session resume <session_id>");
    eprintln!("  {prog} session export <session_id>");
    eprintln!("  {prog} -c            # 续最近会话");
    eprintln!("  {prog} -r            # 选择历史会话");
}

/// One `session list` preview column.
///
/// `SessionRecord::preview()` returns the last user entry VERBATIM out of
/// `<workspace>/.deep-code/sessions/*.json`, a file `workspace_policy` itself
/// documents as "an ordinary `write_file` target for the model". Collapsing
/// newlines and capping the length — all this used to do — touches neither
/// `\x1b` nor the invisible families, so a planted session could repaint the
/// terminal from a plain `deepcode session list`.
///
/// This is the third twin of the two resume pickers hardened in 806ee49 and
/// 6a08a86, and the only one of the three with no rendered-cell test, because
/// it prints rather than draws. Sanitize BEFORE collapsing and truncating: the
/// cap counts characters, and dropping the invisibles first keeps that count
/// describing what is actually shown.
fn session_list_preview(preview: &str) -> String {
    crate::history::truncate_chars(
        &deep_code_agent::neutralize_display_text(preview).replace('\n', " "),
        60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_preview_neutralizes_a_planted_session() {
        let line = session_list_preview("hi\u{1b}[2J\u{1b}[H FAKE\u{202e}x\u{2028}y");

        assert!(
            !line.chars().any(char::is_control),
            "an escape reached stdout: {line:?}"
        );
        assert!(
            !line.contains('\u{202e}') && !line.contains('\u{2028}'),
            "an invisible code point reached stdout: {line:?}"
        );
        assert!(line.starts_with("hi"), "the text must survive: {line:?}");
        assert!(line.contains('y'), "the text must survive: {line:?}");
    }

    #[test]
    fn session_list_preview_flattens_and_caps() {
        let line = session_list_preview(&format!("a\nb{}", "z".repeat(200)));

        assert!(!line.contains('\n'), "newlines must be collapsed: {line:?}");
        assert!(line.starts_with("a b"), "the head must survive: {line:?}");
        assert!(
            line.ends_with(" (truncated)"),
            "over-long previews must say so: {line:?}"
        );
        assert_eq!(
            line.chars().count(),
            60 + " (truncated)".chars().count(),
            "the cap counts characters of the sanitized text: {line:?}"
        );
    }

    #[test]
    fn parse_session_resume_subcommand() {
        let parsed = parse_session_command(vec!["resume".to_string(), "session_123_0".to_string()]);
        assert_eq!(
            parsed.mode,
            RunMode::Tui {
                intent: StartupIntent::ResumeId("session_123_0".to_string()),
                add_dirs: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_session_list_subcommand() {
        let parsed = parse_session_command(vec!["list".to_string()]);
        assert_eq!(parsed.mode, RunMode::SessionList);
    }

    fn tui_intent(args: &[&str]) -> StartupIntent {
        let parsed = parse_tui_args(args.iter().map(|s| (*s).to_string()).collect());
        match parsed.mode {
            RunMode::Tui { intent, .. } => intent,
            other => panic!("expected Tui, got {other:?}"),
        }
    }

    #[test]
    fn add_dir_is_repeatable_deduped_and_canonical() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_arg = first.path().to_string_lossy().into_owned();
        let second_arg = second.path().to_string_lossy().into_owned();
        let parsed = parse_tui_args(vec![
            "--add-dir".to_string(),
            first_arg.clone(),
            format!("--add-dir={second_arg}"),
            "--add-dir".to_string(),
            first_arg,
        ]);
        match parsed.mode {
            RunMode::Tui { add_dirs, .. } => {
                assert_eq!(
                    add_dirs,
                    vec![
                        first.path().canonicalize().unwrap(),
                        second.path().canonicalize().unwrap(),
                    ],
                    "repeats dedupe, both spellings parse, values canonicalize"
                );
            }
            other => panic!("expected Tui, got {other:?}"),
        }
    }

    #[test]
    fn print_args_carry_add_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let parsed = parse_print_args(vec![
            "-p".to_string(),
            "hello".to_string(),
            "--add-dir".to_string(),
            dir.path().to_string_lossy().into_owned(),
        ]);
        match parsed.mode {
            RunMode::Print(print_args) => {
                assert_eq!(
                    print_args.add_dirs,
                    vec![dir.path().canonicalize().unwrap()]
                );
                assert_eq!(print_args.prompt.as_deref(), Some("hello"));
            }
            other => panic!("expected Print, got {other:?}"),
        }
    }

    #[test]
    fn serve_accepts_add_dir_in_both_spellings() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let parsed = parse_serve_command(vec![
            "--http".to_string(),
            "--add-dir".to_string(),
            first.path().to_string_lossy().into_owned(),
            format!("--add-dir={}", second.path().to_string_lossy()),
        ]);
        match parsed.mode {
            RunMode::Serve { add_dirs, .. } => assert_eq!(
                add_dirs,
                vec![
                    first.path().canonicalize().unwrap(),
                    second.path().canonicalize().unwrap(),
                ],
                "serve takes the same two spellings as tui/-p"
            ),
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn tui_flags_map_to_startup_intent() {
        assert_eq!(tui_intent(&[]), StartupIntent::New);
        assert_eq!(tui_intent(&["--new"]), StartupIntent::New);
        assert_eq!(tui_intent(&["-c"]), StartupIntent::ContinueLatest);
        assert_eq!(tui_intent(&["--continue"]), StartupIntent::ContinueLatest);
        assert_eq!(tui_intent(&["-r"]), StartupIntent::ResumePicker);
        assert_eq!(tui_intent(&["--resume"]), StartupIntent::ResumePicker);
        assert_eq!(
            tui_intent(&["--resume", "session_9_0"]),
            StartupIntent::ResumeId("session_9_0".to_string())
        );
        assert_eq!(
            tui_intent(&["--resume=session_9_0"]),
            StartupIntent::ResumeId("session_9_0".to_string())
        );
        assert_eq!(tui_intent(&["--resume="]), StartupIntent::ResumePicker);
    }

    #[test]
    fn usage_names_the_invoked_binary_not_a_hardcoded_one() {
        let text = usage_text();
        // npm installs the binary as `deepcode`, `cargo build` produces
        // `deep-code`. Any hardcoded spelling sends half the users to a command
        // that does not exist for them, so usage must interpolate argv[0].
        assert!(
            !text.contains("deep-code"),
            "usage must not hardcode a binary name: {text}"
        );
        assert!(text.contains(&program_name()));
        assert!(text.contains("--help"), "help must advertise itself");
    }

    #[test]
    fn program_name_falls_back_when_argv0_is_unusable() {
        // Only the fallback is assertable here: argv[0] of the test harness is
        // the test binary, so the happy path is covered by the test above.
        assert!(!program_name().is_empty());
    }

    #[test]
    fn parse_doctor_json_flag() {
        let parsed = parse_doctor_command(vec!["--json".to_string()]);
        assert_eq!(parsed.mode, RunMode::Doctor { json: true });
    }

    fn print_args(args: &[&str]) -> PrintArgs {
        let parsed = parse_print_args(args.iter().map(|s| (*s).to_string()).collect());
        match parsed.mode {
            RunMode::Print(print) => print,
            other => panic!("expected Print, got {other:?}"),
        }
    }

    #[test]
    fn print_defaults_are_new_session_text_output() {
        assert_eq!(
            print_args(&["-p"]),
            PrintArgs {
                prompt: None,
                intent: StartupIntent::New,
                output: OutputFormat::Text,
                permission_mode: None,
                timeout_secs: None,
                verbose: false,
                add_dirs: Vec::new(),
            }
        );
    }

    #[test]
    fn print_prompt_is_positional_on_either_side_of_the_flag() {
        assert_eq!(
            print_args(&["-p", "fix the bug"]).prompt.as_deref(),
            Some("fix the bug")
        );
        assert_eq!(
            print_args(&["fix the bug", "--print"]).prompt.as_deref(),
            Some("fix the bug")
        );
    }

    #[test]
    fn print_full_flag_set_parses() {
        let print = print_args(&[
            "-p",
            "do it",
            "--output-format",
            "json",
            "--permission-mode",
            "accept_edits",
            "--timeout",
            "60",
            "--verbose",
            "-c",
        ]);
        assert_eq!(print.prompt.as_deref(), Some("do it"));
        assert_eq!(print.intent, StartupIntent::ContinueLatest);
        assert_eq!(print.output, OutputFormat::Json);
        assert_eq!(print.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(print.timeout_secs, Some(60));
        assert!(print.verbose);
    }

    #[test]
    fn print_resume_takes_an_explicit_id() {
        assert_eq!(
            print_args(&["-p", "go", "--resume", "session_9_0"]).intent,
            StartupIntent::ResumeId("session_9_0".to_string())
        );
        assert_eq!(
            print_args(&["-p", "go", "--resume=session_9_0"]).intent,
            StartupIntent::ResumeId("session_9_0".to_string())
        );
    }

    /// `-p` must win the routing wherever it appears among TUI-style flags,
    /// while never leaking into real subcommands (those return before the
    /// print check in `parse_args`).
    #[test]
    fn print_mode_is_detected_in_any_position() {
        assert!(wants_print(&argv(&["-p"])));
        assert!(wants_print(&argv(&["-c", "--print"])));
        assert!(wants_print(&argv(&["fix it", "-p", "--verbose"])));
        assert!(!wants_print(&argv(&["-c"])));
        assert!(!wants_print(&argv(&["session", "list"])));
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    /// `--help` past the first position used to fall into each subcommand's own
    /// unknown-argument branch — usage printed to *stderr*, exit 2 — which is the
    /// same defect that was fixed for the bare `--help` but only at the top level.
    #[test]
    fn help_is_recognized_in_any_position() {
        let argv = |args: &[&str]| args.iter().map(|a| (*a).to_string()).collect::<Vec<_>>();

        for args in [
            vec!["--help"],
            vec!["-h"],
            vec!["doctor", "--help"],
            vec!["serve", "--help"],
            vec!["session", "--help"],
            vec!["session", "list", "--help"],
            vec!["-c", "--help"],
            vec!["eval", "--subset", "lite", "--help"],
        ] {
            assert!(wants_help(&argv(&args)), "{args:?} must ask for help");
        }

        for args in [
            vec!["doctor"],
            vec!["doctor", "--json"],
            vec!["serve", "--http", "--port", "8080"],
            vec!["session", "list"],
            vec!["-c"],
            // A value that merely contains the word must not count.
            vec!["session", "resume", "help-me"],
        ] {
            assert!(!wants_help(&argv(&args)), "{args:?} must not ask for help");
        }
    }
}
