//! Local HTTP/SSE runtime API for external GUIs and supervisors.

mod auth;
mod server;

pub use auth::RUNTIME_TOKEN_ENV;
pub use server::{RuntimeServerOptions, run_http_server};
