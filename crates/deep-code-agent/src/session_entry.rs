//! Domain-level session entries.
//!
//! A [`SessionEntry`] is what a conversation *means* (a user turn, an
//! assistant turn with its tool exchanges, a compaction marker) — the
//! DeepSeek/OpenAI wire messages are derived from it in
//! [`crate::session::Session::wire_messages`]. Tool calls and their results
//! are paired structurally in [`ToolExchange`], so a "dangling tool call"
//! cannot exist as persisted state: an exchange whose `result` is `None` is
//! simply one that was interrupted, and the wire derivation synthesizes the
//! placeholder message on demand.

use serde::{Deserialize, Serialize};

use std::path::PathBuf;

use crate::model::ToolCallPayload;
use crate::runtime::TurnId;
use crate::tool::ToolResultStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Runtime turn that produced this entry. Session-wide instructions and
    /// compaction summaries do not belong to an individual turn.
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    #[serde(flatten)]
    pub kind: EntryKind,
}

impl SessionEntry {
    #[must_use]
    pub fn new(kind: EntryKind) -> Self {
        Self {
            turn_id: None,
            kind,
        }
    }

    #[must_use]
    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    /// Number of wire messages this entry derives to (an assistant entry
    /// emits one tool message per exchange). Consumers report this count so
    /// the derived wire representation stays stable.
    #[must_use]
    pub fn wire_message_count(&self) -> usize {
        match &self.kind {
            EntryKind::Assistant { exchanges, .. } => 1 + exchanges.len(),
            _ => 1,
        }
    }

    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(EntryKind::System {
            content: content.into(),
        })
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::user_with_images(content, Vec::new())
    }

    /// A user turn that carries images.
    ///
    /// `images` are local paths, not bytes and not URLs. Bytes would put
    /// megabytes into every session file *and* into every save — the persistence
    /// actor rewrites the whole record — while a URL would be a second copy of
    /// data we already have on disk. [`crate::image::hydrate`] reads them when a
    /// request is assembled.
    #[must_use]
    pub fn user_with_images(content: impl Into<String>, images: Vec<PathBuf>) -> Self {
        Self::new(EntryKind::User {
            content: content.into(),
            images,
        })
    }

    #[must_use]
    pub fn assistant(
        content: impl Into<String>,
        reasoning: Option<String>,
        exchanges: Vec<ToolExchange>,
    ) -> Self {
        Self::new(EntryKind::Assistant {
            content: content.into(),
            reasoning: reasoning.filter(|text| !text.is_empty()),
            exchanges,
        })
    }

    #[must_use]
    pub fn compaction(summary: impl Into<String>, archived_count: usize) -> Self {
        Self::new(EntryKind::Compaction {
            summary: summary.into(),
            archived_count,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    System {
        content: String,
    },
    User {
        content: String,
        /// Local image files attached to this turn, in the order the user
        /// attached them.
        ///
        /// Omitted when empty; deserialization treats an omitted value as no
        /// images.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<PathBuf>,
    },
    Assistant {
        content: String,
        /// DeepSeek thinking-mode replay payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exchanges: Vec<ToolExchange>,
    },
    Compaction {
        summary: String,
        archived_count: usize,
    },
}

/// Whether the approval gate put a call in front of a human.
///
/// A type rather than a bare `bool` because `record_tool_result` takes it
/// positionally at ten-odd call sites, and "is this one `true`?" is exactly the
/// question that gets answered wrong in silence — the two channels differ by one
/// word in the middle of an argument list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The request was shown to a human (or to whoever answers on their behalf
    /// for this runtime) and they answered it.
    Asked,
    /// Nobody was asked: the policy never raised the gate, a standing consent
    /// resolved it before anyone was shown anything, or it was refused outright.
    Unasked,
}

/// One tool call and (once recorded) its model-facing result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExchange {
    pub call: ToolCallPayload,
    /// `None` = interrupted before a result was recorded; the wire derivation
    /// synthesizes the placeholder message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ExchangeResult>,
}

impl ToolExchange {
    #[must_use]
    pub fn pending(call: ToolCallPayload) -> Self {
        Self { call, result: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeResult {
    /// Model-facing content, size-bounded by `truncate_tool_output` (head+tail,
    /// middle elided). This is the only persisted copy: the untruncated output
    /// exists solely on the live event stream, so a large tool result cannot be
    /// recovered in full from a session file or `session export`.
    pub content: String,
    pub status: ToolResultStatus,
    /// Whether the approval gate ever put this call in front of a human.
    ///
    /// This is the predicate the live view badges on and the only thing that
    /// keeps a call out of a folded run, so recording it is what lets a resumed
    /// transcript be the transcript the session actually had.
    ///
    /// **A standing consent is not an ask.** `auto_allow`, a session-remembered
    /// command (`a`), AcceptEdits, Auto and Yolo all resolve the gate *before*
    /// anyone is shown anything — the runtime emits `ApprovalResolved` without
    /// ever emitting `ApprovalRequired`, and the UI's badge is driven by the
    /// latter. So those calls record `false`, and they fold exactly as they did
    /// live. Treating them as asks would un-fold every command under the
    /// permission modes chosen precisely to stop asking.
    ///
    /// `true` when the gate put the call in front of a human; `false` when
    /// policy or standing consent resolved it without a prompt.
    pub asked: bool,
}
