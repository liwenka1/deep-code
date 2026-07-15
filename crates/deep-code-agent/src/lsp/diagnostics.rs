//! Normalized diagnostic types and the text form injected into tool results.
//!
//! The transport hands the manager a flat list of [`Diagnostic`]s; the
//! manager groups them per file into a [`DiagnosticBlock`] whose [`render`]
//! output is appended to the edit tool's result. The envelope is a pseudo-XML
//! tag so the model can locate it unambiguously inside arbitrary tool output:
//!
//! ```text
//! <diagnostics file="src/lib.rs">
//!   ERROR [3:9] mismatched types: E0308 (rust-analyzer)
//! </diagnostics>
//! ```
//!
//! [`render`]: DiagnosticBlock::render

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Severity bucket mirroring the LSP `DiagnosticSeverity` integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    /// Map a raw LSP severity integer (1..=4). Absent or out-of-range values
    /// yield `None`, leaving the fallback policy to the caller.
    #[must_use]
    pub fn from_lsp(raw: Option<i64>) -> Option<Self> {
        Some(match raw? {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Information,
            4 => Self::Hint,
            _ => return None,
        })
    }

    /// Uppercase tag that starts each rendered diagnostic line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARNING",
            Self::Information => "INFO",
            Self::Hint => "HINT",
        }
    }

    /// Sort weight: lower is more severe, so errors lead after ranking.
    pub(crate) fn rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warning => 1,
            Self::Information => 2,
            Self::Hint => 3,
        }
    }
}

/// Span of a diagnostic in 1-based line/column coordinates (the transport
/// shifts from the 0-based positions on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// A single normalized diagnostic, decoupled from the wire representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub range: DiagnosticRange,
    pub severity: Severity,
    pub message: String,
    /// Tool that produced the diagnostic (e.g. a compiler frontend name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Machine-readable code such as a compiler error number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Diagnostic {
    /// First line of the message, trimmed. Multi-line bodies stay out of the
    /// rendered block to keep each entry on one line.
    fn headline(&self) -> &str {
        self.message.lines().next().map(str::trim).unwrap_or("")
    }

    /// Trailing "code (source)" annotation, whichever parts are present.
    fn annotation(&self) -> Option<String> {
        match (self.code.as_deref(), self.source.as_deref()) {
            (Some(code), Some(origin)) => Some(format!("{code} ({origin})")),
            (Some(code), None) => Some(code.to_owned()),
            (None, Some(origin)) => Some(origin.to_owned()),
            (None, None) => None,
        }
    }
}

/// All surviving diagnostics for one file, in final display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticBlock {
    /// Display path (workspace-relative when possible) for the `file` attribute.
    pub file: PathBuf,
    pub items: Vec<Diagnostic>,
}

impl DiagnosticBlock {
    /// Produce the tagged text form shown in the module docs. An empty item
    /// list renders as the empty string so callers can skip injection cheaply.
    #[must_use]
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let mut text = String::new();
        let _ = writeln!(text, "<diagnostics file=\"{}\">", self.file.display());
        for item in &self.items {
            let _ = write!(
                text,
                "  {} [{}:{}] {}",
                item.severity.label(),
                item.range.start_line,
                item.range.start_column,
                item.headline(),
            );
            if let Some(tag) = item.annotation() {
                let _ = write!(text, ": {tag}");
            }
            text.push('\n');
        }
        text.push_str("</diagnostics>");
        text
    }

    /// Cap the item list, keeping the leading (already ranked) entries.
    pub fn truncate(&mut self, cap: usize) {
        self.items.truncate(cap);
    }

    /// One-line severity tally used for status/event surfaces.
    #[must_use]
    pub fn compact_summary(&self) -> String {
        let mut errors = 0usize;
        let mut warnings = 0usize;
        for item in &self.items {
            match item.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                _ => {}
            }
        }
        let file = self.file.display();
        match (errors, warnings) {
            (0, 0) => format!("{file}: {} diagnostic(s)", self.items.len()),
            (e, 0) => format!("{file}: {e} error(s)"),
            (0, w) => format!("{file}: {w} warning(s)"),
            (e, w) => format!("{file}: {e} error(s), {w} warning(s)"),
        }
    }
}

/// Join the non-empty renderings of several blocks, one file per block.
#[must_use]
pub fn render_blocks(blocks: &[DiagnosticBlock]) -> String {
    blocks
        .iter()
        .map(DiagnosticBlock::render)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Semicolon-joined [`compact_summary`] of every block.
///
/// [`compact_summary`]: DiagnosticBlock::compact_summary
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

    fn diag(line: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            file: PathBuf::from("lib/core.rs"),
            range: DiagnosticRange {
                start_line: line,
                start_column: 3,
                end_line: line,
                end_column: 9,
            },
            severity,
            message: message.to_owned(),
            source: None,
            code: None,
        }
    }

    #[test]
    fn severity_mapping_covers_the_lsp_integers() {
        assert_eq!(Severity::from_lsp(Some(1)), Some(Severity::Error));
        assert_eq!(Severity::from_lsp(Some(2)), Some(Severity::Warning));
        assert_eq!(Severity::from_lsp(Some(3)), Some(Severity::Information));
        assert_eq!(Severity::from_lsp(Some(4)), Some(Severity::Hint));
        assert_eq!(Severity::from_lsp(Some(0)), None);
        assert_eq!(Severity::from_lsp(Some(5)), None);
        assert_eq!(Severity::from_lsp(None), None);
    }

    #[test]
    fn block_renders_the_full_envelope() {
        let block = DiagnosticBlock {
            file: PathBuf::from("lib/core.rs"),
            items: vec![diag(4, Severity::Error, "cannot find value `total`")],
        };
        assert_eq!(
            block.render(),
            "<diagnostics file=\"lib/core.rs\">\n  ERROR [4:3] cannot find value `total`\n</diagnostics>"
        );
    }

    #[test]
    fn annotation_combines_code_and_source() {
        let mut item = diag(1, Severity::Error, "mismatched types");
        item.code = Some("E0308".to_owned());
        item.source = Some("rust-analyzer".to_owned());
        let both = DiagnosticBlock {
            file: PathBuf::from("a.rs"),
            items: vec![item.clone()],
        };
        assert!(both.render().contains("mismatched types: E0308 (rust-analyzer)"));

        item.source = None;
        let code_only = DiagnosticBlock {
            file: PathBuf::from("a.rs"),
            items: vec![item],
        };
        assert!(code_only.render().contains("mismatched types: E0308"));
        assert!(!code_only.render().contains('('));
    }

    #[test]
    fn only_the_first_message_line_survives_rendering() {
        let block = DiagnosticBlock {
            file: PathBuf::from("a.rs"),
            items: vec![diag(2, Severity::Warning, "unused variable\nnote: prefix with _\nhelp: ...")],
        };
        let text = block.render();
        assert!(text.contains("WARNING [2:3] unused variable"));
        assert!(!text.contains("note:"));
        assert!(!text.contains("help:"));
    }

    #[test]
    fn empty_blocks_vanish_from_the_bundle() {
        let blocks = vec![
            DiagnosticBlock {
                file: PathBuf::from("empty.rs"),
                items: Vec::new(),
            },
            DiagnosticBlock {
                file: PathBuf::from("busy.rs"),
                items: vec![diag(1, Severity::Error, "broken")],
            },
        ];
        let bundle = render_blocks(&blocks);
        assert!(!bundle.contains("empty.rs"));
        assert!(bundle.starts_with("<diagnostics file=\"busy.rs\">"));
    }

    #[test]
    fn truncate_keeps_the_leading_entries() {
        let mut block = DiagnosticBlock {
            file: PathBuf::from("a.rs"),
            items: (1..=7).map(|n| diag(n, Severity::Error, "e")).collect(),
        };
        block.truncate(3);
        assert_eq!(block.items.len(), 3);
        assert_eq!(block.items[2].range.start_line, 3);
        block.truncate(10); // larger cap is a no-op
        assert_eq!(block.items.len(), 3);
    }

    #[test]
    fn summary_tallies_severities_per_file() {
        let block = DiagnosticBlock {
            file: PathBuf::from("mixed.rs"),
            items: vec![
                diag(1, Severity::Error, "e1"),
                diag(2, Severity::Error, "e2"),
                diag(3, Severity::Warning, "w1"),
            ],
        };
        assert_eq!(block.compact_summary(), "mixed.rs: 2 error(s), 1 warning(s)");

        let hints_only = DiagnosticBlock {
            file: PathBuf::from("h.rs"),
            items: vec![diag(1, Severity::Hint, "h")],
        };
        assert_eq!(hints_only.compact_summary(), "h.rs: 1 diagnostic(s)");
    }

    #[test]
    fn summaries_join_with_semicolons() {
        let blocks = vec![
            DiagnosticBlock {
                file: PathBuf::from("x.rs"),
                items: vec![diag(1, Severity::Error, "e")],
            },
            DiagnosticBlock {
                file: PathBuf::from("y.rs"),
                items: vec![diag(1, Severity::Warning, "w")],
            },
        ];
        assert_eq!(summarize_blocks(&blocks), "x.rs: 1 error(s); y.rs: 1 warning(s)");
    }
}
