//! CLI argument parsing for the `deep-code` binary.

use std::env;
use std::path::PathBuf;

use deep_code_agent::{
    AgentConfig, JsonSessionStore, Lang, SessionId, SessionStore, format_sessions_storage_note,
    now_ms,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    Tui {
        intent: StartupIntent,
    },
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

pub fn parse_args() -> CliArgs {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return CliArgs {
            mode: RunMode::Tui {
                intent: StartupIntent::New,
            },
        };
    }

    match args[0].as_str() {
        "--version" | "-V" => {
            println!("deep-code {}", env!("CARGO_PKG_VERSION"));
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
        _ => {}
    }

    parse_tui_args(args)
}

/// Parse the TUI flags (`--new`, `-c`/`--continue`, `-r`/`--resume [id]`) into
/// a [`StartupIntent`]. Last intent wins; unknown args abort with usage.
fn parse_tui_args(args: Vec<String>) -> CliArgs {
    let mut intent = StartupIntent::New;
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
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    if !positional.is_empty() {
        eprintln!(
            "Unknown arguments: {}. Try `deep-code doctor` or `deep-code serve --http`.",
            positional.join(" ")
        );
        print_usage();
        std::process::exit(2);
    }

    CliArgs {
        mode: RunMode::Tui { intent },
    }
}

fn parse_doctor_command(mut args: Vec<String>) -> CliArgs {
    let mut json = false;
    while let Some(arg) = args.first().cloned() {
        match arg.as_str() {
            "--json" => {
                json = true;
                args.remove(0);
            }
            other => {
                eprintln!("Unknown doctor argument '{other}'. Usage: deep-code doctor [--json]");
                std::process::exit(2);
            }
        }
    }
    CliArgs {
        mode: RunMode::Doctor { json },
    }
}

fn parse_serve_command(mut args: Vec<String>) -> CliArgs {
    let mut http = false;
    let mut host = "127.0.0.1".to_string();
    let mut port = 7878u16;
    let mut auth_token = None;
    let mut resume = None;
    // None = not given on CLI → fall back to env below.
    let mut approval_mode: Option<bool> = None;

    while let Some(arg) = args.first().cloned() {
        match arg.as_str() {
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
                    "Unknown serve argument '{other}'. Usage: deep-code serve --http [--host HOST] [--port PORT] [--auth-token TOKEN] [--resume ID] [--approval-mode interactive|autonomous]"
                );
                std::process::exit(2);
            }
        }
    }

    if !http {
        eprintln!("Usage: deep-code serve --http [--host 127.0.0.1] [--port 7878]");
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
        },
    }
}

fn parse_eval_command(mut args: Vec<String>) -> CliArgs {
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
                    "Unknown eval argument '{other}'. Usage: deep-code eval \
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

fn require_value(args: &mut Vec<String>, flag: &str) -> String {
    if args.is_empty() {
        eprintln!("Missing value for {flag}");
        std::process::exit(2);
    }
    args.remove(0)
}

fn parse_session_command(mut args: Vec<String>) -> CliArgs {
    let Some(subcommand) = args.first().cloned() else {
        eprintln!("Usage: deep-code session <list|resume|delete|export> [id]");
        print_session_usage();
        std::process::exit(2);
    };
    args.remove(0);

    match subcommand.as_str() {
        "list" => {
            if !args.is_empty() {
                eprintln!("Usage: deep-code session list");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::SessionList,
            }
        }
        "resume" => {
            let Some(id) = args.first().cloned() else {
                eprintln!("Usage: deep-code session resume <id>");
                std::process::exit(2);
            };
            if args.len() > 1 {
                eprintln!("Usage: deep-code session resume <id>");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::Tui {
                    intent: StartupIntent::ResumeId(id),
                },
            }
        }
        "delete" => {
            let Some(id) = args.first().cloned() else {
                eprintln!("Usage: deep-code session delete <id>");
                std::process::exit(2);
            };
            if args.len() > 1 {
                eprintln!("Usage: deep-code session delete <id>");
                std::process::exit(2);
            }
            CliArgs {
                mode: RunMode::SessionDelete { id },
            }
        }
        "export" => {
            let Some(id) = args.first().cloned() else {
                eprintln!("Usage: deep-code session export <id>");
                std::process::exit(2);
            };
            if args.len() > 1 {
                eprintln!("Usage: deep-code session export <id>");
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
                let preview =
                    crate::history::truncate_chars(&record.preview().replace('\n', " "), 60);
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
        | RunMode::Doctor { .. }
        | RunMode::Serve { .. }
        | RunMode::Eval { .. } => unreachable!("handled by caller"),
    }
    Ok(())
}

fn print_usage() {
    eprintln!("Commands:");
    eprintln!("  deep-code                # 新会话");
    eprintln!("  deep-code -c             # 续最近会话");
    eprintln!("  deep-code -r             # 选择历史会话");
    eprintln!("  deep-code doctor [--json]");
    eprintln!("  deep-code serve --http [--host HOST] [--port PORT]");
    eprintln!("  deep-code session list|resume|delete|export");
    eprintln!("  deep-code eval [--subset lite] [--sample N] [--parallel N] [--json] [--markdown]");
}

fn print_session_usage() {
    let workspace = workspace_root();
    eprintln!("{}", format_sessions_storage_note(&workspace));
    eprintln!("Examples:");
    eprintln!("  deep-code session list");
    eprintln!("  deep-code session resume <session_id>");
    eprintln!("  deep-code session export <session_id>");
    eprintln!("  deep-code -c            # 续最近会话");
    eprintln!("  deep-code -r            # 选择历史会话");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_resume_subcommand() {
        let parsed = parse_session_command(vec!["resume".to_string(), "session_123_0".to_string()]);
        assert_eq!(
            parsed.mode,
            RunMode::Tui {
                intent: StartupIntent::ResumeId("session_123_0".to_string()),
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
            RunMode::Tui { intent } => intent,
            other => panic!("expected Tui, got {other:?}"),
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
    fn parse_doctor_json_flag() {
        let parsed = parse_doctor_command(vec!["--json".to_string()]);
        assert_eq!(parsed.mode, RunMode::Doctor { json: true });
    }
}
