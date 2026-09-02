//! Execution of the `session list|delete|export` subcommands.
//!
//! Kept out of `cli` (which is pure argument parsing) and out of `main`'s
//! dispatch, mirroring `doctor_cli`/`eval_cli`: parsing decides *which* command,
//! this runs it. `main` dispatches each `RunMode::Session*` variant to the
//! matching function here, so there is no catch-all `unreachable!` bridging two
//! modules.

use deep_code_agent::{
    AgentConfig, JsonSessionStore, Lang, SessionId, SessionStore, format_sessions_storage_note,
    now_ms,
};

use crate::cli::workspace_root;

fn open_session_store() -> JsonSessionStore {
    match JsonSessionStore::for_workspace(workspace_root()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("session storage unavailable: {error}");
            std::process::exit(1);
        }
    }
}

pub fn list() -> anyhow::Result<()> {
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
    Ok(())
}

pub fn delete(id: String) -> anyhow::Result<()> {
    let store = open_session_store();
    store.delete(&SessionId::parse(&id)?)?;
    println!("Deleted session {id}.");
    Ok(())
}

pub fn export(id: String) -> anyhow::Result<()> {
    let store = open_session_store();
    println!("{}", store.export(&SessionId::parse(&id)?)?);
    Ok(())
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
}
