//! Reading a stopped session's storage back: the body the service sends to the old session id, the worker's answer,
//! the marker a working session keeps on its storage, and the output that ends the wait of a session whose worker heard it is being ended.
//!
//! A session's work tree lives on storage that belongs to its runtime session id. When the session is stopped from outside or stops
//! answering, `run-session` calls that session id again with a [`RecoveryInvocationRequest`]. The platform starts a new machine with the
//! same storage, and the worker there does one thing: it copies the tree into one git bundle under a prefix of its own
//! ([`crate::direct::s3::recovery_bundle_key`]) and answers with a [`RecoveryAnswer`]. It starts no engine, takes no session credentials and
//! writes nothing to the storage. The worker stores first and compares afterwards, so `recovered: true` says that the bundle is stored and
//! nothing about whether the service may use it: `run-session` checks the object and decides (`next-slice`), and a copy it does not use
//! stays in the bucket.
//!
//! The crate does no I/O: these are the types and the one check of the request both sides run.

use crate::api::internal::ProcessCredentials;
use crate::common::is_ulid;
use crate::direct::{DIRECT_AGENT_ID, MAX_SLICES, commit_id_ok, s3};
use crate::runtime::{is_id, region_name_ok};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The whole `recover` op of `run-session`, by its own clock, in seconds. Every wait, call and write of the op takes from what is left.
pub const RECOVERY_LIMIT_S: u64 = 120;
/// The most seconds the worker is given for the read (`budgetS`): the smaller of this and what the op has left less 10 seconds.
pub const RECOVERY_WORKER_BUDGET_S: u32 = 75;
/// `TimeoutSeconds` of the state `Recover` of the run machine (`statemachines/direct.asl.json`).
pub const RECOVERY_STATE_TIMEOUT_S: u64 = 150;
/// On an end the worker gave notice of, the service does not begin its own stop before this many seconds after the end was known.
pub const RECOVERY_SETTLE_S: u64 = 15;
/// The most seconds the op's own stop call may take. A stop that gives no definite answer by then is a miss, and the old session id is not called.
pub const RECOVERY_STOP_CAP_S: u64 = 30;
/// No decision for a lost session before this many seconds after the service first knew the session was ending (the worker's notice, else the
/// start of the service's own stop): a copy the stopped machine was uploading can still land for about a minute.
pub const OLD_WORKER_TAIL_S: u64 = 75;
/// The lifetime of the credentials of a read, in seconds: the least STS gives.
pub const RECOVERY_CREDENTIALS_S: i32 = 900;
/// The session tag `agent` of the credentials of a read, and the key prefix `runs/<run>/agents/recovery/`.
pub const RECOVERY_AGENT_ID: &str = "recovery";
/// A file of the copy read back that is shorter than the saved copy's file and larger than this counts as doubt without being read.
pub const MAX_PREFIX_CHECK_BYTES: u64 = 67_108_864;
/// The most paths an answer names for one kind of doubt.
pub const MAX_COMPARE_PATHS_SHOWN: usize = 5;
/// [`RecoveryComparison::state`]: the copy read back was compared with the saved copy.
pub const COMPARISON_DONE: &str = "done";
/// [`RecoveryComparison::state`]: the comparison could not be made (the saved copy could not be fetched or read, or the time ran out).
pub const COMPARISON_UNKNOWN: &str = "unknown";
/// [`SessionEnded::cause`]: the worker heard that the session is being ended. It is a notice, no proof that the machine stopped.
pub const SESSION_END_REQUESTED: &str = "end-requested";

/// The `kind` member of the recovery body: the one word that tells agentd to read the storage back and run no turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RecoverKind {
    #[serde(rename = "recover")]
    Recover,
}

/// `POST /invocations` body of a read: sent by `run-session` to the runtime session id of the session that ended. Its `Debug` hides the
/// credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryInvocationRequest {
    pub kind: RecoverKind,
    /// A ULID made once per session that is read back. A second call with the same id gets the first answer; another id is refused.
    pub attempt_id: String,
    pub run_id: String,
    /// The session whose storage is read: 1 to [`MAX_SLICES`].
    pub session: u32,
    /// The runtime session id of that session, `<run>-direct-g<session>`: the id the call itself is made on.
    pub runtime_session_id: String,
    /// The stage Region (S3).
    pub region: String,
    /// Session credentials of `harness-worker-session` with the tag `agent=recovery`, narrowed to a PUT of [`Self::object`] and a GET of
    /// the baseline's object. They give no right on a run record, the internal API or a model.
    pub credentials: ProcessCredentials,
    /// `s3://<bucket>/runs/<run>/agents/recovery/turns/<session>/recovery.bundle`: the one object the worker writes, once.
    pub object: String,
    /// The commit the run started from.
    pub base_sha: String,
    /// The commit the session started on: the head it restored, or the run's base.
    pub start_head: String,
    pub baseline: RecoveryBaseline,
    /// The seconds the worker has for the whole read: 1 to [`RECOVERY_WORKER_BUDGET_S`].
    pub budget_s: u32,
}

/// The newest saved copy of the run when the read starts: what the run goes on from when the copy read back is not used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryBaseline {
    /// The commit of the saved copy.
    pub head_sha: String,
    /// The number of the saved copy, when its record names that commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// `s3://<bucket>/<key>` of the saved copy's bundle, which the worker fetches with [`RecoveryInvocationRequest::credentials`]. Absent
    /// when the baseline is the commit the session started on, which the session's own repository holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

/// The worker's answer to a [`RecoveryInvocationRequest`]. A reader ignores members it does not know, so the service reads the answer of a
/// later worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryAnswer {
    pub attempt_id: String,
    pub run_id: String,
    pub session: u32,
    /// Whether the bundle is stored. It does not say that the service may use it.
    pub recovered: bool,
    /// Why no bundle is stored, as one word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<RecoveredCopy>,
    /// What the worker found when it compared the stored copy with the baseline. Absent when it compared nothing; a comparison that failed
    /// is present with the state `unknown`. Neither takes a stored copy back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<RecoveryComparison>,
    /// The seconds the worker's machine had been up when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    pub took_ms: u64,
    pub agentd_version: String,
}

/// The bundle the worker stored: the tree as the storage held it, as one commit on the ref `refs/heads/snapshot`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecoveredCopy {
    /// The commit of the copy.
    pub head_sha: String,
    /// The tree of that commit.
    pub tree_sha: String,
    /// The commit below it: the engine's HEAD on the storage. Absent when the history could not be read; the whole tree is then stored
    /// without a parent, and no session starts from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sha: Option<String>,
    /// The marker of the storage, when it could be trusted (owned by the worker's own user, in a folder the engine cannot write to).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<CopyMarker>,
    /// Whether two passes over the tree gave the same tree.
    pub stable: bool,
    pub files: u64,
    pub bytes: u64,
    /// The time of the newest write in the tree. It is shown, never used to choose a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_write_at: Option<String>,
    pub bundle_bytes: u64,
    /// The S3 SHA-256 checksum of the bundle (base64), which the service compares with the object's.
    pub bundle_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// The comparison of the copy read back with the baseline. Only a comparison whose state is [`COMPARISON_DONE`] says anything about the
/// other members; every other state is doubt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryComparison {
    /// [`COMPARISON_DONE`] or [`COMPARISON_UNKNOWN`].
    pub state: String,
    /// Why the comparison is unknown, as one word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The copy read back holds the tree of the baseline.
    #[serde(default)]
    pub same_as_baseline: bool,
    /// The files of which the baseline holds more than the storage did: a file of the copy read back that is a strict prefix of the same
    /// file in the baseline (a shorter file too large to read, [`MAX_PREFIX_CHECK_BYTES`], counts here unread), and a file that the
    /// baseline holds, the commit below the baseline does not, and the copy read back lacks.
    #[serde(default)]
    pub regressed: u32,
    /// At most [`MAX_COMPARE_PATHS_SHOWN`] of those paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regressed_paths: Vec<String>,
    /// The files of the copy read back that equal the file in the commit below the baseline while the baseline holds another version: the
    /// storage holds the earlier version of a file that was rewritten in place.
    #[serde(default)]
    pub rewritten: u32,
    /// At most [`MAX_COMPARE_PATHS_SHOWN`] of those paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewritten_paths: Vec<String>,
}

impl RecoveryComparison {
    /// Whether the comparison was made. A state word this release does not know is not `done`.
    pub fn done(&self) -> bool {
        self.state == COMPARISON_DONE
    }
}

/// The marker a working session keeps on its storage, outside the folder of the engine's user: the newest copy it stored, or the commit it
/// started on. The storage is at or after a saved copy only when the marker names exactly that copy's commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopyMarker {
    pub schema_version: u32,
    pub session: u32,
    /// The number of the copy the marker names. A marker that names the commit the session started on carries that commit's own number
    /// when the commit is a copy a worker made while an engine worked, and 0 otherwise.
    pub count: u32,
    pub head_sha: String,
}

/// The output that ends the wait for a session whose worker gave notice that it is being ended: `{"sessionEnded": {...}}`. It is no
/// turn-complete, so the session counts as lost, with [`Self::cause`] as the cause.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionEnded {
    pub session: u32,
    /// [`SESSION_END_REQUESTED`].
    pub cause: String,
    /// When the service took the notice, RFC 3339 UTC.
    pub at: String,
}

/// What [`validate_recovery_invocation`] refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryInvocationError {
    BadRunId,
    BadSession,
    BadAttemptId,
    BadRuntimeSessionId,
    BadRegion,
    NotSessionCredentials,
    BadObject,
    BadCommit,
    BadBaseline,
    BadBudget,
}

impl fmt::Display for RecoveryInvocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRunId => write!(f, "runId must be [A-Za-z0-9-]+"),
            Self::BadSession => write!(f, "session must be 1 to {MAX_SLICES}"),
            Self::BadAttemptId => write!(f, "attemptId must be a ULID"),
            Self::BadRuntimeSessionId => write!(
                f,
                "runtimeSessionId must be <runId>-{DIRECT_AGENT_ID}-g<session>, 33 to 256 characters"
            ),
            Self::BadRegion => write!(f, "region must be an AWS Region name"),
            Self::NotSessionCredentials => write!(
                f,
                "credentials must be version-1 session credentials (ASIA...)"
            ),
            Self::BadObject => write!(
                f,
                "object must be s3://<bucket>/runs/<runId>/agents/{RECOVERY_AGENT_ID}/turns/<session>/recovery.bundle"
            ),
            Self::BadCommit => write!(
                f,
                "baseSha, startHead and baseline.headSha must each be a 40 character lowercase commit id"
            ),
            Self::BadBaseline => write!(
                f,
                "baseline.object must be, in the bucket of object, the saved copy of this session or of an earlier one (turns/<n>/snapshot.bundle) or the read-back bundle of an earlier session"
            ),
            Self::BadBudget => write!(f, "budgetS must be 1 to {RECOVERY_WORKER_BUDGET_S}"),
        }
    }
}

impl std::error::Error for RecoveryInvocationError {}

/// Whether `id` is the runtime session id of session `session` of `run_id`: `<run>-direct-g<session>`, the number with zeros before it
/// where the id would otherwise be shorter than the platform's 33 characters.
fn session_id_of(run_id: &str, session: u32, id: &str) -> bool {
    let head = format!("{run_id}-{DIRECT_AGENT_ID}-g");
    (33..=256).contains(&id.len())
        && id.strip_prefix(head.as_str()).is_some_and(|tail| {
            !tail.is_empty()
                && tail.bytes().all(|b| b.is_ascii_digit())
                && tail.trim_start_matches('0') == session.to_string()
        })
}

/// Checks everything agentd relies on before it reads the storage, and everything `run-session` promises before it sends the body: the
/// worker writes one object, the read-back key of that session, and reads at most one, a saved copy of this run that this session or an
/// earlier one left or the read-back bundle of an earlier session. The error is the first rule broken.
pub fn validate_recovery_invocation(
    req: &RecoveryInvocationRequest,
) -> Result<(), RecoveryInvocationError> {
    use RecoveryInvocationError as E;
    if !is_id(&req.run_id) {
        return Err(E::BadRunId);
    }
    if !(1..=MAX_SLICES).contains(&req.session) {
        return Err(E::BadSession);
    }
    if !is_ulid(&req.attempt_id) {
        return Err(E::BadAttemptId);
    }
    if !session_id_of(&req.run_id, req.session, &req.runtime_session_id) {
        return Err(E::BadRuntimeSessionId);
    }
    if !region_name_ok(&req.region) {
        return Err(E::BadRegion);
    }
    if req.credentials.version != 1 || !req.credentials.access_key_id.starts_with("ASIA") {
        return Err(E::NotSessionCredentials);
    }
    let bucket = match s3::split_uri(&req.object) {
        Some((bucket, key)) if key == s3::recovery_bundle_key(&req.run_id, req.session) => bucket,
        _ => return Err(E::BadObject),
    };
    if ![&req.base_sha, &req.start_head, &req.baseline.head_sha]
        .into_iter()
        .all(|c| commit_id_ok(c))
    {
        return Err(E::BadCommit);
    }
    if let Some(object) = &req.baseline.object {
        let ok = s3::split_uri(object).is_some_and(|(b, key)| {
            b == bucket
                && ((1..=req.session).any(|n| key == s3::snapshot_bundle_key_t(&req.run_id, n))
                    || (1..req.session).any(|n| key == s3::recovery_bundle_key(&req.run_id, n)))
        });
        if !ok {
            return Err(E::BadBaseline);
        }
    }
    if !(1..=RECOVERY_WORKER_BUDGET_S).contains(&req.budget_s) {
        return Err(E::BadBudget);
    }
    Ok(())
}
