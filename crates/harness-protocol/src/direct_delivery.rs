//! How a run delivers its work, as the worker tells the engine, and the words the harness has for it.
//!
//! The rules every run gets say that a delivery to a repository (a pull request) refuses a run that adds or changes a file under
//! `.github/workflows/` or a root `buildspec.yml`, and that a run that delivers only a zip takes such files. Which of the two a run is follows
//! from its target, and an engine that was asked for a workflow file could not tell whether to write it. The worker therefore says it to the
//! engine at the start of every session, in one short block of the harness's own words.
//!
//! Where the worker reads it: from the target of the stored request ([`DirectDelivery::of_target`]), which every
//! worker receives and whose SHA-256 the invocation names. control-api writes the stored request and the Run item from one request, and the
//! delivery plan tells the bundle validator the target of the Run item, so the words at the start of a session and the check at its end
//! cannot differ. The service of protocol 1.61.0 wrote the fact into the invocation instead (`DirectInvocationRequest.delivery`), where a
//! worker image of an earlier release refused it as a member it did not know: no run started after a rollback of the worker image alone.
//! This version's service writes no such member; the worker still reads one (a service of 1.61.0 sends it), checks it ([`check_delivery`],
//! called by [`crate::direct::validate_direct_invocation`]) and says nothing when it differs from the stored target.
//!
//! The user's switch (`StartDirectRunRequest.deliveryNote`, the key `run.delivery_note`): `false` and the worker says nothing
//! ([`delivery_to_say`]).
//!
//! It is the harness's own fact about the run. It is no outcome label and decides nothing about when the work is done.
//!
//! Everything here is re-exported from [`crate::direct`].

use crate::target::{RepoTarget, RunTarget};
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

    /// How a run with `target` delivers: a zip alone for the zip target, else a pull request to the repository as the user named
    /// it (`cc:<name>`). The worker reads the target of the stored request; the delivery plan reads the target of the Run item when it tells
    /// the bundle validator `target: zip`. control-api writes both from one request (`create_run`).
    pub fn of_target(target: &RunTarget) -> Self {
        match target {
            RunTarget::S3Only => Self::zip(),
            RunTarget::Repo { repo } | RunTarget::NewRepo { repo } => {
                Self::pull_request(repo.to_string())
            }
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

/// What the worker tells the engine about the delivery of a run, or `None` when it tells nothing.
///
/// * `delivery_note` is the stored request's `deliveryNote`: `false` is the user's switch, and nothing is said.
/// * `target` is the stored request's target: it decides what is said ([`DirectDelivery::of_target`]), when [`check_delivery`] takes it.
/// * `invocation` is the invocation's `delivery`, which only a service of protocol 1.61.0 writes. It comes from the same target, so it
///   agrees; when it does not, the worker has two sources that differ, stands behind neither and says nothing.
pub fn delivery_to_say(
    invocation: Option<&DirectDelivery>,
    target: &RunTarget,
    delivery_note: Option<bool>,
) -> Option<DirectDelivery> {
    if delivery_note == Some(false) {
        return None;
    }
    let said = DirectDelivery::of_target(target);
    if !check_delivery(&said) {
        return None;
    }
    match invocation {
        Some(member) if *member != said => None,
        _ => Some(said),
    }
}

/// The most characters of `deliveryNote` in `summary.json` and in the evidence: the block is one line of at most 400 characters and a
/// repository target of at most [`MAX_DELIVERY_REPO_BYTES`] bytes.
pub const MAX_DELIVERY_NOTE_CHARS: usize = 1_000;

/// Whether `note` is a text the evidence shows as what the engine was told: one line with no control character, not empty, of at most
/// [`MAX_DELIVERY_NOTE_CHARS`] characters.
pub fn delivery_note_ok(note: &str) -> bool {
    !note.is_empty()
        && note.chars().count() <= MAX_DELIVERY_NOTE_CHARS
        && !note.chars().any(char::is_control)
}

/// The longest path [`zip_holds`] names; a longer one is cut and ends with three dots.
pub const ZIP_NOTICE_PATH_CHARS: usize = 120;

/// A path as the notice shows it: the engine chose the file name, so a control character becomes a space and a long path is cut.
fn shown_path(path: &str) -> String {
    let clean: String = path
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() > ZIP_NOTICE_PATH_CHARS {
        let head: String = clean.chars().take(ZIP_NOTICE_PATH_CHARS).collect();
        format!("{head}...")
    } else {
        clean
    }
}

/// What a delivered zip holds that a repository would have refused, in the words the user reads: one sentence for the workflow
/// files and one for the root build specification, each saying that the zip holds it and when it would run. One path is named and the rest
/// are counted. `None` when the run changed no such file. The run card shows it at the end of its Detail (run-session `finalize`, from the
/// bundle validator's findings). The packager reads the same changes from the bundle it packages and puts the sentence at the end
/// of the Detail row of EVIDENCE.md and into the zip's copy of `evidence/direct.json`, as the member `zipHolds`.
pub fn zip_holds(workflows: &[String], buildspecs: &[String]) -> Option<String> {
    let mut sentences: Vec<String> = Vec::new();
    if let Some(first) = workflows.first() {
        let first = shown_path(first);
        sentences.push(match workflows.len() {
            1 => format!(
                "The zip holds a workflow file ({first}); it runs only if the code is pushed to a git host that runs such files."
            ),
            n => format!(
                "The zip holds {n} workflow files ({first} and {} more); they run only if the code is pushed to a git host that runs such files.",
                n - 1
            ),
        });
    }
    if let Some(first) = buildspecs.first() {
        sentences.push(format!(
            "The zip holds a build specification ({}); a build service runs it only if a build project builds this code.",
            shown_path(first)
        ));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}
