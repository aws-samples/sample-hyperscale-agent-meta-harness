//! `DirectInvocationRequest.delivery`: how the run delivers its work, as the service tells the worker.
//!
//! The rules every run gets say that a delivery to a repository (a pull request) refuses a run that adds or changes a file under
//! `.github/workflows/` or a root `buildspec.yml`, and that a run that delivers only a zip takes such files. Only the service knows which of
//! the two a run is (its target), so an engine that was asked for a workflow file could not tell whether to write it. The service now puts
//! the fact into the invocation of every session, the worker checks it again ([`check_delivery`], called by
//! [`crate::direct::validate_direct_invocation`]) and says it to the engine at the start of every session.
//!
//! It is the harness's own fact about the run. It is no outcome label and decides nothing about when the work is done.
//!
//! Everything here is re-exported from [`crate::direct`].

use crate::target::RepoTarget;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// The two paths a delivery to a repository refuses and a zip takes: a file under the first, and the file at the root named by the second.
/// The bundle validator protects them (`crates/delivery/src/validator.rs`), the default rules name them
/// (`crates/ham-core/assets/default-rules.md`, section "Delivery") and the worker's words to the engine repeat them; each has a test
/// against this list.
pub const REPOSITORY_REFUSES: [&str; 2] = [".github/workflows/", "buildspec.yml"];

/// The most bytes of [`DirectDelivery::repo`]: the longest repository target (`gl:` and a path) with room to spare.
pub const MAX_DELIVERY_REPO_BYTES: usize = 300;

/// What a run delivers.
// The variants carry plain comments, not doc comments, so that the schema keeps one `enum` of the two words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DirectDeliveryKind {
    // A zip alone: the run's target is `s3-only`.
    Zip,
    // A pull request to a repository, with the zip beside it: the run's target is a repository or a new repository.
    PullRequest,
}

/// How the run delivers its work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectDelivery {
    pub kind: DirectDeliveryKind,
    /// The repository of a pull request, as the user named it (`cc:<name>`). Required for `pull-request`; absent for `zip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl DirectDelivery {
    /// A run that delivers a zip alone.
    pub fn zip() -> Self {
        Self {
            kind: DirectDeliveryKind::Zip,
            repo: None,
        }
    }

    /// A run that delivers a pull request to `repo`, written as the user named it (`cc:<name>`).
    pub fn pull_request(repo: impl Into<String>) -> Self {
        Self {
            kind: DirectDeliveryKind::PullRequest,
            repo: Some(repo.into()),
        }
    }
}

/// Whether `d` is a delivery a worker may say to an engine: a zip that names no repository, or a pull request whose repository is a
/// repository target (one line of the characters a target has, at most [`MAX_DELIVERY_REPO_BYTES`] bytes).
pub fn check_delivery(d: &DirectDelivery) -> bool {
    match (d.kind, d.repo.as_deref()) {
        (DirectDeliveryKind::Zip, None) => true,
        (DirectDeliveryKind::PullRequest, Some(repo)) => {
            repo.len() <= MAX_DELIVERY_REPO_BYTES && RepoTarget::from_str(repo).is_ok()
        }
        _ => false,
    }
}
