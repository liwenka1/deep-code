//! Narrow handle store for large sub-agent transcripts and RLM artifacts.

mod read;
mod store;

pub use read::{HANDLE_READ_TOOL, HandleReadTool, register_handle_read};
pub use store::{
    HandleCount, HandleId, HandleKind, HandleReadOutput, HandleRecord, HandleStore, HandleSummary,
    VarHandle,
};
