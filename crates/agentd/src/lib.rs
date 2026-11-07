//! In-microVM supervisor for one AgentCore session:
//! `/ping` and `/invocations` on port 8080, the engine launched from its profile over ACP,
//! the `credential_process` helper and event posting to the private internal API. A
//! `kind: "direct"` invocation runs the direct driver (`direct`).
//!
//! Secrets rule: request bodies, credentials and the refresh nonce are never logged, echoed in an
//! error or returned; only `agentd creds` prints credentials, to the engine's SDK.

use std::future::Future;
use std::pin::Pin;

pub mod acp;
pub mod config;
pub mod creds;
pub mod creds_socket;
pub mod direct;
pub mod emit;
pub mod engine;
pub mod events;
pub mod health;
pub mod http;
pub mod logging;
pub mod nofile;
pub mod packages;
pub mod s3;
pub mod sandbox;
pub mod secrets;
pub mod server;
pub mod time;
pub mod turn;
pub mod workspace;

/// A boxed, sendable future: the return type of the object-safe async traits in this crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
