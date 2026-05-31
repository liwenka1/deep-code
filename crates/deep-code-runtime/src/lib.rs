//! Local HTTP/SSE runtime API for external GUIs and supervisors.

mod auth;
mod meta;
mod server;
mod sessions;
mod threads;

pub use auth::RUNTIME_TOKEN_ENV;
pub use server::{RuntimeServerOptions, run_http_server};
pub use sessions::ActiveSessionResponse;
pub use threads::{RuntimeEnvelope, RuntimeItem, RuntimeThread, RuntimeThreadDetail, RuntimeTurn};
