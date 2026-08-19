//! The model-side doorbell for widening the write boundary.
//!
//! `request_write_root` lets the model ask for one more writable directory
//! instead of describing the need in prose and waiting for the user to type
//! `/add-dir`. The tool itself grants nothing: the execution policy pins it
//! to NeedsApproval, the approval gate hard-excludes it from every
//! auto-approval channel (all permission modes including Yolo, config
//! `auto_allow`, session memory), and the runtime performs the actual grant
//! in `handle_approval` only after the human said yes — see
//! `AgentRuntime::apply_root_grant`. [`RequestWriteRootTool::run`] is
//! therefore a defensive stub: in the wired runtime it is unreachable.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{Tool, ToolCx, ToolError, ToolOutput};

pub const REQUEST_WRITE_ROOT_TOOL: &str = "request_write_root";

const DESCRIPTION: &str = "Ask the user to grant write access to ONE directory outside the \
current granted roots, when a task genuinely needs it (e.g. a write or cd was just denied \
there). The user sees the resolved directory and your justification, and decides; approval \
widens the boundary for the rest of the session, denial is final — do not request the same \
path again, and never call this speculatively. path must be an absolute path to an existing \
directory; request the narrowest directory that unblocks the task, never a home directory or \
filesystem root.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestWriteRootParams {
    /// Absolute path to an existing directory; the narrowest one that unblocks the task
    #[allow(dead_code)] // consumed by the runtime approval layer from the raw arguments
    path: String,
    /// One short sentence for the user: why the task needs to write there. Shown as your claim, verbatim.
    #[allow(dead_code)] // surfaced to the approval prompt from the raw arguments
    justification: String,
}

/// Schema-bearing registry entry; the grant itself happens in the runtime.
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestWriteRootTool;

#[async_trait]
impl Tool for RequestWriteRootTool {
    type Params = RequestWriteRootParams;

    fn name(&self) -> &str {
        REQUEST_WRITE_ROOT_TOOL
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, _params: Self::Params, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        // Reached only when a caller drives the registry directly with a
        // pre-supplied Approved decision (the runtime intercepts this tool
        // before execution). The registry layer cannot widen any boundary, so
        // be explicit rather than pretend a grant happened.
        Err(ToolError::exec_failed(
            REQUEST_WRITE_ROOT_TOOL,
            "write-root grants are performed by the session runtime after user approval; \
             this registry has no runtime attached, so no grant was made",
        ))
    }
}
