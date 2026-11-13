//! `/internal/*` handler behind the private API Gateway: the caller is always a
//! `harness-worker-session` role session `<run>.<agent>`, derived from the request context.

pub mod aws;
pub mod caller;
pub mod creds;
pub mod handler;
pub mod lambda;
