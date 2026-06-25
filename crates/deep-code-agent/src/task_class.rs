//! Shared task-difficulty signal.
//!
//! A single keyword table feeds *both* model selection and reasoning-effort
//! selection so the two axes can never diverge (previously they used separate,
//! inconsistent lists). ASCII keywords match on word boundaries (token prefix)
//! to avoid substring false positives like `research`→`search` or
//! `terror`→`error`; CJK keywords have no word boundaries, so they match by
//! substring.

/// How heavy a task a keyword implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskWeight {
    /// Debugging / failure investigation: strong model + deepest reasoning.
    Deep,
    /// Substantial engineering: strong model, ordinary deep reasoning.
    Heavy,
    /// Could go either way: strong model unless cost-saving is on.
    Borderline,
    /// Lookups and reads: cheap model + light reasoning.
    Light,
}

impl TaskWeight {
    /// Higher wins when a prompt matches several keywords.
    fn priority(self) -> i32 {
        match self {
            Self::Deep => 3,
            Self::Heavy => 2,
            Self::Borderline => 1,
            Self::Light => 0,
        }
    }
}

const KEYWORDS: &[(&str, TaskWeight)] = &[
    // Deep — debugging / errors (English + 简体/繁体).
    ("debug", TaskWeight::Deep),
    ("error", TaskWeight::Deep),
    ("crash", TaskWeight::Deep),
    ("\u{8c03}\u{8bd5}", TaskWeight::Deep), // 调试
    ("\u{8abf}\u{8a66}", TaskWeight::Deep), // 調試
    ("\u{9519}\u{8bef}", TaskWeight::Deep), // 错误
    ("\u{62a5}\u{9519}", TaskWeight::Deep), // 报错
    ("\u{51fa}\u{9519}", TaskWeight::Deep), // 出错
    ("\u{5d29}\u{6e83}", TaskWeight::Deep), // 崩溃
    // Heavy — substantial engineering.
    ("refactor", TaskWeight::Heavy),
    ("architecture", TaskWeight::Heavy),
    ("design", TaskWeight::Heavy),
    ("security", TaskWeight::Heavy),
    ("review", TaskWeight::Heavy),
    ("audit", TaskWeight::Heavy),
    ("migrate", TaskWeight::Heavy),
    ("optimize", TaskWeight::Heavy),
    ("rewrite", TaskWeight::Heavy),
    ("\u{91cd}\u{6784}", TaskWeight::Heavy), // 重构
    ("\u{91cd}\u{69cb}", TaskWeight::Heavy), // 重構
    ("\u{67b6}\u{6784}", TaskWeight::Heavy), // 架构
    ("\u{67b6}\u{69cb}", TaskWeight::Heavy), // 架構
    ("\u{8bbe}\u{8ba1}", TaskWeight::Heavy), // 设计
    ("\u{8a2d}\u{8a08}", TaskWeight::Heavy), // 設計
    ("\u{5b89}\u{5168}", TaskWeight::Heavy), // 安全
    ("\u{5ba1}\u{67e5}", TaskWeight::Heavy), // 审查
    ("\u{5be9}\u{67e5}", TaskWeight::Heavy), // 審查
    ("\u{5ba1}\u{8ba1}", TaskWeight::Heavy), // 审计
    ("\u{5be9}\u{8a08}", TaskWeight::Heavy), // 審計
    ("\u{8fc1}\u{79fb}", TaskWeight::Heavy), // 迁移
    ("\u{9077}\u{79fb}", TaskWeight::Heavy), // 遷移
    ("\u{4f18}\u{5316}", TaskWeight::Heavy), // 优化
    ("\u{512a}\u{5316}", TaskWeight::Heavy), // 優化
    ("\u{91cd}\u{5199}", TaskWeight::Heavy), // 重写
    ("\u{91cd}\u{5beb}", TaskWeight::Heavy), // 重寫
    // Borderline — implement / analyze.
    ("implement", TaskWeight::Borderline),
    ("analyze", TaskWeight::Borderline),
    ("\u{5b9e}\u{73b0}", TaskWeight::Borderline), // 实现
    ("\u{5be6}\u{73fe}", TaskWeight::Borderline), // 實現
    ("\u{5206}\u{6790}", TaskWeight::Borderline), // 分析
    // Light — lookups / reads.
    ("search", TaskWeight::Light),
    ("lookup", TaskWeight::Light),
    ("\u{641c}\u{7d22}", TaskWeight::Light), // 搜索
    ("\u{67e5}\u{627e}", TaskWeight::Light), // 查找
    ("\u{67e5}\u{8be2}", TaskWeight::Light), // 查询
];

/// Classify a prompt by its highest-priority keyword match, returning the
/// matched weight and keyword (for explainable routing reasons). `None` when
/// no keyword matches.
#[must_use]
pub(crate) fn classify_keyword(prompt: &str) -> Option<(TaskWeight, &'static str)> {
    let lower = prompt.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();

    let mut best: Option<(TaskWeight, &'static str)> = None;
    for (keyword, weight) in KEYWORDS {
        let hit = if keyword.is_ascii() {
            // Token prefix match: catches `debug`→`debugging`/`errors` while
            // rejecting `research`/`terror`.
            tokens.iter().any(|token| token.starts_with(keyword))
        } else {
            lower.contains(keyword)
        };
        if hit && best.is_none_or(|(current, _)| weight.priority() > current.priority()) {
            best = Some((*weight, keyword));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_matches_on_word_boundary_not_substring() {
        // `research` must not trigger `search`; `terror` must not trigger `error`.
        assert_eq!(classify_keyword("research the topic"), None);
        assert_eq!(classify_keyword("a tale of terror"), None);
    }

    #[test]
    fn ascii_prefix_catches_inflections() {
        assert_eq!(classify_keyword("debugging the loop").map(|hit| hit.0), Some(TaskWeight::Deep));
        assert_eq!(classify_keyword("searching files").map(|hit| hit.0), Some(TaskWeight::Light));
    }

    #[test]
    fn highest_priority_keyword_wins() {
        // Contains both Light (`search`) and Heavy (`refactor`); Heavy wins.
        assert_eq!(
            classify_keyword("search usages then refactor").map(|hit| hit.0),
            Some(TaskWeight::Heavy)
        );
    }

    #[test]
    fn cjk_matches_by_substring() {
        assert_eq!(
            classify_keyword("\u{5e2e}\u{6211}\u{91cd}\u{6784}\u{8fd9}\u{4e2a}").map(|hit| hit.0),
            Some(TaskWeight::Heavy)
        );
        assert_eq!(
            classify_keyword("\u{641c}\u{7d22}\u{4ee3}\u{7801}").map(|hit| hit.0),
            Some(TaskWeight::Light)
        );
    }

    #[test]
    fn no_keyword_is_none() {
        assert_eq!(classify_keyword("how are you today"), None);
    }
}
