//! The build settings of a direct run.
//!
//! Every session starts with three defaults that suit a small machine and that a person may not want:
//!
//! - cargo builds the dev and test profiles without debug information (`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`), which makes
//!   `target/` smaller and links faster;
//! - cargo builds without incremental state (`CARGO_INCREMENTAL=0`);
//! - the repository's local exclude file lists `target/`, so the harness's end-of-run commit never tracks build output (cargo's, and the Maven
//!   or Gradle folder of the same name).
//!
//! `StartDirectRunRequest.build` ([`BuildOptions`]) lets the user turn each one off. Every member is a switch that is on when absent; `ham` sends
//! a member only as `false`, so a user who never touches the settings sends the request of an older client byte for byte.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `StartDirectRunRequest.build` (protocol 1.48.0): the user's switches for the session's build defaults. The request type refuses an unknown
/// member and a value that is not a boolean before anything is stored, so there is no separate check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildOptions {
    /// `false` leaves cargo's own debug-information setting: the harness sets neither `CARGO_PROFILE_DEV_DEBUG` nor `CARGO_PROFILE_TEST_DEBUG`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_no_debug_info: Option<bool>,
    /// `false` leaves cargo's own incremental setting: the harness does not set `CARGO_INCREMENTAL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_no_incremental: Option<bool>,
    /// `false` leaves `target/` out of the exclude list: a `target/` folder the repository does not ignore is committed with the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_target_dir: Option<bool>,
}

impl BuildOptions {
    /// Whether the member says nothing, so that `ham` leaves it out of the request.
    pub fn is_empty(&self) -> bool {
        self.cargo_no_debug_info.is_none()
            && self.cargo_no_incremental.is_none()
            && self.exclude_target_dir.is_none()
    }

    /// Whether the session builds cargo's dev and test profiles without debug information (on unless the member says `false`).
    pub fn cargo_no_debug_info(&self) -> bool {
        self.cargo_no_debug_info != Some(false)
    }

    /// Whether the session builds without incremental state (on unless the member says `false`).
    pub fn cargo_no_incremental(&self) -> bool {
        self.cargo_no_incremental != Some(false)
    }

    /// Whether `target/` is on the exclude list (on unless the member says `false`).
    pub fn exclude_target_dir(&self) -> bool {
        self.exclude_target_dir != Some(false)
    }
}
