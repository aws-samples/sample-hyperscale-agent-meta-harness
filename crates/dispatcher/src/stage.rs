//! The account and home Region of the `accounts.json` entry the build selected, and the Regions a
//! cross-Region inference profile may route to.

/// The account and home Region of the `accounts.json` entry the build selected.
pub use harness_protocol::target::{E2E_ACCOUNT as ACCOUNT, E2E_REGION as REGION};
/// Regions a cross-Region inference profile may route to (infra `INFERENCE_REGIONS`).
pub const INFERENCE_REGIONS: [&str; 3] = ["us-west-2", "us-east-1", "us-east-2"];
