use super::types::StructuredReport;

pub const SUBAGENT_OUTPUT_FORMAT: &str = r#"## Output contract (mandatory)

When you finish, your final assistant message MUST include these Markdown H3 sections in order:

### SUMMARY
One paragraph stating what you did and the headline conclusion.

### EVIDENCE
Bullet list of concrete artifacts (`path:line-range`, command + exit code, search hits). Write "None." if nothing was observed.

### CHANGES
Bullet list of every write performed, or the single line "None." if read-only.

### RISKS
Bullet list of risks not addressed, or "None observed."

### BLOCKERS
Blockers that stopped you, or "None." if finished cleanly.

Produce the structured report and stop. Do not ask follow-up questions.
"#;

pub fn parse_structured_report(text: &str) -> Option<StructuredReport> {
    let summary = extract_section(text, "SUMMARY")?;
    let evidence = extract_section(text, "EVIDENCE").unwrap_or_else(|| "None.".to_string());
    let changes = extract_section(text, "CHANGES").unwrap_or_else(|| "None.".to_string());
    let risks = extract_section(text, "RISKS").unwrap_or_else(|| "None observed.".to_string());
    let blockers = extract_section(text, "BLOCKERS").unwrap_or_else(|| "None.".to_string());
    Some(StructuredReport {
        summary,
        evidence,
        changes,
        risks,
        blockers,
    })
}

fn extract_section(text: &str, heading: &str) -> Option<String> {
    let marker = format!("### {heading}");
    let start = text.find(&marker)?;
    let body_start = start + marker.len();
    let rest = &text[body_start..];
    let body = if let Some(next) = rest.find("\n### ") {
        &rest[..next]
    } else {
        rest
    };
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_requires_summary_section() {
        assert!(parse_structured_report("no sections here").is_none());
    }

    #[test]
    fn parses_structured_sections() {
        let text = r#"Done.

### SUMMARY
Mapped the module.

### EVIDENCE
- src/lib.rs:1-20

### CHANGES
None.

### RISKS
None observed.

### BLOCKERS
None.
"#;
        let report = parse_structured_report(text).expect("report");
        assert!(report.summary.contains("Mapped"));
        assert!(report.evidence.contains("lib.rs"));
    }
}
