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

use crate::model::ToolCallPayload;
use crate::tool::ToolResultStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    #[serde(flatten)]
    pub kind: EntryKind,
}

impl SessionEntry {
    #[must_use]
    pub fn new(kind: EntryKind) -> Self {
        Self { kind }
    }

    /// Number of wire messages this entry derives to (an assistant entry
    /// emits one tool message per exchange). Consumers report this count so
    /// it stays stable across the v1→v2 schema migration.
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
        Self::new(EntryKind::User {
            content: content.into(),
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
    /// Trimmed model-facing content; the full result lives in the turn's
    /// [`crate::session_store::TurnRecord`] (dual-copy design).
    pub content: String,
    pub status: ToolResultStatus,
}
