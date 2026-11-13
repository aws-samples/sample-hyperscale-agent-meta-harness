//! Pure harness logic that direct runs share: the diagnostic of an engine process that
//! ended (its exit and a scrubbed line of its stderr, inside a failed turn's error text), the secret
//! redaction that diagnostic uses, and engine-profile loading (`engine/profiles/*.toml`, bundled at
//! build time).
//!
//! `engine_profile` reads `harness-protocol` types; the other modules use crate-local types.

pub mod engine_diagnostic;
pub mod engine_profile;
pub mod redact;
pub mod refusal;
