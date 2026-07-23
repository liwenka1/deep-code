//! UI 文案国际化:扁平 JSON 语言包 + 枚举 key 查表。
//!
//! 只覆盖 TUI 界面文案(chrome),不涉及模型输出、prompt 或工具数据。
//! 语言包在编译期嵌入(`locales/zh.json` / `locales/en.json`),key 是
//! [`TextId`] 变体名——调用处用枚举,手误在编译期就报错;语言包缺 key、
//! 多 key、占位符不一致由本模块的测试兜底,`cargo test` 即可发现。
//!
//! 带参消息在 JSON 里写 `{name}` 占位符,经 [`tr_with`] 运行时替换。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 支持的界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// 配置文件/持久化用的设置值。
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    /// 由语言标签(如 `zh`、`zh_CN.UTF-8`、`en-US`)判定语言。
    /// `/lang` 也复用它,让显式切换与自动探测接受同一组别名。
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        let lower = tag.trim().to_ascii_lowercase();
        if lower.starts_with("zh") {
            Some(Self::Zh)
        } else if lower.starts_with("en") {
            Some(Self::En)
        } else {
            None
        }
    }

    /// 从配置值解析语言:明确的 `zh`/`en` 直接生效;`auto`、空串或无法
    /// 识别的值走环境探测(`LC_ALL` → `LC_MESSAGES` → `LANG`),探测不到
    /// 回退英文。
    #[must_use]
    pub fn resolve(setting: &str, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let normalized = setting.trim();
        if !normalized.is_empty()
            && !normalized.eq_ignore_ascii_case("auto")
            && let Some(lang) = Self::from_tag(normalized)
        {
            return lang;
        }
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Some(lang) = env(key).as_deref().and_then(Self::from_tag) {
                return lang;
            }
        }
        Self::En
    }

    /// [`resolve`](Self::resolve) against the real process environment — the
    /// ergonomic entry point; `resolve` stays the seam tests drive with a
    /// stub env.
    #[must_use]
    pub fn from_env(setting: &str) -> Self {
        Self::resolve(setting, &|name| std::env::var(name).ok())
    }
}

/// 定义 [`TextId`] 及其全量列表:一处声明,枚举与 `ALL` 数组不会漂移。
macro_rules! text_ids {
    ($($name:ident),+ $(,)?) => {
        /// 一条界面文案的 key。变体名即语言包 JSON 的 key。
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum TextId {
            $($name),+
        }

        impl TextId {
            /// 全量列表,供完整性测试遍历(非测试构建中无人使用)。
            #[cfg(test)]
            pub const ALL: &'static [TextId] = &[$(TextId::$name),+];

            #[must_use]
            pub fn key(self) -> &'static str {
                match self {
                    $(TextId::$name => stringify!($name)),+
                }
            }
        }
    };
}

text_ids! {
    // 欢迎卡
    WelcomeStatusLabel,
    WelcomeModelLabel,
    WelcomeWorkspaceLabel,
    WelcomeSessionLabel,
    WelcomeOffline,
    WelcomeModelValue,
    WelcomeIntro,
    SessionNewPersistent,
    SessionNewEphemeral,
    SessionResumed,
    // 审批面板
    ApprovalNeeded,
    RiskHigh,
    RiskMedium,
    RiskLow,
    ApprovalSandbox,
    ApprovalRule,
    ApprovalCautionHeader,
    ApprovalPreviewHeader,
    ApprovalOptApprove,
    ApprovalOptSession,
    ApprovalOptDeny,
    StatusApprovalPrompt,
    StatusApprovalResolved,
    DecisionApproved,
    DecisionApprovedSession,
    DecisionDenied,
    StatusToolResolved,
    WordYes,
    WordNo,
    // 会话选择器
    PickerTitle,
    PickerHelpResume,
    PickerHelpStartup,
    EmptySessionTitle,
    // 相对时间
    TimeJustNow,
    TimeMinutesAgo,
    TimeHoursAgo,
    TimeDaysAgo,
    // 输入区/状态栏
    ComposerPlaceholder,
    CompletionMenuTitle,
    ErrorPrefix,
    StatusEscCancel,
    StreamingGenerating,
    CopiedSelection,
    StatusEmptyPrompt,
    StatusStreamingFrom,
    StatusInputClearedEsc,
    StatusInputClearedCtrlC,
    StatusCtrlCQuitConfirm,
    StatusCancelling,
    StatusAgentError,
    ModeError,
    ModeApproval,
    ModeStreaming,
    ModeReady,
    ModeReadyResumed,
    // 运行时事件
    StatusToolCallReceiving,
    StatusToolCallReceivingArgs,
    StatusToolRunning,
    SystemWorkspaceRestored,
    StatusRestored,
    SystemSaveFailed,
    StatusSaveFailed,
    SystemSaveRecovered,
    StatusSessionUpdated,
    StatusDiagnostics,
    StatusCompacted,
    SystemWarning,
    StatusRollbackHint,
    SystemTurnCancelled,
    StatusCancelled,
    StatusReadySubagents,
    // 转录 cell
    BadgeRequired,
    BadgeApproved,
    BadgeDenied,
    CheckpointLabel,
    CheckpointRestoreHint,
    CompactionSummaryTitle,
    CompactionSummaryTitleMeta,
    // 粘贴 chip
    PasteChipLines,
    PasteChipChars,
    // /find
    FindNoTranscript,
    FindFound,
    FindExhausted,
    FindNotFound,
    UsageFind,
    // 会话管理
    BusyRelaunchConfig,
    SessionReloadFailedRestart,
    BusySwitchSession,
    SessionsReadFailed,
    NoResumableSessions,
    StatusPickSession,
    StatusResumeCancelled,
    SessionStoreOpenFailed,
    SessionIdInvalid,
    SessionNotFound,
    StatusAlreadyCurrent,
    StatusSwitchedSession,
    BusyNewConversation,
    StatusNewConversation,
    ConfigWarningsHeader,
    // 命令 hint
    HintHelp,
    HintClear,
    HintStatus,
    HintModel,
    HintApikey,
    HintLogout,
    HintCopy,
    HintCheckpoints,
    HintRestore,
    HintResume,
    HintSessions,
    HintAgents,
    HintFind,
    HintLang,
    // 命令输出
    UsageRestore,
    ApiKeySaved,
    StatusConnected,
    ModelCurrent,
    StatusModelInfoShown,
    ModelUnknown,
    ModelSwitched,
    LogoutDone,
    StatusLoggedOut,
    CopiedResponse,
    NothingToCopy,
    HelpHeader,
    HelpTipSessions,
    HelpKeys,
    HelpNoteCancel,
    HelpNoteAutoAllow,
    StatusHelpShown,
    StatusCacheHitLine,
    StatusTrimmedSuffix,
    StatusShown,
    SubagentsUnavailable,
    NoSubagents,
    SubagentsHeader,
    SubagentsCount,
    NoSavedSessions,
    SessionsHeader,
    SessionsCount,
    ListFailed,
    SessionsUnavailable,
    NoCheckpoints,
    CheckpointsHeader,
    CheckpointsCount,
    CheckpointsUnavailable,
    RestoreOutsideRuntime,
    RestoreFailed,
    // 遥测
    TelemetryCacheHit,
    TelemetryTurnSession,
    TelemetryNearCompaction,
    TelemetryStreamRetries,
    TelemetryCascade,
    PrefixFirstTurn,
    PrefixStable,
    PrefixChanged,
    // /lang
    LangCurrent,
    LangName,
    LangSwitched,
    LangUnknown,
}

static ZH_JSON: &str = include_str!("../locales/zh.json");
static EN_JSON: &str = include_str!("../locales/en.json");

fn table(lang: Lang) -> &'static HashMap<String, String> {
    static ZH: OnceLock<HashMap<String, String>> = OnceLock::new();
    static EN: OnceLock<HashMap<String, String>> = OnceLock::new();
    let (cell, source, name) = match lang {
        Lang::Zh => (&ZH, ZH_JSON, "zh.json"),
        Lang::En => (&EN, EN_JSON, "en.json"),
    };
    cell.get_or_init(|| {
        serde_json::from_str(source).unwrap_or_else(|error| panic!("{name} 无法解析: {error}"))
    })
}

/// 查表取文案。缺 key 时返回 key 本身(测试保证不会发生,这里只兜底)。
#[must_use]
pub fn tr(lang: Lang, id: TextId) -> &'static str {
    table(lang)
        .get(id.key())
        .map_or_else(|| id.key(), String::as_str)
}

/// 查表并替换 `{name}` 占位符。
#[must_use]
pub fn tr_with(lang: Lang, id: TextId, args: &[(&str, &str)]) -> String {
    let mut out = tr(lang, id).to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn keys(lang: Lang) -> BTreeSet<String> {
        table(lang).keys().cloned().collect()
    }

    fn enum_keys() -> BTreeSet<String> {
        TextId::ALL.iter().map(|id| id.key().to_string()).collect()
    }

    /// 两个语言包与枚举三方 key 集合完全一致:缺译、孤儿 key 都在这里爆。
    #[test]
    fn locale_packs_match_text_ids_exactly() {
        let expected = enum_keys();
        for (lang, name) in [(Lang::Zh, "zh.json"), (Lang::En, "en.json")] {
            let actual = keys(lang);
            let missing: Vec<_> = expected.difference(&actual).collect();
            let orphaned: Vec<_> = actual.difference(&expected).collect();
            assert!(missing.is_empty(), "{name} 缺少 key: {missing:?}");
            assert!(orphaned.is_empty(), "{name} 多出无主 key: {orphaned:?}");
        }
    }

    /// 同一条文案的占位符集合在 zh/en 中必须一致,防止插值静默失效。
    #[test]
    fn placeholders_are_consistent_across_locales() {
        fn placeholders(text: &str) -> BTreeSet<String> {
            let mut out = BTreeSet::new();
            let mut rest = text;
            while let Some(start) = rest.find('{') {
                let Some(len) = rest[start + 1..].find('}') else {
                    break;
                };
                out.insert(rest[start + 1..start + 1 + len].to_string());
                rest = &rest[start + 1 + len..];
            }
            out
        }
        for id in TextId::ALL {
            let zh = placeholders(tr(Lang::Zh, *id));
            let en = placeholders(tr(Lang::En, *id));
            assert_eq!(zh, en, "{} 的占位符 zh/en 不一致", id.key());
        }
    }

    #[test]
    fn tr_with_replaces_placeholders() {
        let text = tr_with(Lang::Zh, TextId::CopiedSelection, &[("count", "12")]);
        assert!(text.contains("12"), "{text}");
        assert!(!text.contains("{count}"), "{text}");
    }

    #[test]
    fn resolve_prefers_setting_then_env_then_english() {
        let no_env = |_: &str| None;
        assert_eq!(Lang::resolve("zh", &no_env), Lang::Zh);
        assert_eq!(Lang::resolve("en", &no_env), Lang::En);
        assert_eq!(Lang::resolve("EN", &no_env), Lang::En);
        // 明确设置压过环境。
        let zh_env = |key: &str| (key == "LANG").then(|| "zh_CN.UTF-8".to_string());
        assert_eq!(Lang::resolve("en", &zh_env), Lang::En);
        // auto / 未知值走环境探测。
        assert_eq!(Lang::resolve("auto", &zh_env), Lang::Zh);
        assert_eq!(Lang::resolve("fr", &zh_env), Lang::Zh);
        // LC_ALL 优先于 LANG。
        let mixed = |key: &str| match key {
            "LC_ALL" => Some("en_US.UTF-8".to_string()),
            "LANG" => Some("zh_CN.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(Lang::resolve("auto", &mixed), Lang::En);
        // 探测不到回退英文。
        assert_eq!(Lang::resolve("auto", &no_env), Lang::En);
    }
}
