//! Prompt assembly for headless runs.
//!
//! Two sources feed one submitted prompt: the positional argument (the
//! instruction) and piped stdin (the data — a diff, a log, a file). Keeping
//! them distinguishable in the composed text matters: pasting a log *as* the
//! instruction invites the model to obey text that was meant as evidence.

use std::io::{IsTerminal, Read};

/// Marker inserted between the instruction and piped data when both are
/// present. Part of the prompt users see in the transcript — change wording,
/// not meaning.
const STDIN_MARKER: &str = "--- stdin ---";

/// Read piped stdin, if any. A TTY stdin means "nothing piped": reading it
/// would block on the keyboard, which a headless run must never do. Bytes are
/// converted lossily — logs are routinely not valid UTF-8, and refusing the
/// whole run over one byte helps nobody.
pub(crate) fn read_piped_stdin() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut bytes = Vec::new();
    stdin.lock().read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Merge the positional prompt and piped stdin into the submitted prompt.
/// `None` means "nothing to run" — the caller turns that into a usage error.
pub(crate) fn compose_prompt(positional: Option<&str>, piped: Option<&str>) -> Option<String> {
    let instruction = positional.map(str::trim).filter(|text| !text.is_empty());
    let data = piped.filter(|text| !text.trim().is_empty());
    match (instruction, data) {
        (Some(instruction), Some(data)) => Some(format!("{instruction}\n\n{STDIN_MARKER}\n{data}")),
        (Some(instruction), None) => Some(instruction.to_string()),
        (None, Some(data)) => Some(data.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_alone_is_the_prompt() {
        assert_eq!(
            compose_prompt(Some("  fix the bug  "), None).as_deref(),
            Some("fix the bug")
        );
    }

    #[test]
    fn piped_stdin_alone_is_the_prompt_verbatim() {
        // Data content is not trimmed: leading/trailing whitespace can be
        // meaningful in logs and patches.
        assert_eq!(
            compose_prompt(None, Some("line 1\nline 2\n")).as_deref(),
            Some("line 1\nline 2\n")
        );
    }

    #[test]
    fn both_present_keeps_instruction_and_data_separated() {
        let composed = compose_prompt(Some("explain this"), Some("panic at main.rs:1")).unwrap();
        assert_eq!(
            composed,
            "explain this\n\n--- stdin ---\npanic at main.rs:1"
        );
    }

    #[test]
    fn blank_sources_yield_none() {
        assert_eq!(compose_prompt(None, None), None);
        assert_eq!(compose_prompt(Some("   "), Some(" \n ")), None);
    }
}
