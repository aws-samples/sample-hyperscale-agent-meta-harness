//! The marker of the copies on the session's storage (`src/direct/marker.rs`).
//!
//! A session that was stopped from outside leaves its files on its storage, and the service reads them back. It starts the next session
//! from them only when the storage itself says that it stands at the last saved copy. agentd writes that down in
//! `<workspace_root>/<run>/copies.json`: once when the session's restore check passed, naming the commit it starts on, and after the
//! bundle of each copy is stored, naming that copy. The file sits in the run's folder, beside the engine's folder and not in it. A copy
//! whose bundle was not stored does not move the marker, and a marker that cannot be written never fails a copy. The reader takes a
//! marker only when the trusted user alone can write it.
#![allow(clippy::unwrap_used)]

mod b854_support;
mod common;
mod direct_support;

use agentd::direct::marker;
use b854_support::{ATTEMPT, LATE_FILE, LATE_TEXT, machine_for, stopped_session, storage_owner};
use common::{git, scratch};
use direct_support::{BUCKET, Direct, Opts, RUN};
use harness_protocol::direct::{CopyMarker, DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where the marker of the run is on the storage of `d`.
fn marker_file(d: &Direct) -> PathBuf {
    d.dir.join("mnt-workspace").join(RUN).join("copies.json")
}

/// The marker on the storage of `d` as it is now; `None` while there is none.
fn marker_now(d: &Direct) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(marker_file(d)).ok()?).ok()
}

/// The commit the repository of `d` is checked out at: the head of the bundle the run starts from.
fn checkout_commit(d: &Direct) -> String {
    git(&d.dir, &["bundle", "list-heads", "repo.bundle"])
        .lines()
        .find_map(|line| line.strip_suffix(" HEAD").map(str::to_string))
        .expect("the bundle names its HEAD")
}

fn record_of(d: &Direct, session: u32) -> Value {
    serde_json::from_slice(
        &d.aws
            .get(&keys::snapshot_json_key_t(RUN, session))
            .expect("snapshot.json"),
    )
    .unwrap()
}

fn saved_copies(d: &Direct) -> Vec<Value> {
    d.aws
        .events()
        .into_iter()
        .filter(|(t, _)| *t == EventType::TurnProgress)
        .filter_map(|(_, v)| v.get("savedCopy").cloned())
        .collect()
}

/// Runs session 1 to its end and returns what the service hands to session 2: the bundle, its head and the run's base.
async fn first_session() -> (Vec<u8>, String, String) {
    let d1 = Direct::start(Opts::default()).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("session 1 delivered a head");
    let base = s.git.base_sha.clone().expect("session 1 has a base");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("session 1 bundled its work");
    (bundle, head, base)
}

/// Session 2 of the run in a new machine, with the copies made while the engine works switched off: its one copy is the copy of its end.
async fn second_session(bundle: Vec<u8>, head: &str, base: &str) -> Direct {
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.snapshot_every_s = Some(0);
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d2.req).expect("the invocation of session 2 is valid");
    d2
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_the_marker_names_each_stored_copy_and_the_copy_the_session_started_from() {
    let d = Direct::start(Opts::default()).await;
    let start = checkout_commit(&d);
    assert!(marker_now(&d).is_none(), "a new storage has no marker");
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    // The first marker on the storage, and how many prompts the engine had got when it was there.
    let deadline = Instant::now() + Duration::from_secs(60);
    let (first, prompts) = loop {
        if let Some(m) = marker_now(&d) {
            break (m, d.prompts_received().len());
        }
        assert!(Instant::now() < deadline, "no marker was written");
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    assert_eq!(
        first,
        json!({"schemaVersion": 1, "session": 1, "count": 0, "headSha": start}),
        "the first marker names the session and the commit it was checked out at, before the starter commit of the harness"
    );
    assert_eq!(
        prompts, 0,
        "the marker of the start is on the storage before the engine works"
    );
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The session stored a copy (the trigger), and the marker names it.
    let record = record_of(&d, 1);
    assert!(!saved_copies(&d).is_empty(), "the session stored a copy");
    let last = marker_now(&d).expect("the marker after the copies");
    assert_eq!(last["schemaVersion"], 1, "{last}");
    assert_eq!(last["session"], 1, "{last}");
    assert_eq!(last["count"], record["count"], "{last} against {record}");
    assert_eq!(
        last["headSha"], record["headSha"],
        "{last} against {record}"
    );
    assert_ne!(
        last["headSha"],
        json!(start),
        "the copy is another commit than the start"
    );
    // It is in the run's folder, beside the engine's folder, and no temporary file is left.
    let run_folder = d.dir.join("mnt-workspace").join(RUN);
    assert!(!run_folder.join("direct/copies.json").exists());
    assert!(!d.worktree().join("copies.json").exists());
    let names: Vec<String> = std::fs::read_dir(&run_folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, ["copies.json", "direct"], "{names:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_copy_whose_bundle_was_not_stored_does_not_move_the_marker() {
    let d = Direct::start(Opts::default()).await;
    let key = keys::snapshot_bundle_key_t(RUN, 1);
    d.aws.fail_puts(&key, u32::MAX);
    let (_, resp) = d.invoke().await;
    assert!(
        d.aws.put_attempts(&key) >= 1,
        "the trigger: the session tried to store a copy: {resp:?}"
    );
    assert!(
        d.aws.get(&key).is_none(),
        "no bundle of a copy is in the bucket"
    );
    let m = marker_now(&d).expect("the marker of the start");
    assert_eq!(
        m,
        json!({"schemaVersion": 1, "session": 1, "count": 0, "headSha": checkout_commit(&d)}),
        "the marker still names the commit the session started on"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_marker_that_cannot_be_written_does_not_fail_the_saved_copy() {
    let d = Direct::start(Opts::default()).await;
    // A folder with a file in it stands where the marker goes: no rename can replace it.
    std::fs::create_dir_all(marker_file(&d).join("in-the-way")).unwrap();
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        marker_file(&d).is_dir(),
        "the trigger: no marker could be written"
    );
    let record = record_of(&d, 1);
    assert!(
        d.aws.get(&keys::snapshot_bundle_key_t(RUN, 1)).is_some(),
        "the copy is stored: {record}"
    );
    assert!(!saved_copies(&d).is_empty(), "the run was told of the copy");
    let lines = d.log.lines();
    let about_copies: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("direct turn copy marker"))
        .collect();
    assert_eq!(
        about_copies.len(),
        1,
        "a marker that cannot be written after a copy is logged once: {about_copies:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("direct turn start marker"))
            .count(),
        1,
        "so is the marker of the start"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_later_session_marks_the_commit_it_restored_and_then_its_own_copy() {
    let (bundle, head, base) = first_session().await;
    // With no copy of its own stored, the marker stays what the start wrote: this session and the commit it restored.
    let d2 = second_session(bundle.clone(), &head, &base).await;
    let key = keys::snapshot_bundle_key_t(RUN, 2);
    d2.aws.fail_puts(&key, u32::MAX);
    let (_, resp) = d2.invoke().await;
    assert!(d2.aws.put_attempts(&key) >= 1, "the trigger: {resp:?}");
    assert_eq!(
        marker_now(&d2).expect("the marker of the start"),
        json!({"schemaVersion": 1, "session": 2, "count": 0, "headSha": head}),
        "the commit a session restored is no copy with a number, so the number is 0"
    );
    // With its copy stored, the marker names that copy.
    let d2 = second_session(bundle, &head, &base).await;
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let record = record_of(&d2, 2);
    let last = marker_now(&d2).expect("the marker after the copy");
    assert_eq!(last["session"], 2, "{last}");
    assert_eq!(last["count"], record["count"], "{last} against {record}");
    assert_eq!(
        last["headSha"], record["headSha"],
        "{last} against {record}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b854_a_session_whose_restore_check_fails_writes_no_marker() {
    let (bundle, head, base) = first_session().await;
    let mut d2 = second_session(bundle, &head, &base).await;
    // The service names another commit than the bundle holds.
    d2.req.slice.as_mut().unwrap().restore_head = Some(base.clone());
    let (_, resp) = d2.invoke().await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d2.prompts_received().is_empty(),
        "the trigger: the engine never started"
    );
    assert!(
        !marker_file(&d2).exists(),
        "a session that did not restore the commit says nothing about its storage"
    );
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A run's folder on a storage of its own with a marker agentd wrote; the folder and the file are writable by their owner alone.
async fn storage_with_marker(label: &str) -> (PathBuf, u32, CopyMarker) {
    let root = scratch(label);
    let folder = root.join(RUN);
    std::fs::create_dir_all(&folder).unwrap();
    set_mode(&folder, 0o755);
    let uid = std::fs::metadata(&folder).unwrap().uid();
    let m = CopyMarker {
        schema_version: 1,
        session: 3,
        count: 7,
        head_sha: "a".repeat(40),
    };
    marker::write(&root, RUN, &m).await.unwrap();
    (root, uid, m)
}

#[tokio::test]
async fn b854_a_marker_is_written_whole_and_the_next_one_replaces_it() {
    let (root, uid, m) = storage_with_marker("b854-marker-write").await;
    assert_eq!(marker::path(&root, RUN), root.join(RUN).join("copies.json"));
    assert_eq!(marker::read(&root, RUN, uid), Some(m.clone()));
    let mode = std::fs::metadata(marker::path(&root, RUN)).unwrap().mode() & 0o777;
    assert_eq!(
        mode & 0o022,
        0,
        "neither group nor others may write it: {mode:o}"
    );
    let next = CopyMarker {
        count: 8,
        head_sha: "b".repeat(40),
        ..m
    };
    marker::write(&root, RUN, &next).await.unwrap();
    assert_eq!(marker::read(&root, RUN, uid), Some(next));
    let names: Vec<String> = std::fs::read_dir(root.join(RUN))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["copies.json"], "no temporary file is left");
    // A folder that is not there: the write fails and says so; nothing is made.
    let err = marker::write(&root, "01M4XNOSUCHRUN000000000001", &m)
        .await
        .unwrap_err();
    assert!(err.starts_with("marker: "), "{err}");
    assert!(!root.join("01M4XNOSUCHRUN000000000001").exists());
}

#[tokio::test]
async fn b854_the_reader_takes_only_a_marker_that_the_trusted_user_alone_can_write() {
    let (root, uid, m) = storage_with_marker("b854-marker-trust").await;
    let folder = root.join(RUN);
    let file = marker::path(&root, RUN);
    assert_eq!(
        marker::read(&root, RUN, uid),
        Some(m.clone()),
        "the trigger: the marker is taken as it was written"
    );
    // Another owner than the trusted one.
    assert_eq!(marker::read(&root, RUN, uid + 1), None);
    // A file that group or others may write.
    for mode in [0o664, 0o646, 0o666] {
        set_mode(&file, mode);
        assert_eq!(marker::read(&root, RUN, uid), None, "file mode {mode:o}");
    }
    set_mode(&file, 0o644);
    assert_eq!(marker::read(&root, RUN, uid), Some(m.clone()));
    // A folder that group or others may write: whoever can write there can replace the file.
    for mode in [0o775, 0o757, 0o777] {
        set_mode(&folder, mode);
        assert_eq!(marker::read(&root, RUN, uid), None, "folder mode {mode:o}");
    }
    set_mode(&folder, 0o755);
    assert_eq!(marker::read(&root, RUN, uid), Some(m.clone()));
    // A link where the marker goes, to a file that would be taken.
    let elsewhere = root.join("elsewhere.json");
    std::fs::rename(&file, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
    assert_eq!(
        marker::read(&root, RUN, uid),
        None,
        "a link is not followed"
    );
    std::fs::remove_file(&file).unwrap();
    // No marker at all.
    assert_eq!(marker::read(&root, RUN, uid), None);
    // A file that is no marker of this kind.
    let good = serde_json::to_value(&m).unwrap();
    let mut other_version = good.clone();
    other_version["schemaVersion"] = json!(2);
    let mut no_session = good.clone();
    no_session["session"] = json!(0);
    let mut short_commit = good.clone();
    short_commit["headSha"] = json!("abc1234");
    let mut upper_commit = good.clone();
    upper_commit["headSha"] = json!("A".repeat(40));
    for (what, text) in [
        ("no JSON", "copies".to_string()),
        ("another schema version", other_version.to_string()),
        ("session 0", no_session.to_string()),
        ("a short commit id", short_commit.to_string()),
        ("an upper-case commit id", upper_commit.to_string()),
        ("a file of 8 KiB", format!("{}{}", good, " ".repeat(8192))),
    ] {
        std::fs::write(&file, text).unwrap();
        set_mode(&file, 0o644);
        assert_eq!(marker::read(&root, RUN, uid), None, "{what}");
    }
    std::fs::write(&file, good.to_string()).unwrap();
    set_mode(&file, 0o644);
    assert_eq!(marker::read(&root, RUN, uid), Some(m.clone()));
    // The run's folder is a link to a folder with a marker that would be taken.
    let real = root.join("real-folder");
    std::fs::rename(&folder, &real).unwrap();
    std::os::unix::fs::symlink(&real, &folder).unwrap();
    assert_eq!(
        marker::read(&root, RUN, uid),
        None,
        "a linked folder is not the run's folder"
    );
}

/// One way in which the marker on a stopped session's storage cannot be trusted.
#[derive(Clone, Copy, Debug)]
enum Untrusted {
    /// It belongs to another user than the one the machine trusts.
    AnotherOwner,
    FileGroupWritable,
    FileOtherWritable,
    /// Whoever can write the run's folder can replace the file.
    FolderGroupWritable,
    FolderOtherWritable,
    /// A link stands where the marker goes, to a file that would be taken.
    Link,
    Unreadable,
    Missing,
}

/// Reads the storage of a stopped session back through the whole read (a new machine, `/invocations` with a body of kind `recover`)
/// after its marker was made untrusted in the way `how` (`None`: the marker is left as agentd wrote it). Returns the marker the
/// answer names, and whether the copy in the bucket holds the file written after the last saved copy.
async fn read_back_with_marker(how: Option<Untrusted>) -> (Option<CopyMarker>, bool) {
    let s = stopped_session().await;
    let folder = s.storage().join(RUN);
    let file = folder.join("copies.json");
    assert!(
        file.is_file(),
        "TRIGGER PRESENT: session 1 left its marker on the storage"
    );
    let mut owner = storage_owner(&s);
    match how {
        None => {}
        Some(Untrusted::AnotherOwner) => owner += 1,
        Some(Untrusted::FileGroupWritable) => set_mode(&file, 0o664),
        Some(Untrusted::FileOtherWritable) => set_mode(&file, 0o646),
        Some(Untrusted::FolderGroupWritable) => set_mode(&folder, 0o775),
        Some(Untrusted::FolderOtherWritable) => set_mode(&folder, 0o757),
        Some(Untrusted::Link) => {
            let elsewhere = s.d.dir.join("copies-elsewhere.json");
            std::fs::rename(&file, &elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
        }
        Some(Untrusted::Unreadable) => set_mode(&file, 0o000),
        Some(Untrusted::Missing) => std::fs::remove_file(&file).unwrap(),
    }
    let m = machine_for(&s, "marker", &s.storage(), owner).await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    if matches!(how, Some(Untrusted::Unreadable)) {
        set_mode(&file, 0o644);
    }
    assert!(
        answer.recovered,
        "{how:?}: the work is stored whatever the marker says: {answer:?}"
    );
    let copy = answer.copy.clone().expect("the stored copy");
    let late = std::fs::read_to_string(s.stored_tree("marker").join(LATE_FILE)).ok();
    (copy.marker, late.as_deref() == Some(LATE_TEXT))
}

/// The marker's trust through the whole read. The service starts a session from a copy read back only when the answer of the read
/// names the storage's marker (the use rule of run-session refuses a copy without one). Whatever is wrong with the marker, the read
/// still stores its copy, with the file written after the last saved copy in it: a marker that cannot be trusted keeps the work from
/// being used, never from being kept.
#[tokio::test(flavor = "multi_thread")]
async fn b854_a_marker_that_cannot_be_trusted_lets_the_read_store_its_copy_and_is_not_passed_on() {
    let mut cases = vec![
        Untrusted::AnotherOwner,
        Untrusted::FileGroupWritable,
        Untrusted::FileOtherWritable,
        Untrusted::FolderGroupWritable,
        Untrusted::FolderOtherWritable,
        Untrusted::Link,
        Untrusted::Missing,
    ];
    // The user root reads a file of any mode, so the unreadable marker is a case for every other user alone.
    if std::fs::metadata(scratch("b854-marker-who")).unwrap().uid() != 0 {
        cases.push(Untrusted::Unreadable);
    }
    // The trigger: a marker as agentd left it is named in the answer.
    let (marker, late) = read_back_with_marker(None).await;
    let marker = marker.expect("TRIGGER PRESENT: the marker of an untouched storage is passed on");
    assert_eq!(marker.session, 1, "{marker:?}");
    assert!(
        late,
        "the copy holds the file written after the last saved copy"
    );
    for how in cases {
        let (marker, late) = read_back_with_marker(Some(how)).await;
        assert!(
            late,
            "{how:?}: the copy in the bucket holds the file written after the last saved copy"
        );
        assert_eq!(
            marker, None,
            "{how:?}: a marker that cannot be trusted is not passed on, so no session starts from the copy"
        );
    }
}
