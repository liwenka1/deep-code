//! Session permission mode: how far the approval gate is relaxed.
//!
//! A mode only ever relaxes a `NeedsApproval` verdict into an auto-run. A hard
//! `Deny` (rm -rf, fork bomb, `curl | sh`, …) is short-circuited in the policy
//! engine before any decision is consulted, so no mode — not even `Yolo` —
//! runs a command the deny list *recognized*. Be honest about what that
//! buys: `shell_deny` is best-effort string parsing, not a security boundary.
//! An obfuscation it fails to parse falls through as `NeedsApproval`, which
//! `Yolo` auto-approves — so under `Yolo` the real containment is the OS
//! sandbox (where enabled), not this floor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// Ordered strictest → most permissive; [`cycle`](PermissionMode::cycle) walks
/// the ring (bound to Shift+Tab in the TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Prompt on every gated call. The historical default.
    #[default]
    Default,
    /// Auto-approve workspace file edits (and cc-style in-workspace fs commands);
    /// still prompt for shell/network/anything else.
    AcceptEdits,
    /// Everything `AcceptEdits` waves through, plus a cheap classifier model
    /// judging the rest — behind three floors it cannot override: a root grant
    /// and any egress (a declared `network: true`, or a network-native tool)
    /// ask a human, and a High-tier call never reaches the judge — it asks,
    /// unless the inherited `AcceptEdits` allowance already covers it (an
    /// untrusted `mkdir src/x` is High by default and runs unasked here just
    /// as it does there). Unsure/hostile/error → prompt. The order is drawn in
    /// the `execution_policy` module docs and lives in
    /// `runtime::approval_flow::auto_mode_approves`.
    Auto,
    /// Auto-approve everything that reaches the gate (hard denies still block).
    Yolo,
}

impl PermissionMode {
    /// Next mode in the Shift+Tab ring.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Auto,
            Self::Auto => Self::Yolo,
            Self::Yolo => Self::Default,
        }
    }

    /// Config/persistence token.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept_edits",
            Self::Auto => "auto",
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
            Self::Auto => crate::i18n::TextId::PermModeAuto,
            Self::Yolo => crate::i18n::TextId::PermModeYolo,
        }
    }

    /// Parse a config/CLI value (tolerant of `-`/`_` and case).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "default" => Some(Self::Default),
            "accept_edits" | "acceptedits" | "edits" => Some(Self::AcceptEdits),
            "auto" => Some(Self::Auto),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    /// Rank on the permissiveness ladder (`Default` ⊂ `AcceptEdits` ⊂ `Auto` ⊂
    /// `Yolo`). Also used to enforce tighten-only config merges: an untrusted
    /// layer may lower the tier, never raise it.
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::AcceptEdits => 1,
            Self::Auto => 2,
            Self::Yolo => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::AcceptEdits,
            2 => Self::Auto,
            3 => Self::Yolo,
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
    fn cycle_is_a_four_stop_ring() {
        let mut m = PermissionMode::Default;
        m = m.cycle();
        assert_eq!(m, PermissionMode::AcceptEdits);
        m = m.cycle();
        assert_eq!(m, PermissionMode::Auto);
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
            PermissionMode::Auto,
            PermissionMode::Yolo,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_setting()), Some(mode));
        }
        assert_eq!(
            PermissionMode::parse("accept-edits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(PermissionMode::parse("YOLO"), Some(PermissionMode::Yolo));
        assert_eq!(PermissionMode::parse("nonsense"), None);
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
        clone.set(PermissionMode::Auto);
        assert_eq!(shared.get(), PermissionMode::Auto);
    }

    #[test]
    fn unknown_byte_falls_back_to_strictest() {
        // Not reachable via the safe API, but the mapping must fail safe.
        assert_eq!(PermissionMode::from_u8(99), PermissionMode::Default);
    }
}
