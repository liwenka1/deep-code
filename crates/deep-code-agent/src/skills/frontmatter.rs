//! Metadata-block reader for `SKILL.md` files.
//!
//! Every skill file opens with a fenced metadata block followed by free-form
//! instructions:
//!
//! ```text
//! ---
//! name: fix-lints
//! description: Run the linter and repair what it flags.
//! ---
//! Step-by-step instructions live here.
//! ```
//!
//! This module deliberately implements the smallest grammar that covers real
//! skill files rather than pulling in a YAML engine: scalar `key: value`
//! lines, optional single/double quoting, `#` comments, and blank lines.
//! Unknown keys are ignored so authors can carry extra metadata without
//! breaking discovery.

/// The fields deep-code extracts from one `SKILL.md`, plus its instruction
/// body (everything after the closing fence, trimmed).
#[derive(Debug)]
pub(crate) struct SkillFile {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Parse the raw text of a `SKILL.md`.
///
/// Errors are plain prose because they surface verbatim in user-facing load
/// warnings.
pub(crate) fn read_skill_file(text: &str) -> Result<SkillFile, String> {
    let (meta, body) = split_meta_block(text)?;

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    for line in meta.lines() {
        let Some((key, value)) = scalar_field(line) else {
            continue;
        };
        match key.as_str() {
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            _ => {}
        }
    }

    let name = name
        .filter(|candidate| !candidate.is_empty())
        .ok_or_else(|| "metadata block does not define a non-empty `name`".to_owned())?;

    Ok(SkillFile {
        name,
        description: description.unwrap_or_default(),
        body: body.trim().to_owned(),
    })
}

/// Split a document into its metadata block and the body that follows.
///
/// The opening fence must be the first non-whitespace content; the closing
/// fence is the next line that begins with `---`. Content on the opening
/// fence line itself (`--- name: fix-lints`) is kept as the first metadata
/// line rather than discarded — hand-written files do this in the wild, and
/// silently dropping the line makes a skill vanish with a confusing
/// "no `name`" warning.
fn split_meta_block(text: &str) -> Result<(&str, &str), String> {
    let doc = text.trim_start();
    let Some(opened) = doc.strip_prefix("---") else {
        return Err("file must open with a `---` metadata fence".to_owned());
    };

    let mut cursor = opened;
    let mut on_fence_line = true;
    while !cursor.is_empty() {
        let (line, rest) = cursor.split_once('\n').unwrap_or((cursor, ""));
        if !on_fence_line && line.trim_start().starts_with("---") {
            let meta_len = opened.len() - cursor.len();
            return Ok((&opened[..meta_len], rest));
        }
        on_fence_line = false;
        cursor = rest;
    }
    Err("metadata fence is never closed".to_owned())
}

/// Interpret one metadata line as a `key: value` scalar.
///
/// Returns `None` for blank lines, `#` comments, and lines with no colon.
/// Keys are lowercased so `Name:` and `name:` behave identically; values
/// lose one matching layer of surrounding quotes.
fn scalar_field(raw: &str) -> Option<(String, &str)> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    Some((key.trim().to_ascii_lowercase(), unquote(value.trim())))
}

/// Strip one pair of matching `"` or `'` quotes, if present.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    let wrapped = bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0];
    if wrapped {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_description_and_body() {
        let text =
            "---\nname: git-bisect\ndescription: Narrow a regression down.\n---\nRun bisect.\n";
        let parsed = read_skill_file(text).expect("well-formed file parses");
        assert_eq!(parsed.name, "git-bisect");
        assert_eq!(parsed.description, "Narrow a regression down.");
        assert_eq!(parsed.body, "Run bisect.");
    }

    #[test]
    fn unwraps_quoted_values_and_lowercases_keys() {
        let text = "---\nName: \"quoted-name\"\nDESCRIPTION: 'single quoted'\n---\nbody";
        let parsed = read_skill_file(text).expect("quoted fields parse");
        assert_eq!(parsed.name, "quoted-name");
        assert_eq!(parsed.description, "single quoted");
    }

    #[test]
    fn ignores_comments_blanks_and_unknown_keys() {
        let text = "---\n# a comment\n\nname: keeper\nauthor: someone\n---\nbody";
        let parsed = read_skill_file(text).expect("noise lines are skipped");
        assert_eq!(parsed.name, "keeper");
        assert_eq!(parsed.description, "");
    }

    #[test]
    fn keeps_fields_written_on_the_opening_fence_line() {
        let text = "--- name: fence-rider\ndescription: on the fence\n---\nbody";
        let parsed = read_skill_file(text).expect("fence-line field parses");
        assert_eq!(parsed.name, "fence-rider");
        assert_eq!(parsed.description, "on the fence");
    }

    #[test]
    fn tolerates_leading_whitespace_before_the_fence() {
        let text = "\n\n  ---\nname: padded\n---\nbody";
        assert_eq!(read_skill_file(text).unwrap().name, "padded");
    }

    #[test]
    fn rejects_file_without_opening_fence() {
        let err = read_skill_file("name: naked\n").unwrap_err();
        assert!(err.contains("metadata fence"), "got: {err}");
    }

    #[test]
    fn rejects_unclosed_fence() {
        let err = read_skill_file("---\nname: dangling\n").unwrap_err();
        assert!(err.contains("never closed"), "got: {err}");
    }

    #[test]
    fn rejects_missing_or_empty_name() {
        assert!(read_skill_file("---\ndescription: no name here\n---\nbody").is_err());
        assert!(read_skill_file("---\nname:\n---\nbody").is_err());
    }

    #[test]
    fn later_duplicate_keys_win() {
        let text = "---\nname: first\nname: second\n---\nbody";
        assert_eq!(read_skill_file(text).unwrap().name, "second");
    }

    #[test]
    fn body_is_trimmed() {
        let text = "---\nname: trim-me\n---\n\n\n  content  \n\n";
        assert_eq!(read_skill_file(text).unwrap().body, "content");
    }
}
