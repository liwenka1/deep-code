//! Local HTTP/SSE runtime API. Surface kept deliberately small: the only
//! production consumer is headless automation (CI bot) driving
//! `/prompt` + `/approvals` with a bearer token.
//!
//! `/prompt` accepts only `{ "prompt": "text" }`. SSE tool results preserve
//! ordered text/image content arrays and structured execution details. Image
//! assets in output identify local snapshots; they are not an upload API.

mod auth;
mod events;
mod server;

pub use auth::RUNTIME_TOKEN_ENV;
pub use events::{EnvelopeStream, RuntimeEnvelope, RuntimeItem};
pub use server::{RuntimeServerOptions, run_http_server};
