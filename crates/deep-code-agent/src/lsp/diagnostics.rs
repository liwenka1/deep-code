//! Diagnostic model and compact rendering for post-edit LSP feedback.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// LSP severity (1 = Error, 2 = Warning, 3 = Information, 4 = Hint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    #[must_use]
    pub fn from_lsp(code: Option<i64>) -> Option<Self> {
        match code? {
            1 => Some(Self::Error),
            2 => Some(Self::Warning),
            3 => Some(Self::Information),
            4 => Some(Self::Hint),
            _ => None,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARNING",
            Self::Information => "INFO",
            Self::Hint => "HINT",
        }
    }
}

/// 1-based line/column range for display and serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// One normalized LSP diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub range: DiagnosticRange,
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Diagnostic {
    fn render_message(&self) -> String {
        self.message.lines().next().unwrap_or("").trim().to_string()
    }

    fn render_code(&self) -> String {
        match (&self.code, &self.source) {
            (Some(code), Some(source)) => format!("{code} ({source})"),
            (Some(code), None) => code.clone(),
            (None, Some(source)) => source.clone(),
            (None, None) => String::new(),
        }
    }
}

/// Diagnostics for one file, ready to render or inject into agent context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticBlock {
    pub file: PathBuf,
    pub items: Vec<Diagnostic>,
}

impl DiagnosticBlock {
    #[must_use]
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let file_attr = self.file.display();
        let mut out = format!("<diagnostics file=\"{file_attr}\">\n");
        for item in &self.items {
            let code_suffix = item.render_code();
            let suffix = if code_suffix.is_empty() {
                String::new()
            } else {
                format!(": {code_suffix}")
            };
            out.push_str(&format!(
                "  {} [{}:{}] {}{suffix}\n",
                item.severity.label(),
                item.range.start_line,
                item.range.start_column,
                item.render_message(),
            ));
        }
        out.push_str("</diagnostics>");
        out
    }

    pub fn truncate(&mut self, max_per_file: usize) {
        if self.items.len() > max_per_file {
            self.items.truncate(max_per_file);
        }
    }

    #[must_use]
    pub fn compact_summary(&self) -> String {
        let errors = self
            .items
            .iter()
            .filter(|item| item.severity == Severity::Error)
            .count();
        let warnings = self
            .items
            .iter()
            .filter(|item| item.severity == Severity::Warning)
            .count();
        let file = self.file.display();
        match (errors, warnings) {
            (0, 0) => format!("{file}: {} diagnostic(s)", self.items.len()),
            (e, 0) => format!("{file}: {e} error(s)"),
            (0, w) => format!("{file}: {w} warning(s)"),
            (e, w) => format!("{file}: {e} error(s), {w} warning(s)"),
        }
    }
}

#[must_use]
pub fn render_blocks(blocks: &[DiagnosticBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            let rendered = block.render();
            if rendered.is_empty() {
                None
            } else {
                Some(rendered)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[must_use]
pub fn summarize_blocks(blocks: &[DiagnosticBlock]) -> String {
    blocks
        .iter()
        .map(DiagnosticBlock::compact_summary)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_diagnostic() -> Diagnostic {
        Diagnostic {
            file: PathBuf::from("src/foo.rs"),
            range: DiagnosticRange {
                start_line: 12,
                start_column: 8,
                end_line: 12,
                end_column: 9,
            },
            severity: Severity::Error,
            message: "missing semicolon".to_string(),
            source: Some("rust-analyzer".to_string()),
            code: Some("E0101".to_string()),
        }
    }

    #[test]
    fn renders_block_with_source_and_code() {
        let block = DiagnosticBlock {
            file: PathBuf::from("src/foo.rs"),
            items: vec![sample_diagnostic()],
        };
        let rendered = block.render();
        assert!(rendered.contains("<diagnostics file=\"src/foo.rs\">"));
        assert!(rendered.contains("ERROR [12:8] missing semicolon: E0101 (rust-analyzer)"));
    }

    #[test]
    fn summarize_counts_severities() {
        let block = DiagnosticBlock {
            file: PathBuf::from("a.rs"),
            items: vec![
                Diagnostic {
                    file: PathBuf::from("a.rs"),
                    range: DiagnosticRange {
                        start_line: 1,
                        start_column: 1,
                        end_line: 1,
                        end_column: 2,
                    },
                    severity: Severity::Error,
                    message: "err".to_string(),
                    source: None,
                    code: None,
                },
                Diagnostic {
                    file: PathBuf::from("a.rs"),
                    range: DiagnosticRange {
                        start_line: 2,
                        start_column: 1,
                        end_line: 2,
                        end_column: 2,
                    },
                    severity: Severity::Warning,
                    message: "warn".to_string(),
                    source: None,
                    code: None,
                },
            ],
        };
        assert_eq!(block.compact_summary(), "a.rs: 1 error(s), 1 warning(s)");
    }
}
