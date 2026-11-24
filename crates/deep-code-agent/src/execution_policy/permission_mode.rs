//! Session permission modes resolve eligible approval requests.
//!
//! Policy denials take precedence over every mode. Write-root grants and
//! other `HumanOnly` requests require an explicit decision. The command
//! deny list recognizes known command shapes. Containment depends on the
//! platform. See [what is behind a command nobody read](super#what-is-behind-a-command-nobody-read).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// Ordered strictest → most permissive; [`cycle`](PermissionMode::cycle) walks
/// the ring (bound to Shift+Tab in the TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Prompt on every gated call that no standing consent (`auto_allow`, a
    /// session "a") already covers. The historical default.
    #[default]
    Default,
    /// Auto-approve workspace file edits, the dispatch of a writing sub-agent
    /// (its spawn is the write authorization) and filesystem-shaped shell/job
    /// commands (`mkdir`/`touch`/`mv`/`cp`/`rm`/`rmdir` by program name, with
    /// every operand a relative in-tree spelling — the sandbox bounds their
    /// writes, the spelling rule bounds the reads a `cp` from outside would
    /// make; cc's `acceptEdits`); still prompt for other shell commands, any
    /// network declaration, and everything else. A listed program counts only
    /// when this host can run it as argv: an unattended command runs without a
    /// shell, so a name that resolves to a `cmd` builtin (Windows' `mkdir`) or
    /// to a `.cmd`/`.bat` wrapper keeps PROMPTING instead of being auto-approved
    /// into a spawn that would fail (`command_runnable_unattended` in
    /// `runtime/approval_flow.rs`).
    AcceptEdits,
    /// Auto-approve eligible requests and give sandboxed commands ambient
    /// network access when the sandbox network mode permits it.
    Yolo,
}

impl PermissionMode {
    /// Next mode in the Shift+Tab ring.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Yolo,
            Self::Yolo => Self::Default,
        }
    }

    /// Config/persistence token.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept_edits",
            Self::Yolo => "yolo",
        }
    }

    /// The localization key for this mode's status-chip label. On the enum so
    /// the label mapping has one home next to the variants (the accent colour
    /// stays in the TUI, which owns ratatui).
    #[must_use]
    pub fn text_id(self) -> crate::i18n::TextId {
        match self {
            Self::Default => crate::i18n::TextId::PermModeDefault,
            Self::AcceptEdits => crate::i18n::TextId::PermModeAcceptEdits,
            Self::Yolo => crate::i18n::TextId::PermModeYolo,
        }
    }

    /// Parse a config/CLI value (tolerant of `-`/`_` and case).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "default" => Some(Self::Default),
            "accept_edits" | "acceptedits" | "edits" => Some(Self::AcceptEdits),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    /// Rank on the permissiveness ladder (`Default` ⊂ `AcceptEdits` ⊂ `Yolo`).
    /// The config merge uses this rank to accept project settings that tighten
    /// the user's permission mode.
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::AcceptEdits => 1,
            Self::Yolo => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::AcceptEdits,
            2 => Self::Yolo,
            // Unknown byte falls back to the strictest mode — never silently
            // more permissive.
            _ => Self::Default,
        }
    }
}

/// Process-shared, lock-free current mode. The TUI reads it every frame (for
/// the status indicator) and flips it on Shift+Tab; the runtime gate reads it
/// per gated call. An atomic (not an async mutex) keeps the per-frame UI read
/// cheap and lets the sync key handler toggle it without blocking.
#[derive(Debug, Clone)]
pub struct SharedPermissionMode(Arc<AtomicU8>);

impl SharedPermissionMode {
    #[must_use]
    pub fn new(mode: PermissionMode) -> Self {
        Self(Arc::new(AtomicU8::new(mode.to_u8())))
    }

    #[must_use]
    pub fn get(&self) -> PermissionMode {
        PermissionMode::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, mode: PermissionMode) {
        self.0.store(mode.to_u8(), Ordering::Relaxed);
    }

    /// Advance to the next mode in the ring and return it.
    pub fn cycle(&self) -> PermissionMode {
        let next = self.get().cycle();
        self.set(next);
        next
    }
}

impl Default for SharedPermissionMode {
    fn default() -> Self {
        Self::new(PermissionMode::Default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_is_a_three_stop_ring() {
        let mut m = PermissionMode::Default;
        m = m.cycle();
        assert_eq!(m, PermissionMode::AcceptEdits);
        m = m.cycle();
        assert_eq!(m, PermissionMode::Yolo);
        m = m.cycle();
        assert_eq!(m, PermissionMode::Default);
    }

    #[test]
    fn parse_roundtrips_setting_and_tolerates_aliases() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Yolo,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_setting()), Some(mode));
        }
        assert_eq!(
            PermissionMode::parse("accept-edits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(PermissionMode::parse("YOLO"), Some(PermissionMode::Yolo));
        assert_eq!(PermissionMode::parse("auto"), None);
        assert_eq!(PermissionMode::parse("nonsense"), None);
    }

    /// `as_setting` is also the serde spelling: the config file, the wire
    /// events and the diagnostics name a tier the same way, so a variant rename
    /// cannot drift one without the other.
    #[test]
    fn setting_spelling_is_the_serde_spelling() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Yolo,
        ] {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::Value::String(mode.as_setting().to_string()),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn shared_get_set_cycle() {
        let shared = SharedPermissionMode::new(PermissionMode::Default);
        assert_eq!(shared.get(), PermissionMode::Default);
        assert_eq!(shared.cycle(), PermissionMode::AcceptEdits);
        assert_eq!(shared.get(), PermissionMode::AcceptEdits);
        shared.set(PermissionMode::Yolo);
        assert_eq!(shared.get(), PermissionMode::Yolo);
        // Clones share the same underlying atomic.
        let clone = shared.clone();
        clone.set(PermissionMode::Default);
        assert_eq!(shared.get(), PermissionMode::Default);
    }

    #[test]
    fn unknown_byte_falls_back_to_strictest() {
        // Not reachable via the safe API, but the mapping must fail safe.
        assert_eq!(PermissionMode::from_u8(99), PermissionMode::Default);
    }
}
