//! Reading a stopped session's storage back: the worker's half.
//!
//! When a session is stopped from outside, or is lost, its files stay on its storage. The service then calls the same session id with a
//! body of kind `recover`; the platform starts a new machine with that storage, and this module answers the call. It makes one copy of
//! the work tree as the storage holds it, stores it at the one key the request names, and only then compares it with the saved copy the
//! request names. It answers with what it stored and what it found. Whether a session starts from the copy is the service's decision,
//! never the worker's: nothing here withholds work.
//!
//! In this mode agentd does one thing. It starts no engine, installs no session credentials, opens no credential socket and no package
//! proxy, calls no internal API, and writes nothing under the storage's root. The copy is made in a private repository on the machine's
//! own disk (`<data_root>/<run>/recover/<attempt>/`), with the engine's objects as an alternate and the tree as the work tree
//! (`Snapshotter::for_read_back`). The request's credentials are used for the one PUT and at most two GETs,
//! and never reach a log line.
//! One thing does move on the storage: git renews the time stamp of an object file of the engine's store when the copy uses that
//! object again through the alternate, as it does for every copy of a working session. No file, no content and no name changes.
//!
//! The whole read has one limit, the request's `budgetS`. When it ends before the copy is stored, the answer says `budget` and nothing
//! is in the bucket. When it ends after, the answer names the stored copy and says that the comparison is unknown: a copy that is
//! stored stays stored, and a comparison that fails never takes it back.

use crate::direct::classify::scrub_detail;
use crate::direct::marker;
use crate::direct::snapshot::{GiveUp, Snapshotter};
use crate::s3::{PutNew, S3Objects, sha256_base64};
use crate::server::App;
use harness_protocol::bundle_header::{self, HEADER_BYTES_MAX};
use harness_protocol::direct::{
    COMPARISON_DONE, COMPARISON_UNKNOWN, DIRECT_AGENT_ID, MAX_COMPARE_PATHS_SHOWN,
    MAX_PREFIX_CHECK_BYTES, RecoveredCopy, RecoveryAnswer, RecoveryComparison,
    RecoveryInvocationRequest, s3 as keys,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// This machine is being ended: it reads nothing.
pub const REASON_ENDING: &str = "ending";
/// The root of the session's storage is not there.
pub const REASON_NOT_MOUNTED: &str = "not-mounted";
/// The run's folder, the engine's folder or the repository is missing or is a link.
pub const REASON_NO_TREE: &str = "no-tree";
/// The tree could not be read into a copy.
pub const REASON_TREE_UNREADABLE: &str = "tree-unreadable";
/// The request's time ended before the copy was stored; as the reason of a comparison, before the comparison was made.
pub const REASON_BUDGET: &str = "budget";
/// The copy was made and could not be stored.
pub const REASON_UPLOAD_FAILED: &str = "upload-failed";
/// The key already holds an object: nothing is written over it.
pub const REASON_EXISTS: &str = "exists";
/// The saved copy to compare with could not be fetched or read, or is another commit than the request names.
pub const REASON_BASELINE_UNREADABLE: &str = "baseline-unreadable";

/// The read ended without an answer of its own.
pub const REASON_NO_ANSWER: &str = "no-answer";

/// How often the PUT of the copy is tried again after the first try.
const PUT_RETRIES: u32 = 2;

/// Where the private repository of one read is: on the machine's own disk, never on the storage.
pub fn private_dir(data_root: &Path, run: &str, attempt: &str) -> PathBuf {
    data_root.join(run).join("recover").join(attempt)
}

/// Whether `path` is a folder of its own: there, a directory, and no link.
fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// The seconds this machine has been up, when the system says.
fn uptime_s() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (seconds >= 0.0).then_some(seconds as u64)
}

/// A comparison that was not made, and why.
fn unknown(reason: &str) -> RecoveryComparison {
    RecoveryComparison {
        state: COMPARISON_UNKNOWN.to_string(),
        reason: Some(reason.to_string()),
        same_as_baseline: false,
        regressed: 0,
        regressed_paths: Vec::new(),
        rewritten: 0,
        rewritten_paths: Vec::new(),
    }
}

/// What a machine answers for a read that ended without an answer of its own: no copy is named. The server keeps it from before the
/// read begins, so a read is never made twice on one machine.
pub fn no_answer(req: &RecoveryInvocationRequest) -> RecoveryAnswer {
    RecoveryAnswer {
        attempt_id: req.attempt_id.clone(),
        run_id: req.run_id.clone(),
        session: req.session,
        recovered: false,
        reason: Some(REASON_NO_ANSWER.to_string()),
        copy: None,
        comparison: None,
        uptime_s: uptime_s(),
        took_ms: 0,
        agentd_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Answers one request to read this machine's storage back. The request is valid (`validate_recovery_invocation`) and this machine
/// never ran a turn (`server`). The answer always comes: with the stored copy and what the comparison found, or with the one word
/// that says why no copy is stored.
pub async fn handle(app: &Arc<App>, req: &RecoveryInvocationRequest) -> RecoveryAnswer {
    let started = Instant::now();
    let budget = Duration::from_secs(u64::from(req.budget_s));
    // What is stored already when the time ends: from then on the answer names it, whatever else happens.
    let stored: Arc<Mutex<Option<RecoveredCopy>>> = Arc::new(Mutex::new(None));
    let give_up = GiveUp::at(Some(tokio::time::Instant::now() + budget));
    let read = read(app, req, &stored, &give_up);
    let outcome = match tokio::time::timeout(budget, read).await {
        Ok(outcome) => outcome,
        // The read is dropped here, and every git it started ends with it.
        Err(_) => match stored.lock().unwrap_or_else(|p| p.into_inner()).take() {
            Some(copy) => Ok((copy, unknown(REASON_BUDGET))),
            None => Err(REASON_BUDGET),
        },
    };
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (recovered, reason, copy, comparison) = match outcome {
        Ok((copy, comparison)) => (true, None, Some(copy), Some(comparison)),
        Err(reason) => (false, Some(reason.to_string()), None, None),
    };
    app.log.line(&format!(
        "direct recover session {}: {} in {took_ms} ms",
        req.session,
        match (&reason, &comparison) {
            (Some(reason), _) => format!("no copy stored ({reason})"),
            (None, Some(c)) if c.done() => "a copy is stored and compared".to_string(),
            (None, _) => "a copy is stored; the comparison is unknown".to_string(),
        }
    ));
    RecoveryAnswer {
        attempt_id: req.attempt_id.clone(),
        run_id: req.run_id.clone(),
        session: req.session,
        recovered,
        reason,
        copy,
        comparison,
        uptime_s: uptime_s(),
        took_ms,
        agentd_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The second try of the fetch of a saved copy, after the storage's own objects could not complete it (`first` is what git
/// said). A saved copy may hold what the run changed and need what the bundle the run started from holds. The stopped session's
/// store holds that while its `.git` is whole, and no longer when the engine removed `.git` or the objects of the base. The read
/// then takes the base first, as every reader of such a copy does: `runs/<run>/input/repo.bundle` of the bucket the request names
/// is downloaded to the private folder and fetched into the private repository, and the copy after it. A copy whose header names
/// no commit it needs is not tried again, and a base that cannot be read or fetched leaves `first` the reason, with what the base
/// said.
async fn fetch_after_the_base(
    objects: &S3Objects,
    req: &RecoveryInvocationRequest,
    dir: &Path,
    copy: &Path,
    snapshotter: &Snapshotter,
    first: String,
) -> Result<String, String> {
    let header = super::file_prefix(copy, HEADER_BYTES_MAX);
    let needs_base = bundle_header::needs(&header).is_ok_and(|commits| !commits.is_empty());
    let Some((bucket, _)) = keys::split_uri(&req.object).filter(|_| needs_base) else {
        return Err(first);
    };
    let base_uri = keys::uri(bucket, &keys::repo_bundle_key(&req.run_id));
    let base = dir.join("base.bundle");
    objects
        .get_to_file(&base_uri, &req.credentials, &base)
        .await
        .map_err(|e| format!("{first}; the bundle the run started from could not be read: {e}"))?;
    snapshotter.fetch_base(&base).await.map_err(|e| {
        format!("{first}; the bundle the run started from could not be fetched: {e}")
    })?;
    snapshotter.fetch_copy(copy).await
}

/// A file call off the runtime's threads: a storage that stops answering, or a large file, holds one thread and never the read, so the
/// read's one limit still ends it.
async fn on_storage<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(call).await.ok()
}

async fn read(
    app: &Arc<App>,
    req: &RecoveryInvocationRequest,
    stored: &Mutex<Option<RecoveredCopy>>,
    give_up: &GiveUp,
) -> Result<(RecoveredCopy, RecoveryComparison), &'static str> {
    if app.ending.requested() {
        return Err(REASON_ENDING);
    }
    let cfg = &app.cfg;
    let log = |what: &str, e: &str| {
        app.log.line(&format!(
            "direct recover session {}: {what}: {}",
            req.session,
            scrub_detail(e)
        ));
    };

    // 1. The storage: its root is there, and the run's folder, the engine's folder and the repository are folders of their own.
    let root = cfg.workspace_root.clone();
    let run_dir = root.join(&req.run_id);
    let repo = run_dir.join(DIRECT_AGENT_ID).join("repo");
    let found = {
        let (root, run_dir, repo) = (root.clone(), run_dir.clone(), repo.clone());
        on_storage(move || {
            if !real_dir(&root) {
                return Err(REASON_NOT_MOUNTED);
            }
            let folders = [&run_dir, &run_dir.join(DIRECT_AGENT_ID), &repo];
            if folders.into_iter().all(|d| real_dir(d)) {
                Ok(())
            } else {
                Err(REASON_NO_TREE)
            }
        })
        .await
    };
    found.unwrap_or(Err(REASON_NO_TREE))?;

    // 2. The marker of the storage, when it can be trusted. No marker is no reason not to copy: the service then starts no session from
    //    the copy, and the work is still stored.
    let seen = {
        let (root, run, owner) = (root.clone(), req.run_id.clone(), cfg.direct.marker_owner);
        on_storage(move || marker::read(&root, &run, owner))
            .await
            .flatten()
    };

    // 3. The copy, in a repository of this machine's own disk.
    let dir = private_dir(&cfg.data_root, &req.run_id, &req.attempt_id);
    let made_dir = {
        let dir = dir.clone();
        on_storage(move || std::fs::create_dir_all(&dir).map_err(|e| e.to_string())).await
    };
    if let Err(e) = made_dir.unwrap_or_else(|| Err("the call ended without an answer".to_string()))
    {
        log("the private folder", &e);
        return Err(REASON_TREE_UNREADABLE);
    }
    let mut snapshotter = Snapshotter::for_read_back(&dir, &repo, &req.base_sha)
        .await
        .map_err(|e| {
            log("the private repository", &e);
            REASON_TREE_UNREADABLE
        })?;
    let made = snapshotter
        .read_back(&req.start_head, req.session, give_up)
        .await
        .map_err(|e| {
            log("the copy", &e);
            REASON_TREE_UNREADABLE
        })?;
    let (files, bytes) = snapshotter.totals(&made.tree).await.unwrap_or_else(|e| {
        log("the size of the tree", &e);
        (0, 0)
    });

    // 4. The copy is stored at once, create-only and with its checksum, before anything is fetched or compared.
    let checksum = {
        let bundle = made.bundle.clone();
        on_storage(move || {
            std::fs::read(&bundle)
                .map(|body| sha256_base64(&body))
                .map_err(|e| e.to_string())
        })
        .await
    };
    let checksum =
        match checksum.unwrap_or_else(|| Err("the call ended without an answer".to_string())) {
            Ok(sum) => sum,
            Err(e) => {
                log("the bundle", &e);
                return Err(REASON_TREE_UNREADABLE);
            }
        };
    let objects = S3Objects::new(req.region.clone(), cfg.s3_endpoint.clone());
    let version_id = match objects
        .put_file_new_retrying_receipt(
            &req.object,
            &req.credentials,
            &made.bundle,
            PUT_RETRIES,
            cfg.direct.upload_retry_pause,
        )
        .await
    {
        Ok((PutNew::Stored, pin)) => pin.and_then(|p| p.version_id),
        Ok((PutNew::Exists, _)) => return Err(REASON_EXISTS),
        Err(e) => {
            log("the copy could not be stored", &e);
            return Err(REASON_UPLOAD_FAILED);
        }
    };
    let copy = RecoveredCopy {
        head_sha: made.commit.clone(),
        tree_sha: made.tree.clone(),
        parent_sha: made.parent.clone(),
        marker: seen,
        stable: made.stable,
        files,
        bytes,
        newest_write_at: None,
        bundle_bytes: made.bundle_bytes,
        bundle_sha256: checksum,
        version_id,
    };
    *stored.lock().unwrap_or_else(|p| p.into_inner()) = Some(copy.clone());

    // 5. The comparison with the saved copy. Whatever fails from here on is said about the comparison, never about the copy.
    let baseline = match &req.baseline.object {
        Some(uri) => {
            let file = dir.join("baseline.bundle");
            match objects.get_to_file(uri, &req.credentials, &file).await {
                Ok(()) => match snapshotter.fetch_copy(&file).await {
                    Ok(commit) => Ok(commit),
                    Err(first) => {
                        fetch_after_the_base(&objects, req, &dir, &file, &snapshotter, first).await
                    }
                },
                Err(e) => Err(e),
            }
        }
        // The saved copy is a commit the session itself restored or made: the engine's store holds it.
        None if snapshotter.has_commit(&req.baseline.head_sha).await => {
            Ok(req.baseline.head_sha.clone())
        }
        None => Err("the saved copy is not in the repository".to_string()),
    };
    let comparison = match baseline {
        Ok(commit) if commit == req.baseline.head_sha => {
            match snapshotter
                .compare(
                    &made.tree,
                    &commit,
                    MAX_PREFIX_CHECK_BYTES,
                    MAX_COMPARE_PATHS_SHOWN,
                )
                .await
            {
                Ok(found) => RecoveryComparison {
                    state: COMPARISON_DONE.to_string(),
                    reason: None,
                    same_as_baseline: found.same,
                    regressed: found.regressed,
                    regressed_paths: found.regressed_paths,
                    rewritten: found.rewritten,
                    rewritten_paths: found.rewritten_paths,
                },
                Err(e) => {
                    log("the comparison", &e);
                    unknown(REASON_BASELINE_UNREADABLE)
                }
            }
        }
        Ok(_) => {
            log(
                "the saved copy",
                "it is another commit than the request names",
            );
            unknown(REASON_BASELINE_UNREADABLE)
        }
        Err(e) => {
            log("the saved copy", &e);
            unknown(REASON_BASELINE_UNREADABLE)
        }
    };
    Ok((copy, comparison))
}
