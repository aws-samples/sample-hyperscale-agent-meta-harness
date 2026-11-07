//! The wire types of `POST /v1/tags/edit`: one batch of at most 40 edits, one result for each in request order.
//!
//! These are `ham-core` types, not part of the frozen protocol (`harness-protocol` and `schemas/` do not change): the route and the
//! client both depend on `ham-core`, like the run row. The route reads the owner from the authenticated caller; the request never
//! names one.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The most edits in one request.
pub const MAX_EDITS: usize = 40;
/// The most tags in one edit, `add` and `remove` together.
pub const MAX_TAGS_PER_EDIT: usize = 12;
/// The longest run id the route reads.
pub const RUN_ID_MAX: usize = 64;

/// One run's edit. `add` and `remove` may be left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagEdit {
    pub run_id: String,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// The request body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagEditRequest {
    pub edits: Vec<TagEdit>,
}

/// Why a request as a whole is refused (a 400). A bad tag is not one of these: it makes that one edit `invalid` and the others go on.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("at most {max} edits in one request (got {got})")]
    TooManyEdits { max: usize, got: usize },
    #[error("at most {max} tags in one edit, add and remove together (run {run_id} has {got})")]
    TooManyTags {
        run_id: String,
        max: usize,
        got: usize,
    },
    #[error("a run id is 1 to 64 letters, digits, hyphens or underscores (got {run_id:?})")]
    BadRunId { run_id: String },
    #[error("run {run_id} appears in two edits of one request")]
    DuplicateRunId { run_id: String },
}

impl TagEditRequest {
    /// The limits of the route: edits in the request, tags in an edit, plain run ids, no run twice in one batch (two edits of one run
    /// would race each other).
    pub fn validate(&self) -> Result<(), WireError> {
        if self.edits.len() > MAX_EDITS {
            return Err(WireError::TooManyEdits {
                max: MAX_EDITS,
                got: self.edits.len(),
            });
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for edit in &self.edits {
            let id = edit.run_id.as_str();
            let plain = !id.is_empty()
                && id.len() <= RUN_ID_MAX
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !plain {
                return Err(WireError::BadRunId {
                    run_id: edit.run_id.clone(),
                });
            }
            let tags = edit.add.len() + edit.remove.len();
            if tags > MAX_TAGS_PER_EDIT {
                return Err(WireError::TooManyTags {
                    run_id: edit.run_id.clone(),
                    max: MAX_TAGS_PER_EDIT,
                    got: tags,
                });
            }
            if !seen.insert(id) {
                return Err(WireError::DuplicateRunId {
                    run_id: edit.run_id.clone(),
                });
            }
        }
        Ok(())
    }
}

/// What happened to one run's edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditStatus {
    /// The lists changed; `tags` is the effective list after the edit.
    Ok,
    /// The edit changed nothing (a replay); `tags` is the effective list.
    Unchanged,
    /// A tag broke a rule, or a cap was reached; `message` says which rule. Nothing was written.
    Invalid,
    /// The run does not exist or belongs to another caller. Nothing was written.
    NotFound,
    /// The run's lists changed under the edit twice; try again. Nothing was written.
    Conflict,
}

/// One run's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagEditResult {
    pub run_id: String,
    pub status: EditStatus,
    /// The effective tags after the edit; empty for `invalid`, `not-found` and `conflict`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The rule text when the edit is `invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl TagEditResult {
    fn new(run_id: &str, status: EditStatus, tags: Vec<String>, message: Option<String>) -> Self {
        Self {
            run_id: run_id.to_string(),
            status,
            tags,
            message,
        }
    }

    pub fn ok(run_id: &str, tags: Vec<String>) -> Self {
        Self::new(run_id, EditStatus::Ok, tags, None)
    }

    pub fn unchanged(run_id: &str, tags: Vec<String>) -> Self {
        Self::new(run_id, EditStatus::Unchanged, tags, None)
    }

    pub fn invalid(run_id: &str, message: &str) -> Self {
        Self::new(
            run_id,
            EditStatus::Invalid,
            Vec::new(),
            Some(message.to_string()),
        )
    }

    pub fn not_found(run_id: &str) -> Self {
        Self::new(run_id, EditStatus::NotFound, Vec::new(), None)
    }

    pub fn conflict(run_id: &str) -> Self {
        Self::new(run_id, EditStatus::Conflict, Vec::new(), None)
    }
}

/// The response body: one result per edit, in request order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagEditResponse {
    pub results: Vec<TagEditResult>,
}
