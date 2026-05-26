//! Language detection and default LSP server registry.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Other,
}

impl Language {
    #[must_use]
    pub fn as_key(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub fn language_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Other => "plaintext",
        }
    }
}

#[must_use]
pub fn detect_language(path: &Path) -> Language {
    let ext = match path.extension().and_then(|value| value.to_str()) {
        Some(ext) => ext.to_ascii_lowercase(),
        None => return Language::Other,
    };
    match ext.as_str() {
        "rs" => Language::Rust,
        "ts" | "tsx" => Language::TypeScript,
        "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
        _ => Language::Other,
    }
}

#[must_use]
pub fn server_for(lang: Language) -> Option<(&'static str, &'static [&'static str])> {
    match lang {
        Language::Rust => Some(("rust-analyzer", &[])),
        Language::TypeScript | Language::JavaScript => {
            Some(("typescript-language-server", &["--stdio"]))
        }
        Language::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_rust_and_typescript() {
        assert_eq!(detect_language(&PathBuf::from("foo.rs")), Language::Rust);
        assert_eq!(
            detect_language(&PathBuf::from("foo.ts")),
            Language::TypeScript
        );
        assert_eq!(
            detect_language(&PathBuf::from("notes.txt")),
            Language::Other
        );
    }

    #[test]
    fn server_for_rust_is_rust_analyzer() {
        let (cmd, args) = server_for(Language::Rust).expect("rust server");
        assert_eq!(cmd, "rust-analyzer");
        assert!(args.is_empty());
    }
}
