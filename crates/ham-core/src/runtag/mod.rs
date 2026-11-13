//! Run tags: the pure core. A tag is `kind:value` display and search text on a run, with the kind `project`, `topic` or `tech`.
//! The utility model writes topic and tech tags when a run starts; the person adds and removes tags on any run; the explorer counts them.
//!
//! Everything here is pure: no network, no AWS client, no clock, no file read at run time. The callers are `run-session` (the tag branch
//! of the `name-run` op), `control-api` (the edit route and the list row) and `ham` (the explorer and `/tag`).
//!
//! - [`grammar`]: what a tag may be, the policy words, the comparison key, the project tag a repository gives.
//! - [`effective`]: the one function that says which tags a run shows: derived project, then the person's tags, then the automatic
//!   tags, minus the tags the person removed. The person's removal wins on every read.
//! - [`vocab`]: the person's recent topic and tech tags with counts, the reuse hints the model sees.
//! - [`judge`]: the prompt, the Converse request, the reply parser and the judge that keeps only tags that pass the grammar.
//! - [`edit`]: the person's edits (`/tag`) and what a follow-up inherits.
//! - [`wire`]: the types of `POST /v1/tags/edit`.
//! - [`dynamo`]: the DynamoDB expressions of the tag writers and the check that a fake rejects what DynamoDB rejects.
//!
//! A tag is display and search text. It never reaches an S3 key, a branch, a zip name, a pull-request title or a process argument.

pub mod dynamo;
pub mod edit;
pub mod effective;
pub mod grammar;
pub mod judge;
pub mod vocab;
pub mod wire;

/// The most automatic tags a run stores (3 topic and 3 tech at most).
pub const MAX_AUTO: usize = 6;
/// The most tags the person may add to one run.
pub const MAX_USER: usize = 12;
/// The most tags the person may remove from one run; an edit that would pass it is refused and nothing is dropped.
pub const MAX_OFF: usize = 32;
/// The most tags a run shows: the derived project, the person's tags and the automatic tags.
pub const MAX_EFFECTIVE: usize = 1 + MAX_USER + MAX_AUTO;
