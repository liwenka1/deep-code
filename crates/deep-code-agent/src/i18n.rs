//! UI 文案国际化:扁平 JSON 语言包 + 枚举 key 查表。
//!
//! 只覆盖 TUI 界面文案(chrome),不涉及模型输出、prompt 或工具数据。
//! 语言包在编译期嵌入(`locales/zh.json` / `locales/en.json`),key 是
//! [`TextId`] 变体名——调用处用枚举,手误在编译期就报错;语言包缺 key、
//! 多 key、占位符不一致由本模块的测试兜底,`cargo test` 即可发现。
//!
//! 带参消息在 JSON 里写 `{name}` 占位符,经 [`tr_with`] 运行时替换。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

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

    fn to_u8(self) -> u8 {
        match self {
            Self::En => 0,
            Self::Zh => 1,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Zh,
            // Unknown byte → English, matching `resolve`'s ultimate fallback.
            _ => Self::En,
        }
    }
}

/// Process-shared, lock-free UI language. Mirrors `SharedPermissionMode`: the
/// runtime holds it and reads it per user-facing string; the TUI flips it on
/// `/lang` (through the runtime handle), so a live switch reaches
/// runtime-rendered text — error diagnostics, approval previews — without
/// relaunching the runtime.
#[derive(Debug, Clone)]
pub struct SharedLang(Arc<AtomicU8>);

impl SharedLang {
    #[must_use]
    pub fn new(lang: Lang) -> Self {
        Self(Arc::new(AtomicU8::new(lang.to_u8())))
    }

    #[must_use]
    pub fn get(&self) -> Lang {
        Lang::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, lang: Lang) {
        self.0.store(lang.to_u8(), Ordering::Relaxed);
    }
}

impl Default for SharedLang {
    fn default() -> Self {
        Self::new(Lang::En)
    }
}

/// 定义 [`TextId`] 及其全量列表:一处声明,枚举与 `ALL` 数组不会漂移。
macro_rules! text_ids {
    ($($name:ident),+ $(,)?) => {
        /// 一条界面文案的 key。变体名即语言包 JSON 的 key。
        /// `Serialize`/`Deserialize` 以变体名出入,让结构化文案(如审批安全
        /// 提示)可随 `ApprovalRequest` 走 serve 线路。
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
        )]
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
    PrefixFirstTurn,
    PrefixStable,
    PrefixChanged,
    // /lang
    LangCurrent,
    LangName,
    LangSwitched,
    LangUnknown,
    // AgentError 用户可见诊断(agent crate)
    ErrMissingApiKey,
    ErrHttp,
    ErrApiUnauthorized,
    ErrApiRateLimited,
    ErrApiServer,
    ErrApiGeneric,
    ErrParse,
    ErrSerde,
    ErrRequestTimeout,
    ErrStreamStalled,
    ErrStreamDeadline,
    ErrStreamOverflow,
    // web 工具错误(web_tools.rs)
    WebUrlParseError,
    WebHttpStatus,
    WebReadBodyError,
    WebContentTruncated,
    WebSearchUrlError,
    WebSearchRequestError,
    WebSearchHttpStatus,
    WebSearchReadError,
    WebSearchNoResults,
    WebClientInitError,
    WebRequestError,
    WebRedirectInvalid,
    WebRedirectLimit,
    WebSchemeNotAllowed,
    WebUrlNoHost,
    WebPrivateHostBlocked,
    WebHostResolveError,
    WebHostNoAddrs,
    WebPrivateResolvedBlocked,
    // 配置加载警告(layers.rs)
    CfgFileUnusable,
    CfgProjectApiKeyIgnored,
    CfgProjectBaseUrlOverride,
    CfgUnknownReasoning,
    CfgProjectFieldIgnored,
    CfgUnknownCurrency,
    CfgProjectAutoAllowIgnored,
    CfgGlobalKeyPerms,
    // 配置写入(write.rs)
    CfgApiKeyEmpty,
    CfgApiKeyWhitespace,
    CfgApiKeyTooShort,
    CfgReadFailed,
    CfgParseFailed,
    CfgDirCreateFailed,
    CfgWriteFailed,
    CfgReplaceFailed,
    CfgTemplateHeader,
    // 审批 diff 预览(approval_preview.rs)
    PreviewNewFile,
    PreviewMoreLines,
    PreviewFileTooBig,
    PreviewReadFail,
    PreviewNoChange,
    PreviewNotUtf8,
    // shell 命令安全提示(execution_policy/shell_deny.rs)
    SafetyRedirectReason,
    SafetyRedirectSuggestion,
    SafetyPathOutsideReason,
    SafetyPathOutsideSuggestion,
    SafetyNetworkReason,
    SafetyNetworkSuggestion,
    SafetyDeleteReason,
    SafetyDeleteSuggestion,
    SafetyChmodReason,
    SafetyChmodSuggestion,
    SafetyGitRemoteReason,
    SafetyGitRemoteSuggestion,
    SafetyInstallReason,
    SafetyInstallSuggestion,
    // 运行时其它用户可见提示
    CheckpointSnapshotFailed,
    // 离线 echo 后端提示
    EchoOfflineHint,
    // 路由遥测原因(auto_mode / streaming),显示在状态栏
    RouteFixedModel,
    RouteFixedModelPassthrough,
    RouteCascade,
    RouteContextPressure,
    RouteKeywordDeep,
    RouteKeywordHeavy,
    RouteKeywordBorderline,
    RouteFlashDefault,
    RouteFallbackProToFlash,
    // 权限模式(Shift+Tab 循环 + 状态栏标识)
    PermModeDefault,
    PermModeAcceptEdits,
    PermModeAuto,
    PermModeYolo,
    PermModeSwitched,
    PermModeYoloArm,
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

/// 查表并替换 `{name}` 占位符。单趟扫描:替换进去的值不会被后续参数再次匹配
/// (值里若恰好含 `{other}` 会原样保留),避免顺序替换导致的二次插值。
#[must_use]
pub fn tr_with(lang: Lang, id: TextId, args: &[(&str, &str)]) -> String {
    let template = tr(lang, id);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('}') {
            let name = &after[..close];
            match args.iter().find(|(key, _)| *key == name) {
                Some((_, value)) => out.push_str(value),
                // 未知占位符:原样保留 `{name}`。
                None => {
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
            }
            rest = &after[close + 1..];
        } else {
            // 未闭合的 `{`:剩余部分原样输出。
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
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
    fn tr_with_does_not_resubstitute_values() {
        // A value that itself contains another arg's placeholder token must be
        // emitted verbatim, not re-expanded by that later arg (single-pass).
        let out = tr_with(
            Lang::En,
            TextId::ModelSwitched,
            &[("model", "{backend}"), ("backend", "deepseek")],
        );
        assert!(out.contains("{backend}"), "value re-substituted: {out}");
    }

    #[test]
    fn shared_lang_get_set_and_shares_atomic() {
        let shared = SharedLang::new(Lang::En);
        assert_eq!(shared.get(), Lang::En);
        shared.set(Lang::Zh);
        assert_eq!(shared.get(), Lang::Zh);
        // Clones share one atomic, so a TUI-side `/lang` flip is visible to the
        // runtime that holds a clone.
        let clone = shared.clone();
        clone.set(Lang::En);
        assert_eq!(shared.get(), Lang::En);
        // Unknown byte fails safe to English.
        assert_eq!(Lang::from_u8(9), Lang::En);
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
