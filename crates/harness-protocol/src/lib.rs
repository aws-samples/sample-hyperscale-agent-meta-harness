//! Wire types shared by every harness component: event envelope, `/v1` and `/internal` API types, store
//! items, the model catalog row, evidence records, engine profiles, the direct-run contract, the tool
//! table of the harness's tool server ([`tools`]) and the AgentCore Runtime contract.
//!
//! The crate does no I/O. [`export`] renders the contract artifacts under `schemas/`; the
//! `export-schemas` binary writes them.

/// The `accounts.json` selection rule the build script applies.
pub mod accounts;
pub mod api;
pub mod catalog;
pub mod common;
pub mod direct;
pub mod direct_attachments;
pub mod direct_build;
pub mod direct_compute;
pub mod direct_continue;
pub mod direct_engine_config;
pub mod direct_flow;
pub mod direct_goal;
pub mod direct_input;
pub mod direct_limits;
pub mod direct_request;
pub mod direct_review;
pub mod direct_skills;
pub mod engine_profile;
pub mod envelope;
pub mod evidence;
pub mod export;
pub mod goal;
pub mod messages;
pub mod orchestration;
pub mod output_pins;
pub mod research;
/// The exact-copy rule for stored third-party files.
pub mod research_copy;
pub mod runtime;
pub mod store;
pub mod target;
pub mod test_report;
pub mod tools;
pub mod transcript;

/// Protocol version; bump it with any contract-artifact change after the 80% checkpoint.
pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");
