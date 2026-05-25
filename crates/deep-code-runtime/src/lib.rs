//! Local HTTP/SSE runtime API for external GUIs and supervisors.

mod auth;
mod meta;
mod server;
mod sessions;

pub use auth::RUNTIME_TOKEN_ENV;
pub use server::{RuntimeServerOptions, run_http_server};
pub use sessions::ActiveSessionResponse;
