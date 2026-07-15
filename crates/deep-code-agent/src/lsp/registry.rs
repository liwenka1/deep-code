//! Built-in table mapping file extensions to languages and their default
//! language servers.
//!
//! The table is intentionally small: it only lists languages whose servers
//! we have exercised end-to-end. Adding a language is one new [`LangEntry`]
//! row plus a [`Language`] variant.

use std::path::Path;

/// Languages the diagnostics pipeline knows how to serve. `Other` marks
/// files with no matching table row; the manager skips those entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Other,
}

/// One row of the built-in language table.
struct LangEntry {
    language: Language,
    /// Stable lowercase identifier used in log lines.
    key: &'static str,
    /// `languageId` value sent in `textDocument/didOpen`.
    lsp_id: &'static str,
    /// Lowercase extensions (without the dot) owned by this language.
    extensions: &'static [&'static str],
    /// Default server binary plus its arguments.
    server: (&'static str, &'static [&'static str]),
}

const LANGUAGE_TABLE: &[LangEntry] = &[
    LangEntry {
        language: Language::Rust,
        key: "rust",
        lsp_id: "rust",
        extensions: &["rs"],
        server: ("rust-analyzer", &[]),
    },
    LangEntry {
        language: Language::TypeScript,
        key: "typescript",
        lsp_id: "typescript",
        extensions: &["ts", "tsx"],
        server: ("typescript-language-server", &["--stdio"]),
    },
    LangEntry {
        language: Language::JavaScript,
        key: "javascript",
        lsp_id: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        // The TypeScript server handles plain JavaScript as well.
        server: ("typescript-language-server", &["--stdio"]),
    },
];

fn row(language: Language) -> Option<&'static LangEntry> {
    LANGUAGE_TABLE
        .iter()
        .find(|entry| entry.language == language)
}

impl Language {
    /// Stable lowercase name for logs and config keys.
    #[must_use]
    pub fn as_key(self) -> &'static str {
        row(self).map_or("other", |entry| entry.key)
    }

    /// `languageId` to report when opening a document of this language.
    #[must_use]
    pub fn language_id(self) -> &'static str {
        row(self).map_or("plaintext", |entry| entry.lsp_id)
    }
}

/// Classify a file by its extension (case-insensitively). Files without an
/// extension, or with one outside the table, come back as [`Language::Other`].
#[must_use]
pub fn detect_language(path: &Path) -> Language {
    let Some(raw_ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return Language::Other;
    };
    let ext = raw_ext.to_ascii_lowercase();
    LANGUAGE_TABLE
        .iter()
        .find(|entry| entry.extensions.contains(&ext.as_str()))
        .map_or(Language::Other, |entry| entry.language)
}

/// Default `(binary, args)` launch spec for a language, or `None` when the
/// table has no server wired for it.
#[must_use]
pub fn server_for(lang: Language) -> Option<(&'static str, &'static [&'static str])> {
    row(lang).map(|entry| entry.server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_lookup_ignores_case() {
        assert_eq!(detect_language(&PathBuf::from("Main.RS")), Language::Rust);
        assert_eq!(
            detect_language(&PathBuf::from("App.TSX")),
            Language::TypeScript
        );
    }

    #[test]
    fn every_javascript_flavor_maps_to_javascript() {
        for name in ["a.js", "b.jsx", "c.mjs", "d.cjs"] {
            assert_eq!(
                detect_language(&PathBuf::from(name)),
                Language::JavaScript,
                "{name} should classify as JavaScript"
            );
        }
    }

    #[test]
    fn extensionless_and_unknown_files_are_other() {
        assert_eq!(detect_language(&PathBuf::from("Makefile")), Language::Other);
        assert_eq!(detect_language(&PathBuf::from("data.csv")), Language::Other);
    }

    #[test]
    fn table_supplies_a_server_per_language() {
        assert_eq!(server_for(Language::Rust), Some(("rust-analyzer", &[][..])));
        let (ts_cmd, ts_args) = server_for(Language::TypeScript).unwrap();
        let (js_cmd, js_args) = server_for(Language::JavaScript).unwrap();
        assert_eq!((ts_cmd, ts_args), (js_cmd, js_args));
        assert_eq!(ts_cmd, "typescript-language-server");
        assert_eq!(ts_args, ["--stdio"]);
    }

    #[test]
    fn other_has_no_server_and_placeholder_ids() {
        assert!(server_for(Language::Other).is_none());
        assert_eq!(Language::Other.as_key(), "other");
        assert_eq!(Language::Other.language_id(), "plaintext");
    }

    #[test]
    fn keys_and_lsp_ids_are_consistent() {
        for lang in [Language::Rust, Language::TypeScript, Language::JavaScript] {
            assert_eq!(lang.as_key(), lang.language_id());
        }
    }
}
