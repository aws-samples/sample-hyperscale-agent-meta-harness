//! The delivered head of a finished run: `GET /v1/runs/{runId}/head`.
//!
//! A follow-up run starts from the code a finished run delivered, and the person is shown which commit that is before the second Enter
//! (the box title, the confirmation) and the start key is made from it. Nothing in `RunSummary`, `RunDetail` or the events names that
//! commit; the one harness-made object that does is the verified bundle, whose first bytes name `refs/heads/harness/<runId>`. The wire
//! type lives here, not in `harness-protocol`: a field there would change the worker image key. `control-api` serialises it, `ham`
//! decodes and [checks](DeliveredHead::check) it, because the answer carries text that a worker wrote.
//!
//! What the answer holds:
//! - `headSha`: the full commit id (40 or 64 lower-case hex digits), read from the verified bundle's header.
//! - `runStatus`: the Run item's status, so the screen can tell a finished run from one that is still going.
//! - `clineMessage`: the engine's last message out of `evidence/direct.json`, which run-session `plan-delivery` writes (its `summary.clineMessage`,
//!   see [`cline_message_in_evidence`]). The envelope is the harness's; the text is the worker's, so it stays untrusted and is only ever added to a
//!   follow-up prompt by a key the person presses.
//! - `changes`: the files the delivery changed, out of the record the bundle validator writes beside the verified bundle
//!   (`runs/<run>/deliverable/changes.json`). It has two states and the digest words them differently: `unavailable` (no record, an
//!   unreadable one, or one that does not belong to this head) and `recorded` (a list that may be cut, with the number it left out).
//!   An empty list is a delivery that changed nothing, which is not the same as no record.

use crate::sanitize::display_text;
use harness_protocol::direct::MAX_CLINE_MESSAGE_CHARS;
use harness_protocol::goal::GoalStatus;
use serde::{Deserialize, Serialize};

/// The most files the record lists; the rest is counted in `omitted`.
pub const MAX_CHANGE_FILES: usize = 2_000;
/// The most bytes of one path in the record. A longer path is not listed and is counted in `omitted`.
pub const MAX_CHANGE_PATH_BYTES: usize = 1024;
/// `schemaVersion` of `changes.json`.
pub const CHANGES_SCHEMA_VERSION: u32 = 1;
/// The most bytes of `changes.json` read: 2,000 paths of the longest length and their framing fit well inside, and the head route answers with them
/// inside the 6 MB of a Lambda response.
pub const MAX_CHANGES_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// `GET /v1/runs/{runId}/head`.
pub fn path(run_id: &str) -> String {
    format!("/v1/runs/{run_id}/head")
}

/// The key of the validator's record of what a delivery changed: `runs/<run>/deliverable/changes.json`.
pub fn changes_key(run_id: &str) -> String {
    format!("runs/{run_id}/deliverable/changes.json")
}

/// Where `runs/<run>/evidence/direct.json` holds the engine's last message. run-session `plan-delivery` writes that file (`EvidenceFile`): its own
/// members, and agentd's `summary.json` whole under `summary`, whose `clineMessage` is the message. The envelope has no `clineMessage` of its own.
pub const CLINE_MESSAGE_POINTER: &str = "/summary/clineMessage";

/// Where `runs/<run>/evidence/direct.json` records the flow the run declared: agentd's `summary.json` is under `summary`, and its `flow` is
/// `quick`, `full` or `none`. The envelope has no `flow` of its own.
pub const FLOW_POINTER: &str = "/summary/flow";

/// The flow a parsed `evidence/direct.json` records, when it records one this build knows. The one reader of that place: the client reads the
/// delivered zip's copy with it, and run-session's test of what `plan-delivery` writes uses it too, so a drift of the writer or of a reader fails
/// a test.
pub fn flow_in_evidence(
    evidence: &serde_json::Value,
) -> Option<harness_protocol::direct::DirectFlow> {
    harness_protocol::direct::DirectFlow::parse(evidence.pointer(FLOW_POINTER)?.as_str()?)
}

/// The engine's last message in a parsed `evidence/direct.json`, as written (not cleaned: [`DeliveredHead::message`] does that). `None` when the
/// file holds no string there. The one reader of that place: the head route and [`crate::chat::final_message_from_evidence`] both use it, and
/// run-session's test of what `plan-delivery` writes uses it too, so a drift of the writer or of a reader fails a test.
pub fn cline_message_in_evidence(evidence: &serde_json::Value) -> Option<&str> {
    evidence.pointer(CLINE_MESSAGE_POINTER)?.as_str()
}

/// The answer of the head route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveredHead {
    pub run_id: String,
    /// The full commit id the verified bundle names.
    pub head_sha: String,
    pub run_status: GoalStatus,
    /// The engine's last message (at most `MAX_CLINE_MESSAGE_CHARS` characters), when the harness wrote one. Worker-authored text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cline_message: Option<String>,
    #[serde(default)]
    pub changes: Changes,
}

/// What a delivery changed, as far as a verified record says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Changes {
    /// No verified change list: the delivery predates the record, the object could not be read, or it names another head.
    #[default]
    Unavailable,
    Recorded(ChangeList),
}

/// The list in `changes.json`, as the route passes it on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeList {
    /// What the paths are changes from.
    pub base: ChangeBase,
    /// How many paths changed in all.
    pub total: u64,
    /// How many of them `files` leaves out (cut by count, or too long a path).
    pub omitted: u64,
    pub files: Vec<ChangedFile>,
}

/// The side of the comparison the validator used. It is the base of the run, which is not always the head of an earlier run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChangeBase {
    /// The run started from nothing: every path of the head is an addition.
    EmptyTree,
    Commit {
        sha: String,
    },
}

/// One changed path. `path` is git's own spelling, with non-UTF-8 bytes replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangeStatus,
}

/// `git diff-tree --name-status` without rename detection: a rename is a deletion and an addition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeStatus {
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "D")]
    Deleted,
    #[serde(rename = "T")]
    TypeChanged,
    /// A status letter this client has no word for.
    #[serde(other)]
    Other,
}

impl ChangeStatus {
    /// The word the digest uses.
    pub fn word(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::TypeChanged => "type changed",
            Self::Other => "changed",
        }
    }
}

/// Why `changes.json` is not used. No variant holds a byte of the record: it is a file the delivery wrote, but the words go to logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChangesRecordError {
    #[error("the change record is {0} bytes; the limit is {MAX_CHANGES_RECORD_BYTES}")]
    TooLarge(usize),
    #[error("the change record is not a JSON object of the expected shape")]
    Shape,
    #[error("the change record has schemaVersion {0}")]
    Schema(u32),
    #[error("the change record names another run")]
    Run,
    #[error("the change record names another head")]
    Head,
}

/// `changes.json` as the validator writes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangesRecord {
    schema_version: u32,
    run_id: String,
    head_sha: String,
    #[serde(flatten)]
    list: ChangeList,
}

/// Why an answer of the head route is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeadError {
    #[error("the answer is about another run")]
    Run,
    #[error("the answer names a head that is not a full commit id")]
    Head,
    #[error("the answer carries a message longer than the evidence can hold")]
    Message,
    #[error("the answer's change list is not well formed")]
    Changes,
}

fn is_object_id(s: &str) -> bool {
    crate::snapshot::is_object_id(s)
}

impl ChangeList {
    fn check(&self) -> Result<(), HeadError> {
        if let ChangeBase::Commit { sha } = &self.base
            && !is_object_id(sha)
        {
            return Err(HeadError::Changes);
        }
        let listed = u64::try_from(self.files.len()).map_err(|_| HeadError::Changes)?;
        if self.files.len() > MAX_CHANGE_FILES
            || listed.checked_add(self.omitted) != Some(self.total)
        {
            return Err(HeadError::Changes);
        }
        let bad_path =
            |p: &str| p.is_empty() || p.len() > MAX_CHANGE_PATH_BYTES || p.contains('\0');
        if self.files.iter().any(|f| bad_path(&f.path)) {
            return Err(HeadError::Changes);
        }
        Ok(())
    }
}

impl ChangeList {
    /// The list in a `changes.json`, when it is the record of this run's delivered `head_sha` and well formed. A record that names another
    /// head is the record of another delivery, and its paths are never shown as this one's.
    pub fn from_record(
        bytes: &[u8],
        run_id: &str,
        head_sha: &str,
    ) -> Result<ChangeList, ChangesRecordError> {
        if bytes.len() > MAX_CHANGES_RECORD_BYTES {
            return Err(ChangesRecordError::TooLarge(bytes.len()));
        }
        let record: ChangesRecord =
            serde_json::from_slice(bytes).map_err(|_| ChangesRecordError::Shape)?;
        if record.schema_version != CHANGES_SCHEMA_VERSION {
            return Err(ChangesRecordError::Schema(record.schema_version));
        }
        if record.run_id != run_id {
            return Err(ChangesRecordError::Run);
        }
        if record.head_sha != head_sha {
            return Err(ChangesRecordError::Head);
        }
        record.list.check().map_err(|_| ChangesRecordError::Shape)?;
        Ok(record.list)
    }
}

impl DeliveredHead {
    /// Whether this is an answer about `run_id` that the client can use. Checked on what was decoded, not on what was asked.
    pub fn check(&self, run_id: &str) -> Result<(), HeadError> {
        if self.run_id != run_id {
            return Err(HeadError::Run);
        }
        if !is_object_id(&self.head_sha) {
            return Err(HeadError::Head);
        }
        if self
            .cline_message
            .as_ref()
            .is_some_and(|m| m.chars().count() > MAX_CLINE_MESSAGE_CHARS)
        {
            return Err(HeadError::Message);
        }
        if let Changes::Recorded(list) = &self.changes {
            list.check()?;
        }
        Ok(())
    }

    /// The engine's last message as text to show or quote: terminal controls and escape sequences gone, bidirectional and invisible marks shown,
    /// token-shaped strings hidden. `None` when there is none or it is blank.
    pub fn message(&self) -> Option<String> {
        let raw = self.cline_message.as_deref()?;
        let clean = crate::redact::redact_tokens(&display_text(raw));
        (!clean.trim().is_empty()).then_some(clean)
    }
}
