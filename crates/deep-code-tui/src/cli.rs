//! CLI argument parsing for the `deep-code` binary.

use std::env;
use std::path::PathBuf;

use deep_code_agent::{format_sessions_storage_note, JsonSessionStore, SessionId, SessionStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    Tui {
        resume: Option<String>,
        force_new: bool,
    },
    SessionList,
    SessionDelete {
        id: String,
    },
    SessionExport {
        id: String,
    },
    Mcp {
        subcommand: String,
        args: Vec<String>,
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
                resume: None,
                force_new: false,
            },
        };
    }

    if args[0] == "session" {
        args.remove(0);
        return parse_session_command(args);
    }

    if args[0] == "mcp" {
        args.remove(0);
        return parse_mcp_command(args);
    }

    let mut resume = None;
    let mut force_new = false;
    let mut positional = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--new" => force_new = true,
            "--resume" => {
                index += 1;
                if index < args.len() && !args[index].starts_with('-') {
                    resume = Some(args[index].clone());
                } else {
                    resume = Some("latest".to_string());
                    index -= 1;
                }
            }
            value if value.starts_with("--resume=") => {
                let id = value.trim_start_matches("--resume=");
                resume = Some(if id.is_empty() { "latest".to_string() } else { id.to_string() });
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    if !positional.is_empty() {
        eprintln!(
            "Unknown arguments: {}. Try `deep-code session list`.",
            positional.join(" ")
        );
        print_session_usage();
        std::process::exit(2);
    }

    CliArgs {
        mode: RunMode::Tui {
            resume,
            force_new,
        },
    }
}

fn parse_mcp_command(mut args: Vec<String>) -> CliArgs {
    let Some(subcommand) = args.first().cloned() else {
        eprintln!("Usage: deep-code mcp <list|enable|disable|validate|reload> [server]");
        std::process::exit(2);
    };
    args.remove(0);
    CliArgs {
        mode: RunMode::Mcp {
            subcommand,
            args,
        },
    }
}

fn parse_session_command(mut args: Vec<String>) -> CliArgs {
    let Some(subcommand) = args.first().cloned() else {
        eprintln!("Usage: deep-code session <list|delete|export> [id]");
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
            eprintln!("Unknown session subcommand '{other}'. Use list, delete, or export.");
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
            for record in records {
                let preview = truncate_preview(&record.preview(), 60);
                println!(
                    "{}\t{}\t{} msgs\t{}",
                    record.id.as_str(),
                    format_timestamp(record.updated_at_ms),
                    record.messages.len(),
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
        RunMode::Tui { .. } => unreachable!("TUI mode handled by caller"),
        RunMode::Mcp { .. } => unreachable!("MCP mode handled by caller"),
    }
    Ok(())
}

fn print_session_usage() {
    let workspace = workspace_root();
    eprintln!("{}", format_sessions_storage_note(&workspace));
    eprintln!("Examples:");
    eprintln!("  deep-code session list");
    eprintln!("  deep-code session export <session_id>");
    eprintln!("  deep-code --resume latest");
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return text.replace('\n', " ");
        };
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push_str("...");
    }
    out.replace('\n', " ")
}

fn format_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let sub = ms % 1000;
    format!("{secs}.{sub:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_is_tui() {
        let args = CliArgs {
            mode: RunMode::Tui {
                resume: None,
                force_new: false,
            },
        };
        assert_eq!(
            args.mode,
            RunMode::Tui {
                resume: None,
                force_new: false,
            }
        );
    }

    #[test]
    fn parse_session_list_subcommand() {
        let parsed = parse_session_command(vec!["list".to_string()]);
        assert_eq!(parsed.mode, RunMode::SessionList);
    }
}
