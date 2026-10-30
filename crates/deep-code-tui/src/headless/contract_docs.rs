//! The published wrapper contract, held to the binary that publishes it.
//!
//! `--output-format json` is the one surface another program codes against, and
//! the field table for it does not live in this crate — it lives in two
//! READMEs (`README.md`, and the npm page's copy in `packages/deepcode`). That
//! is deliberate: a generator subcommand would be a new surface to maintain,
//! test and keep stable, while the table a wrapper author actually reads is
//! Markdown on the package page and the repository front door.
//!
//! The cost of prose is that nothing forces it to stay true, which is exactly
//! how the `-p` help line came to describe stdin semantics the binary did not
//! have. So each fact a wrapper depends on is pinned here instead:
//!
//! 1. the documented field names and their optionality are the keys a
//!    [`PrintReport`] actually serializes;
//! 2. every `--flag` named in a contract section is one `-p` accepts;
//! 3. the exit codes printed in the docs are rendered from the constants the
//!    process returns with.
//!
//! A doc that drifts now fails `cargo test` rather than a wrapper's first run.

use super::*;
use crate::cli::PRINT_FLAG_GROUPS;
use deep_code_agent::{CostEstimate, Usage};
use serde_json::Value;

/// The contract section in the repository README (and its translation).
const ROOT_SECTION: &str = "### Driving it from another program";
/// The same contract on the npm page, which is what an installed user lands on.
const NPM_SECTION: &str = "## Use it from your code";

const README: &str = include_str!("../../../../README.md");
const README_ZH: &str = include_str!("../../../../README.zh-CN.md");
const NPM_README: &str = include_str!("../../../../packages/deepcode/README.md");

/// The documented field table must be the report's field set — nothing
/// missing, nothing invented, and "(optional)" must mean "a run can omit it"
/// rather than "the author thought it sounded optional".
///
/// `telemetry` is intentionally outside the comparison: it is a diagnostic
/// blob the README explicitly excludes from the contract, so a wrapper is
/// told not to read it.
#[test]
fn the_readme_field_table_is_what_a_report_serializes() {
    let required = serialized_keys(&contract_sample(None, None, None, None));
    let populated = serialized_keys(&contract_sample(
        Some("boom"),
        Some("session_1_0"),
        Some(CostEstimate::default()),
        Some(Usage::default()),
    ));
    let mut optional: Vec<String> = populated
        .into_iter()
        .filter(|key| !required.contains(key))
        .collect();
    optional.sort();

    for (markdown, heading) in [(README, ROOT_SECTION), (NPM_README, NPM_SECTION)] {
        let (documented_required, documented_optional) = contract_table(markdown, heading);
        assert_eq!(
            documented_required, required,
            "{heading}: the required rows are no longer the report's required keys"
        );
        assert_eq!(
            documented_optional, optional,
            "{heading}: the (optional) rows are no longer the keys a run can omit"
        );
    }
}

/// A contract section that names a flag `-p` rejects is worse than no section
/// at all, because the reader has no way to tell. `--help` is the one global
/// flag both sections are allowed to point at.
#[test]
fn the_readme_contract_sections_name_only_real_print_flags() {
    for (markdown, heading) in [(README, ROOT_SECTION), (NPM_README, NPM_SECTION)] {
        let documented_flags = PRINT_FLAG_GROUPS.join(" ");
        let named: Vec<&str> = readme_section(markdown, heading)
            .split(|c: char| !(c == '-' || c.is_ascii_alphanumeric()))
            .filter(|token| is_flag(token))
            .collect();
        assert!(
            !named.is_empty(),
            "{heading}: no flags found — did the section move or get renamed?"
        );
        for flag in named {
            assert!(
                documented_flags.contains(flag) || flag == "--help",
                "{heading} names {flag}, which -p does not accept"
            );
        }
    }
}

/// The exit codes in the docs are the ones the process returns with. Building
/// the expected line from the constants is the point: changing a code without
/// touching the prose is then a red test, not a stale promise.
#[test]
fn the_readme_exit_codes_match_the_constants() {
    let english = format!(
        "`{EXIT_OK}` finished / `{EXIT_FAILURE}` error / `{EXIT_USAGE}` usage / \
`{EXIT_TIMEOUT}` timeout (`--timeout SECS`) / `{EXIT_INTERRUPT}` Ctrl-C"
    );
    assert!(
        README.contains(&english),
        "README's exit-code line is stale; expected to contain: {english}"
    );
    let chinese = format!(
        "`{EXIT_OK}` 完成 / `{EXIT_FAILURE}` 出错 / `{EXIT_USAGE}` 用法错误 / \
`{EXIT_TIMEOUT}` 超时(`--timeout SECS`)/ `{EXIT_INTERRUPT}` Ctrl-C"
    );
    assert!(
        README_ZH.contains(&chinese),
        "README.zh-CN's exit-code line is stale; expected to contain: {chinese}"
    );
    let npm = format!(
        "`{EXIT_OK}` finished / `{EXIT_FAILURE}` error / `{EXIT_USAGE}` usage / \
`{EXIT_TIMEOUT}` timeout / `{EXIT_INTERRUPT}` Ctrl-C"
    );
    assert!(
        NPM_README.contains(&npm),
        "packages/deepcode/README.md's exit-code line is stale; expected: {npm}"
    );
}

/// One serialized report with exactly the optional fields asked for, so the
/// emitted key set answers "what can a wrapper rely on seeing?".
fn contract_sample(
    error: Option<&str>,
    session_id: Option<&str>,
    cost: Option<CostEstimate>,
    usage: Option<Usage>,
) -> Value {
    let report = output::PrintReport {
        status: "finished",
        result: "answer".to_string(),
        reasoning: String::new(),
        error: error.map(str::to_string),
        session_id: session_id.map(str::to_string),
        denied_approvals: 0,
        duration_ms: 1,
        cost,
        usage,
        telemetry: None,
    };
    serde_json::from_str(&output::report_to_json(&report)).expect("a report serializes to JSON")
}

fn serialized_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("a report is a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// `--timeout` is a flag; a table rule (`---`) and a bare `--` are not.
fn is_flag(token: &str) -> bool {
    token
        .strip_prefix("--")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_alphabetic())
}

/// The `(required, optional)` field names of one contract table, in the order
/// the table lists them, compared as sorted sets.
fn contract_table(markdown: &str, heading: &str) -> (Vec<String>, Vec<String>) {
    let mut required = Vec::new();
    let mut optional = Vec::new();
    // `| ` opens a table row whose first cell is a backticked field name; the
    // header and the `| --- |` separator do not.
    for row in readme_section(markdown, heading)
        .lines()
        .filter(|line| line.starts_with("| `"))
    {
        let name = row
            .split('`')
            .nth(1)
            .unwrap_or_else(|| panic!("{heading}: unparsable table row: {row}"))
            .to_string();
        if row.contains("(optional)") {
            optional.push(name);
        } else {
            required.push(name);
        }
    }
    required.sort();
    optional.sort();
    (required, optional)
}

/// The text from `heading` up to the next top-level section, so a test reads
/// the contract and not the whole file around it.
fn readme_section<'a>(markdown: &'a str, heading: &str) -> &'a str {
    let start = markdown
        .find(heading)
        .unwrap_or_else(|| panic!("no {heading} section in the docs"));
    let body = &markdown[start..];
    let end = body[1..]
        .find("\n## ")
        .map_or(body.len(), |index| index + 1);
    &body[..end]
}
