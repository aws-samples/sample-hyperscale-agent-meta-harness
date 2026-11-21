//! The worker's half of reading a stopped session's storage back (`src/direct/recover.rs`, `src/server.rs`).
//!
//! A session that is stopped from outside leaves its files on its storage. The service calls the same session id with a body of kind
//! `recover`, the platform starts a new machine with that storage, and agentd answers the call itself: it makes one copy of the work
//! tree as the storage holds it, stores it at the one key the request names, then compares it with the saved copy, and says what it
//! stored and what it found.
//!
//! The stand-in (`b854_support`): session 1 of a run works in one agentd (the old machine) and ends; a file is then written and
//! closed in its work tree, after the last stored copy. A second agentd with a disk of its own and the same storage root is the new
//! machine.
#![allow(clippy::unwrap_used)]

mod b854_support;
mod common;
mod direct_support;

use agentd::direct::snapshot::{GiveUp, Snapshotter};
use b854_support::{
    ATTEMPT, LATE_FILE, LATE_TEXT, OTHER_ATTEMPT, Stopped, machine_at, new_machine,
    process_machine, recover_on, stopped_session,
};
use common::{git, python_calc, scratch};
use direct_support::{
    BUCKET, Direct, Opts, PutFailure, RUN, checkout_of_copy, http, needs_the_base, no_secrets,
    sha256_b64,
};
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::runtime::{PingStatus, RepoSource, TurnOutcome};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Every entry under `root`: its path, kind, size and time of change. The time of an object file of the engine's store is left out:
/// git renews it when a copy uses the object again through the alternate, as it does for every copy of a working session, and
/// changes nothing else about the file.
fn listing(root: &Path) -> Vec<(PathBuf, bool, u64, i64, i64)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let is_dir = meta.file_type().is_dir();
            let object = !is_dir && path.to_string_lossy().contains("/.git/objects/");
            out.push((
                path.clone(),
                is_dir,
                if is_dir { 0 } else { meta.len() },
                if object { 0 } else { meta.mtime() },
                if object { 0 } else { meta.mtime_nsec() },
            ));
            if is_dir {
                todo.push(path);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_recover_copies_a_file_written_after_the_last_stored_copy() {
    let s = stopped_session().await;
    // The trigger: the last stored copy does not hold the file, and the storage does.
    let saved_clone = checkout_of_copy(
        &s.d.aws,
        &s.d.dir,
        &keys::snapshot_bundle_key_t(RUN, 1),
        "saved-clone",
    );
    assert!(
        !saved_clone.join(LATE_FILE).exists(),
        "the trigger: the saved copy was made before the file was written"
    );
    let engine_head = git(&s.d.worktree(), &["rev-parse", "HEAD"]);

    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "{answer:?}");
    assert_eq!(answer.reason, None);
    assert_eq!(
        (
            answer.attempt_id.as_str(),
            answer.run_id.as_str(),
            answer.session
        ),
        (ATTEMPT, RUN, 1)
    );
    let copy = answer.copy.clone().expect("the stored copy");

    // The object holds the file, on the branch every saved copy has.
    let tree = s.stored_tree("read-back");
    assert_eq!(
        std::fs::read_to_string(tree.join(LATE_FILE)).unwrap(),
        LATE_TEXT
    );
    assert_eq!(git(&tree, &["rev-parse", "HEAD"]), copy.head_sha);
    assert_eq!(git(&tree, &["rev-parse", "HEAD^{tree}"]), copy.tree_sha);
    assert_eq!(
        copy.parent_sha.as_deref(),
        Some(engine_head.as_str()),
        "the copy sits on the commit the engine's repository stood at"
    );
    assert_eq!(git(&tree, &["rev-parse", "HEAD^"]), engine_head);
    assert!(copy.stable, "{copy:?}");
    assert!(copy.files > 0 && copy.bytes > 0, "{copy:?}");
    // It was written once, with its checksum, and the answer names that checksum.
    assert_eq!(s.d.aws.put_attempts(&s.key()), 1);
    assert_eq!(
        s.d.aws.checksum_of(&s.key()),
        Some(copy.bundle_sha256.clone())
    );
    assert_eq!(
        copy.bundle_bytes,
        s.d.aws.get(&s.key()).unwrap().len() as u64
    );
    // The marker of the storage names the newest stored copy of the session.
    let marker = copy.marker.clone().expect("the marker of the storage");
    assert_eq!(marker.session, 1);
    assert_eq!(serde_json::json!(marker.count), s.record["count"]);
    assert_eq!(serde_json::json!(marker.head_sha), s.record["headSha"]);
    // The comparison with the saved copy was made: another tree, and no file that the saved copy holds more of.
    let cmp = answer.comparison.clone().expect("the comparison");
    assert!(cmp.done(), "{cmp:?}");
    assert!(!cmp.same_as_baseline, "{cmp:?}");
    assert_eq!((cmp.regressed, cmp.rewritten), (0, 0), "{cmp:?}");

    // The next session starts from that bundle through agentd's own start path, in a machine of its own.
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(
        &keys::recovery_bundle_key(RUN, 1),
        s.d.aws.get(&s.key()).unwrap(),
    );
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::recovery_bundle_key(RUN, 1)),
    };
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(s.base.clone()),
        restore_head: Some(copy.head_sha.clone()),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d2.req)
        .expect("the arrival check takes the read-back key of an earlier session");
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        !d2.prompts_received().is_empty(),
        "the restore check passed and the engine worked"
    );
    assert_eq!(
        std::fs::read_to_string(d2.worktree().join(LATE_FILE)).unwrap(),
        LATE_TEXT,
        "the next session has the file"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_recover_writes_nothing_under_the_workspace_root() {
    let s = stopped_session().await;
    let before = listing(&s.storage());
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(
        answer.recovered && s.d.aws.get(&s.key()).is_some(),
        "first: a copy is stored: {answer:?}"
    );
    let after = listing(&s.storage());
    assert_eq!(before.len(), after.len(), "no entry was added or removed");
    assert_eq!(
        before, after,
        "no entry changed its size, and no file of the work tree, no folder and no file of .git but an object changed its time"
    );
    let head = s.d.worktree().join(".git/HEAD");
    assert!(
        before.iter().any(|e| e.0 == head && e.3 != 0),
        "the trigger: the listing holds the repository's own files with their times"
    );
    assert!(
        m.data.join(RUN).join("recover").join(ATTEMPT).is_dir(),
        "the private repository is on the machine's own disk"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_recover_starts_no_engine_and_calls_no_internal_api() {
    let s = stopped_session().await;
    let calls = |s: &Stopped| {
        (
            s.d.aws.batches().len(),
            s.d.aws.heartbeats().len(),
            s.d.aws.complete().len(),
            s.d.aws.refreshes(),
        )
    };
    let before = calls(&s);
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(
        answer.recovered && s.d.aws.get(&s.key()).is_some(),
        "first: a copy is stored: {answer:?}"
    );
    assert_eq!(calls(&s), before, "the internal API got no request");
    // No engine: no session folder with an engine's data, no credential socket, no package proxy.
    assert!(!m.data.join(RUN).join("direct").exists());
    let entries: Vec<_> = std::fs::read_dir(m.data.join(RUN))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["recover"], "{entries:?}");
    assert!(
        !m.runtime.exists(),
        "no credential socket and no proxy were set up"
    );
    assert!(
        !m.log.lines().iter().any(|l| l.contains("engine")),
        "{:?}",
        m.log.lines()
    );
    let (_, ping) = http("GET", &format!("{}/ping", m.url), None).await;
    let ping: harness_protocol::runtime::PingResponse = serde_json::from_str(&ping).unwrap();
    assert_eq!(ping.status, PingStatus::Healthy, "the read has ended");
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_recover_is_refused_on_a_machine_that_ran_a_turn_and_a_turn_is_refused_after_a_recover()
 {
    let s = stopped_session().await;
    // The old machine ran a turn: it never reads a storage back.
    let (code, text) =
        s.d.post_raw(serde_json::to_string(&s.request(ATTEMPT)).unwrap())
            .await;
    assert_eq!(code, 409, "{text}");
    assert!(text.contains("session-active"), "{text}");
    assert!(s.d.aws.get(&s.key()).is_none(), "nothing was stored");
    // A machine in the middle of a turn refuses too.
    let busy = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 15,
        ..Opts::default()
    })
    .await;
    let (code, text) = busy.post().await;
    assert_eq!(code, 200, "{text}");
    let (code, text) = busy
        .post_raw(serde_json::to_string(&s.request(ATTEMPT)).unwrap())
        .await;
    assert_eq!(code, 409, "{text}");
    assert!(text.contains("session-active"), "{text}");
    busy.wait_result().await;

    // A machine that read a storage back never becomes a working session.
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "{answer:?}");
    let (code, text) = m.post(serde_json::to_string(&s.d.req).unwrap()).await;
    assert_eq!(code, 409, "{text}");
    assert!(
        text.contains("this machine read a stopped session's storage and runs no turn"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_the_same_attempt_gets_the_same_answer_and_another_attempt_is_refused() {
    let s = stopped_session().await;
    let m = new_machine(&s, "recover").await;
    let req = s.request(ATTEMPT);
    let body = serde_json::to_string(&req).unwrap();
    let (first, second) = tokio::join!(m.post(body.clone()), m.post(body.clone()));
    assert_eq!((first.0, second.0), (200, 200), "{first:?} {second:?}");
    assert_eq!(first.1, second.1, "one read, one answer");
    assert_eq!(s.d.aws.put_attempts(&s.key()), 1, "one copy was made");
    let (code, third) = m.post(body).await;
    assert_eq!(code, 200);
    assert_eq!(third, first.1, "the same answer later too");
    let (code, text) = m
        .post(serde_json::to_string(&s.request(OTHER_ATTEMPT)).unwrap())
        .await;
    assert_eq!(code, 409, "{text}");
    assert_eq!(s.d.aws.put_attempts(&s.key()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_tree_without_history_is_stored_and_not_used() {
    let s = stopped_session().await;
    // The engine removed its repository's history before the session was stopped.
    std::fs::rename(
        s.d.worktree().join(".git"),
        s.d.dir.join("history-moved-away"),
    )
    .unwrap();
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "the tree is the work: {answer:?}");
    let copy = answer.copy.clone().unwrap();
    assert_eq!(
        copy.parent_sha, None,
        "a copy without history has no parent, and the service starts no session from it"
    );
    let tree = s.stored_tree("no-history");
    assert_eq!(
        std::fs::read_to_string(tree.join(LATE_FILE)).unwrap(),
        LATE_TEXT
    );
    assert_eq!(
        git(&tree, &["rev-list", "--count", "HEAD"]),
        "1",
        "the bundle holds the whole tree in one commit"
    );
    assert!(
        tree.join("src").is_dir(),
        "with the files of the repository"
    );
}

/// Whether the repository at `repo` can read the commit `commit`.
fn holds_commit(repo: &Path, commit: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .current_dir(repo)
        .output()
        .unwrap()
        .status
        .success()
}

/// The saved copy of a stopped session leaves out what the bundle the run started from holds. The read fetches it into a
/// repository that reads the stopped session's own objects, which hold the base, and the comparison is made with no read of the
/// base. When those objects no longer hold what the copy needs (the engine removed `.git`, or the objects of the base, before the
/// stop), the read takes the base first, as every reader of such a copy does: it downloads the bundle the run started from, fetches
/// it into its private repository and the copy after it, and the comparison is made as it was when a saved copy held the whole
/// history. A base that cannot be read leaves the comparison unknown, with the reason, and the copy read back stays stored.
#[tokio::test(flavor = "multi_thread")]
async fn b860_a_read_compares_with_a_saved_copy_that_needs_the_base_and_says_when_it_cannot() {
    let base_key = keys::repo_bundle_key(RUN);
    let saved_key = keys::snapshot_bundle_key_t(RUN, 1);

    // 1. The storage holds the base: nothing of it is asked of the bucket.
    let s = stopped_session().await;
    assert!(
        needs_the_base(&s.d.aws.get(&saved_key).unwrap()),
        "TRIGGER PRESENT: the saved copy of the stopped session names a commit it needs"
    );
    let before = s.d.aws.gets_of(&base_key);
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "{answer:?}");
    let cmp = answer.comparison.clone().expect("the comparison");
    assert!(cmp.done(), "{cmp:?}");
    assert!(answer.copy.clone().unwrap().parent_sha.is_some());
    assert_eq!(
        s.d.aws.gets_of(&base_key),
        before,
        "the storage holds the base, so the read asks the bucket for nothing of it"
    );

    // 2. The engine removed its repository's history before the session was stopped: the base is not on the storage any more.
    let s = stopped_session().await;
    assert!(needs_the_base(&s.d.aws.get(&saved_key).unwrap()));
    std::fs::rename(
        s.d.worktree().join(".git"),
        s.d.dir.join("history-moved-away"),
    )
    .unwrap();
    let before = s.d.aws.gets_of(&base_key).len();
    let m = new_machine(&s, "recover-without-history").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "the tree is the work: {answer:?}");
    assert_eq!(
        answer.copy.clone().unwrap().parent_sha,
        None,
        "a copy without history has no parent, and the service starts no session from it"
    );
    let cmp = answer.comparison.clone().expect("the comparison");
    assert!(
        cmp.done(),
        "the read took the base first and compared: {cmp:?}"
    );
    assert!(
        !cmp.same_as_baseline,
        "the storage holds a file that was written after the saved copy: {cmp:?}"
    );
    let reads = s.d.aws.gets_of(&base_key);
    assert_eq!(
        reads.len(),
        before + 1,
        "one download of the bundle the run started from: {reads:?}"
    );
    assert_eq!(reads.last().unwrap(), &None, "the whole object");

    // 3. The engine kept `.git` and removed the objects of the base from it: a new root commit, every other ref gone, and
    // everything that no ref reaches pruned.
    let s = stopped_session().await;
    let saved = s.d.aws.get(&saved_key).unwrap();
    let needed = harness_protocol::bundle_header::needs(&saved[..saved.len().min(65_536)]).unwrap();
    assert!(
        !needed.is_empty(),
        "TRIGGER PRESENT: the saved copy names commits it needs"
    );
    let wt = s.d.worktree();
    git(&wt, &["checkout", "-q", "--orphan", "fresh"]);
    git(&wt, &["commit", "-q", "-m", "a new root"]);
    git(&wt, &["remote", "remove", "origin"]);
    for name in git(&wt, &["for-each-ref", "--format=%(refname)"]).lines() {
        if name != "refs/heads/fresh" {
            git(&wt, &["update-ref", "--no-deref", "-d", name]);
        }
    }
    git(&wt, &["reflog", "expire", "--expire=now", "--all"]);
    git(&wt, &["gc", "-q", "--prune=now"]);
    for commit in &needed {
        assert!(
            !holds_commit(&wt, commit),
            "TRIGGER PRESENT: the storage no longer holds the commit {commit} that the saved copy needs"
        );
    }
    assert!(wt.join(".git").is_dir(), "TRIGGER PRESENT: .git is kept");
    let before = s.d.aws.gets_of(&base_key).len();
    let m = new_machine(&s, "recover-pruned").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "{answer:?}");
    let cmp = answer.comparison.clone().expect("the comparison");
    assert!(
        cmp.done(),
        "the read took the base first and compared: {cmp:?}"
    );
    assert_eq!(
        s.d.aws.gets_of(&base_key).len(),
        before + 1,
        "one download of the bundle the run started from"
    );

    // 4. The storage lacks the base and the bucket cannot give it: the comparison is unknown and says why, and the copy read back
    // is stored all the same.
    let s = stopped_session().await;
    std::fs::rename(
        s.d.worktree().join(".git"),
        s.d.dir.join("history-moved-away"),
    )
    .unwrap();
    s.d.aws.remove(&base_key);
    let m = new_machine(&s, "recover-without-base").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "the tree is the work: {answer:?}");
    assert!(
        s.d.aws.get(&s.key()).is_some(),
        "the copy was stored before anything was compared"
    );
    let cmp = answer.comparison.clone().expect("the comparison says why");
    assert!(!cmp.done(), "{cmp:?}");
    assert_eq!(cmp.reason.as_deref(), Some("baseline-unreadable"));
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_the_key_is_written_once() {
    let s = stopped_session().await;
    s.d.aws.put(&s.key(), b"another writer's object".to_vec());
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(!answer.recovered, "{answer:?}");
    assert_eq!(answer.reason.as_deref(), Some("exists"));
    assert_eq!(answer.copy, None);
    assert_eq!(
        s.d.aws.get(&s.key()).unwrap(),
        b"another writer's object".to_vec(),
        "a taken key is never written over"
    );

    // A PUT whose answer is lost: the store has the object, and the worker's next try finds the key taken (412). The credentials
    // of a read may write that one key and read the saved copy. They may not look at the key: a HEAD is refused.
    let s = stopped_session().await;
    s.d.aws
        .fail_puts_after_store(&s.key(), 1, PutFailure::ResetBody);
    s.d.aws.heads_answer(403);
    let m = new_machine(&s, "lost-answer").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(
        s.d.aws.put_attempts(&s.key()) >= 2,
        "TRIGGER PRESENT: the first PUT lost its answer and the worker tried again: {} PUT(s)",
        s.d.aws.put_attempts(&s.key())
    );
    // The worker cannot tell the object from another writer's, so it names no copy; it says that the upload failed.
    assert_eq!(
        (answer.recovered, answer.reason.as_deref()),
        (false, Some("upload-failed")),
        "{answer:?}"
    );
    assert_eq!(answer.copy, None);
    // The copy is in the bucket all the same: written once, whole, and with the checksum the store validated, so the service can
    // check the object against that checksum with its own grants and needs no grant of the worker to look at the key.
    let stored =
        s.d.aws
            .get(&s.key())
            .expect("the copy the first PUT stored");
    assert_eq!(
        s.d.aws.versions_of(&s.key()).len(),
        1,
        "the later tries were create-only writes that the store refused: the object was written once"
    );
    assert_eq!(s.d.aws.checksum_of(&s.key()), Some(sha256_b64(&stored)));
    assert_eq!(
        std::fs::read_to_string(s.stored_tree("lost-answer").join(LATE_FILE)).unwrap(),
        LATE_TEXT,
        "the object is the copy of the storage, with the file written after the last saved copy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_saved_copy_that_cannot_be_fetched_leaves_the_stored_copy_and_an_unknown_comparison()
{
    let s = stopped_session().await;
    let mut req = s.request(ATTEMPT);
    // The saved copy the request names is not in the bucket.
    s.d.aws
        .put(&keys::snapshot_bundle_key_t(RUN, 1), Vec::new());
    req.baseline.head_sha = "f".repeat(40);
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&req).await;
    assert!(answer.recovered, "{answer:?}");
    assert!(
        s.d.aws.get(&s.key()).is_some(),
        "the copy was stored before anything was compared"
    );
    let cmp = answer.comparison.clone().expect("the comparison says why");
    assert!(!cmp.done(), "{cmp:?}");
    assert_eq!(cmp.reason.as_deref(), Some("baseline-unreadable"));
    assert_eq!(
        std::fs::read_to_string(s.stored_tree("kept").join(LATE_FILE)).unwrap(),
        LATE_TEXT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_time_that_ends_before_the_copy_is_stored_leaves_no_copy_and_after_it_keeps_the_copy()
{
    // Before: the PUT of the copy takes longer than the request's time.
    let s = stopped_session().await;
    s.d.aws.delay_puts(&s.key(), Duration::from_secs(4));
    let mut req = s.request(ATTEMPT);
    req.budget_s = 2;
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&req).await;
    assert!(!answer.recovered, "{answer:?}");
    assert_eq!(answer.reason.as_deref(), Some("budget"));
    assert!(answer.took_ms < 3_500, "{answer:?}");

    // After: the copy is stored, and the fetch of the saved copy takes longer than what is left.
    let s = stopped_session().await;
    s.d.aws
        .delay_gets(&keys::snapshot_bundle_key_t(RUN, 1), Duration::from_secs(6));
    let mut req = s.request(ATTEMPT);
    req.budget_s = 3;
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&req).await;
    assert!(
        answer.recovered,
        "a copy that is stored stays stored: {answer:?}"
    );
    assert!(s.d.aws.get(&s.key()).is_some());
    let cmp = answer.comparison.clone().expect("the comparison says why");
    assert!(!cmp.done(), "{cmp:?}");
    assert_eq!(cmp.reason.as_deref(), Some("budget"));
    assert_eq!(
        std::fs::read_to_string(s.stored_tree("kept").join(LATE_FILE)).unwrap(),
        LATE_TEXT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_missing_mount_or_a_linked_folder_is_no_tree() {
    let s = stopped_session().await;
    // No storage at the root.
    let m = machine_at(&s, "no-mount", &s.d.dir.join("no-such-mount")).await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert_eq!(
        (answer.recovered, answer.reason.as_deref()),
        (false, Some("not-mounted"))
    );
    // A storage whose root is there and holds no folder of the run.
    let empty = s.d.dir.join("empty-mount");
    std::fs::create_dir_all(&empty).unwrap();
    let m = machine_at(&s, "empty", &empty).await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert_eq!(
        (answer.recovered, answer.reason.as_deref()),
        (false, Some("no-tree"))
    );
    // The repository is a link to a folder elsewhere.
    let linked = s.d.dir.join("linked-mount");
    std::fs::create_dir_all(linked.join(RUN).join("direct")).unwrap();
    std::os::unix::fs::symlink(s.d.worktree(), linked.join(RUN).join("direct/repo")).unwrap();
    let m = machine_at(&s, "linked", &linked).await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert_eq!(
        (answer.recovered, answer.reason.as_deref()),
        (false, Some("no-tree"))
    );
    assert!(
        s.d.aws.get(&s.key()).is_none(),
        "none of them stored anything"
    );
    // The same storage, mounted as it is: a copy.
    let m = new_machine(&s, "mounted").await;
    assert!(m.recover(&s.request(ATTEMPT)).await.recovered);
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_the_requests_secrets_reach_no_log_line() {
    let s = stopped_session().await;
    // A read that stores its copy, and reads that fail in three places.
    let m = new_machine(&s, "recover").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    assert!(answer.recovered, "{answer:?}");
    let again = stopped_session().await;
    again.d.aws.fail_puts(&again.key(), u32::MAX);
    let failing = new_machine(&again, "failing").await;
    let failed = failing.recover(&again.request(ATTEMPT)).await;
    assert_eq!(
        failed.reason.as_deref(),
        Some("upload-failed"),
        "{failed:?}"
    );
    let refused =
        s.d.post_raw(serde_json::to_string(&s.request(OTHER_ATTEMPT)).unwrap())
            .await;
    assert_eq!(refused.0, 409);
    let mut bad = serde_json::to_value(s.request(OTHER_ATTEMPT)).unwrap();
    bad["budgetS"] = serde_json::json!(0);
    let (code, text) = failing.post(bad.to_string()).await;
    assert_eq!(code, 400, "{text}");
    for text in [
        m.log.lines().join("\n"),
        failing.log.lines().join("\n"),
        s.d.log.lines().join("\n"),
        refused.1,
        text,
        serde_json::to_string(&answer).unwrap(),
        serde_json::to_string(&failed).unwrap(),
    ] {
        no_secrets(&text);
        assert!(!text.contains("secretAccessKey"), "{text}");
        assert!(!text.contains("sessionToken"), "{text}");
    }
    assert!(
        m.log.lines().iter().any(|l| l.contains("direct recover")),
        "the trigger: the read wrote log lines"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_body_of_neither_kind_is_refused_as_before() {
    let s = stopped_session().await;
    let m = new_machine(&s, "probe").await;
    for body in [
        r#"{"kind":"probe"}"#.to_string(),
        r#"{"prompt":"hello"}"#.to_string(),
        "not json".to_string(),
    ] {
        let (code, text) = m.post(body.clone()).await;
        assert_eq!(code, 400, "{body}: {text}");
        assert!(
            text.contains("body is not a DirectInvocationRequest"),
            "{body}: {text}"
        );
    }
    // A recover body with a member the type does not know is refused as a recover body, with a place and not the input.
    let mut odd = serde_json::to_value(s.request(ATTEMPT)).unwrap();
    odd["extra"] = serde_json::json!(true);
    let (code, text) = m.post(odd.to_string()).await;
    assert_eq!(code, 400, "{text}");
    assert!(
        text.contains("body is not a RecoveryInvocationRequest (line 1, column"),
        "{text}"
    );
    // None of them made this machine a recovery machine or a working one: a read still works.
    assert!(m.recover(&s.request(ATTEMPT)).await.recovered);
}

#[tokio::test]
async fn b854_a_file_that_is_a_prefix_of_the_saved_copys_file_is_counted_and_a_shorter_rewrite_is_not()
 {
    let dir = scratch("b854-compare");
    let repo = python_calc(&dir);
    let put = |name: &str, text: &str| std::fs::write(repo.join(name), text).unwrap();
    put("log.txt", "one\n");
    put("notes.txt", "first version\n");
    put("short.txt", "abcdef\n");
    put("gone.txt", "x\n");
    put("grows.txt", "a\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "the files of the test"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    // The session works, and a copy is saved.
    put("log.txt", "one\ntwo\nthree\n");
    put("notes.txt", "second version\n");
    put("short.txt", "abcdef\nmore\n");
    let data = dir.join("session-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut session = Snapshotter::new(&data, &repo, &base).await.unwrap();
    let saved = session.snapshot().await.unwrap().expect("the tree changed");

    // The storage as a stop left it, older than the saved copy in places.
    put("log.txt", "one\ntwo\n"); // the saved copy holds more of it: a strict prefix
    put("notes.txt", "first version\n"); // the committed version, below the saved copy's
    put("short.txt", "xyz\n"); // shorter, and no prefix
    std::fs::remove_file(repo.join("gone.txt")).unwrap(); // only the saved copy holds it
    put("grows.txt", "a\nb\n"); // longer
    let private = dir.join("recover-data");
    std::fs::create_dir_all(&private).unwrap();
    let mut reader = Snapshotter::for_read_back(&private, &repo, &base)
        .await
        .unwrap();
    let made = reader.read_back(&base, 1, &GiveUp::at(None)).await.unwrap();
    assert!(made.stable);
    assert_eq!(made.parent.as_deref(), Some(base.as_str()));
    let baseline = reader.fetch_copy(&saved.bundle).await.unwrap();
    assert_eq!(
        baseline, saved.commit,
        "the trigger: the saved copy was fetched"
    );
    let found = reader
        .compare(&made.tree, &baseline, 64 * 1024 * 1024, 5)
        .await
        .unwrap();
    assert!(!found.same);
    assert_eq!(
        (found.regressed, found.regressed_paths.clone()),
        (1, vec!["log.txt".to_string()]),
        "{found:?}"
    );
    assert_eq!(
        (found.rewritten, found.rewritten_paths.clone()),
        (1, vec!["notes.txt".to_string()]),
        "{found:?}"
    );
    // A shorter file whose longer side is too large to read is counted unread, and at most as many paths are shown as asked for.
    let unread = reader.compare(&made.tree, &baseline, 1, 1).await.unwrap();
    assert_eq!(unread.regressed, 3, "{unread:?}");
    assert_eq!(unread.regressed_paths.len(), 1, "{unread:?}");
    let (files, bytes) = reader.totals(&made.tree).await.unwrap();
    assert!(files >= 4 && bytes > 0, "{files} files, {bytes} bytes");

    // A storage that holds exactly the saved copy: the same tree.
    put("log.txt", "one\ntwo\nthree\n");
    put("notes.txt", "second version\n");
    put("short.txt", "abcdef\nmore\n");
    put("gone.txt", "x\n");
    put("grows.txt", "a\n");
    let private = dir.join("recover-data-2");
    std::fs::create_dir_all(&private).unwrap();
    let mut reader = Snapshotter::for_read_back(&private, &repo, &base)
        .await
        .unwrap();
    let made = reader.read_back(&base, 1, &GiveUp::at(None)).await.unwrap();
    let baseline = reader.fetch_copy(&saved.bundle).await.unwrap();
    let found = reader
        .compare(&made.tree, &baseline, 64 * 1024 * 1024, 5)
        .await
        .unwrap();
    assert!(found.same, "{found:?}");
    assert_eq!((found.regressed, found.rewritten), (0, 0));
}

/// A file the saved copy added (the commit below it does not hold it) that the storage lacks. A copy of a working session reads
/// the tree through the machine's file cache, so it can hold a file that never reached the storage, and the read then finds a tree
/// without it. That reads the same as a file the engine removed after the copy, so it is doubt and no proof: it is counted with the
/// files of which the saved copy holds more, and the service goes on from the saved copy while both copies stay downloadable. A file
/// that the commit below the saved copy already held and the storage lacks was removed since then: a deletion, which is no doubt.
#[tokio::test]
async fn b854_a_file_the_saved_copy_added_and_the_storage_lacks_is_counted_and_a_file_removed_since_the_commit_below_is_not()
 {
    let dir = scratch("b854-compare-missing");
    let repo = python_calc(&dir);
    let put = |name: &str, text: &str| std::fs::write(repo.join(name), text).unwrap();
    put("old.txt", "committed\n");
    put("stays.txt", "committed\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "the files of the test"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    // The session works: it makes two new files, and a copy is saved.
    put("notes.md", "made after the commit\n");
    put("draft.md", "made after the commit too\n");
    let data = dir.join("session-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut session = Snapshotter::new(&data, &repo, &base).await.unwrap();
    let saved = session.snapshot().await.unwrap().expect("the tree changed");

    // The storage as a stop left it: it lacks one file the saved copy added and one file the commit below already held, and it
    // holds one file written after the copy.
    std::fs::remove_file(repo.join("notes.md")).unwrap();
    std::fs::remove_file(repo.join("old.txt")).unwrap();
    put("later.txt", "written after the copy\n");
    let private = dir.join("recover-data");
    std::fs::create_dir_all(&private).unwrap();
    let mut reader = Snapshotter::for_read_back(&private, &repo, &base)
        .await
        .unwrap();
    let made = reader.read_back(&base, 1, &GiveUp::at(None)).await.unwrap();
    let baseline = reader.fetch_copy(&saved.bundle).await.unwrap();
    assert_eq!(
        baseline, saved.commit,
        "the trigger: the saved copy was fetched"
    );
    let found = reader
        .compare(&made.tree, &baseline, 64 * 1024 * 1024, 5)
        .await
        .unwrap();
    assert!(!found.same);
    assert_eq!(
        (found.regressed, found.regressed_paths.clone()),
        (1, vec!["notes.md".to_string()]),
        "the file the saved copy added and the storage lacks is doubt; the file removed since the commit below is not: {found:?}"
    );
    assert_eq!(found.rewritten, 0, "{found:?}");

    // Trigger: with that file on the storage, the same comparison finds nothing in doubt, although the trees differ.
    put("notes.md", "made after the commit\n");
    let private = dir.join("recover-data-2");
    std::fs::create_dir_all(&private).unwrap();
    let mut reader = Snapshotter::for_read_back(&private, &repo, &base)
        .await
        .unwrap();
    let made = reader.read_back(&base, 1, &GiveUp::at(None)).await.unwrap();
    let baseline = reader.fetch_copy(&saved.bundle).await.unwrap();
    let found = reader
        .compare(&made.tree, &baseline, 64 * 1024 * 1024, 5)
        .await
        .unwrap();
    assert!(
        !found.same,
        "TRIGGER PRESENT: old.txt is gone and later.txt is new"
    );
    assert_eq!((found.regressed, found.rewritten), (0, 0), "{found:?}");
}

/// The git of this machine's search path.
fn real_git() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("git"))
                .find(|git| git.is_file())
        })
        .expect("git on the search path")
}

/// A stand-in for a storage, or a disk, that stops answering while git works on it: a `git` that never returns from a call that
/// names `word` (from every call when `word` is empty), in a folder that leads the search path of a real `agentd serve`. It writes
/// the words of each call it holds to the file it returns. Every other call is the real git.
fn stalling_git(dir: &Path, name: &str, word: &str) -> (PathBuf, PathBuf) {
    let real = real_git();
    let bin = dir.join(format!("{name}-bin"));
    std::fs::create_dir_all(&bin).unwrap();
    let held = dir.join(format!("{name}-held.txt"));
    let hold = format!("echo \"$*\" >> '{}'\nexec sleep 600\n", held.display());
    let script = if word.is_empty() {
        format!("#!/bin/sh\n{hold}")
    } else {
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\nif [ \"$a\" = {word} ]; then\n{hold}fi\ndone\nexec '{}' \"$@\"\n",
            real.display()
        )
    };
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, held)
}

/// The stand-ins of git that the process `pid` still has as children (`sleep`, which a held call becomes).
fn held_children(pid: u32) -> String {
    let out = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string(), "sleep"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Waits until the process `pid` has no held git left; whether that came within five seconds.
async fn no_git_left(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !held_children(pid).is_empty() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

/// The read has one limit, the request's `budgetS`, and every git the read started ends with it. Here the git itself does not
/// return, in a real `agentd serve`: at the first call of the private repository (its setup), at the reading of the base into it
/// (the seed) and at the bundle. Each time the read ends at its limit, nothing is in the bucket and no git is left behind. When the
/// git that does not return is the one that fetches the saved copy for the comparison, the copy is stored already: it stays, and
/// the comparison is unknown.
#[tokio::test(flavor = "multi_thread")]
async fn b854_a_git_that_does_not_return_ends_at_the_budget_and_no_git_is_left_behind() {
    let s = stopped_session().await;
    let dir = scratch("b854-slow-git");
    for (name, word, budget_s) in [
        ("first-call", "", 1_u32),
        ("seed", "read-tree", 2),
        ("bundle", "bundle", 4),
    ] {
        let (bin, held) = stalling_git(&dir, name, word);
        let m = process_machine(&s, &bin).await;
        let mut req = s.request(ATTEMPT);
        req.budget_s = budget_s;
        let asked = Instant::now();
        let answer = recover_on(&m, &req).await;
        let took = asked.elapsed();
        let held_calls = std::fs::read_to_string(&held).unwrap_or_default();
        assert!(
            !held_calls.is_empty() && held_calls.contains(word),
            "TRIGGER PRESENT: {name}: the read's git is the stand-in and one of its calls did not return: {held_calls:?}; {answer:?}"
        );
        assert_eq!(
            (answer.recovered, answer.reason.as_deref()),
            (false, Some("budget")),
            "{name}: {answer:?}"
        );
        assert_eq!(answer.copy, None, "{name}");
        assert!(
            took < Duration::from_secs(u64::from(budget_s) + 4),
            "{name}: the read ended {took:?} after it began; its limit is {budget_s} s"
        );
        assert!(
            s.d.aws.get(&s.key()).is_none() && s.d.aws.put_attempts(&s.key()) == 0,
            "{name}: nothing is in the bucket, and nothing was sent"
        );
        let pid = m.process.as_ref().unwrap().pid();
        assert!(
            no_git_left(pid).await,
            "{name}: a git of the read still runs after the read ended: {}",
            held_children(pid)
        );
    }

    // After the copy is stored: the fetch of the saved copy does not return.
    let (bin, held) = stalling_git(&dir, "fetch", "fetch");
    let m = process_machine(&s, &bin).await;
    let mut req = s.request(ATTEMPT);
    req.budget_s = 8;
    let answer = recover_on(&m, &req).await;
    let held_calls = std::fs::read_to_string(&held).unwrap_or_default();
    assert!(
        held_calls.contains("fetch"),
        "TRIGGER PRESENT: the git that fetches the saved copy is the stand-in and did not return: {held_calls:?}; {answer:?}"
    );
    assert!(
        answer.recovered,
        "a copy that is stored stays stored: {answer:?}"
    );
    let cmp = answer.comparison.clone().expect("the comparison says why");
    assert!(!cmp.done(), "{cmp:?}");
    assert_eq!(cmp.reason.as_deref(), Some("budget"));
    assert_eq!(
        std::fs::read_to_string(s.stored_tree("kept-slow-git").join(LATE_FILE)).unwrap(),
        LATE_TEXT
    );
    let pid = m.process.as_ref().unwrap().pid();
    assert!(
        no_git_left(pid).await,
        "a git of the read still runs after the read ended: {}",
        held_children(pid)
    );
}
