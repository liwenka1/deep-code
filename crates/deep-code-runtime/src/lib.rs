//! Local HTTP/SSE runtime API. Surface kept deliberately small: the only
//! production consumer is headless automation (CI bot) driving
//! `/v1/prompt` + `/v1/approvals` with a bearer token.

mod auth;
mod events;
mod server;

pub use auth::RUNTIME_TOKEN_ENV;
pub use events::{EnvelopeStream, RuntimeEnvelope, RuntimeItem};
pub use server::{RuntimeServerOptions, run_http_server};
