//! The storage watch in whole runs: the real driver in process, the fake engine, a fake AWS. No AWS call is made.
//!
//! The scenario `no_space_result` is `writes_files` plus a command whose result names "No space left on device". The words alone bring
//! no notice: the worker tests the storage itself, and the test's storage takes the write. With the switch of the live test row the
//! worker's one test write is answered with errno 28, and the run says so once, and stays the engine's.
//!
//! The switch answers the first test write that a tool result asks for once a copy of the session is marked, so the runs that use it
//! step the engine (`FAKE_ACP_STEP_FILE`): the two edits first, then, once the copy task has stored a copy of them and written its
//! marker through the watch, the command whose result names no space. The notice then names that copy with the version the bucket
//! answered for its bundle.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::Value;
use std::time::{Duration, Instant};

/// What the notice says about this storage (the lead's ruling 9), word for word.
const KNOWN: &str = "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up. In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.";

fn storage_notices(d: &Direct) -> Vec<Value> {
    d.events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "notify" && v["reason"] == "storage_refused")
        .collect()
}

fn storage_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn storage"))
        .collect()
}

/// The names in the run's own folder on the session's storage: the engine's folder, the marker of the copies, and no test file.
fn run_folder(d: &Direct) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d.dir.join("mnt-workspace").join(RUN))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

/// The number of the copy the marker on the session's storage names; 0 for the marker of the session's start and for no marker.
fn marked_copy(d: &Direct) -> u64 {
    std::fs::read(d.dir.join("mnt-workspace").join(RUN).join("copies.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["count"].as_u64())
        .unwrap_or(0)
}

/// Whether the newest record of a copy in the bucket is the copy of the end.
fn end_copy_stored(d: &Direct) -> bool {
    d.aws
        .get(&keys::snapshot_json_key(RUN))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["final"] == true)
}

async fn until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One run of `no_space_result` whose engine is stepped: its two edits, then nothing until a copy of the work is stored and its
/// marker is on the session's storage, then the rest of its reply (the command whose result names no space, and the end of the
/// turn). `refuse` sets the switch of the live test row.
async fn stepped(refuse: bool, label: &str) -> (Direct, InvocationResponse) {
    let dir = common::scratch(label);
    let steps = dir.join("engine-steps");
    // The first four messages of the reply are the two edits (a tool call and its update each).
    std::fs::write(&steps, "4\n").unwrap();
    let d = Direct::start(Opts {
        scenario: "no_space_result",
        engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
        edit_cfg: Some(Box::new(move |c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
            c.direct.storage_test_refusal = refuse;
        })),
        ..Opts::default()
    })
    .await;
    let gate = async {
        let marked = until(Duration::from_secs(90), || marked_copy(&d) >= 1).await;
        // The engine goes on either way, so that a run that never marked a copy still ends and the assertion below can say so.
        std::fs::write(&steps, "1000000\n").unwrap();
        marked
    };
    let ((_, resp), marked) = tokio::join!(d.invoke(), gate);
    assert!(
        marked,
        "TRIGGER PRESENT is not shown: no copy was stored and marked before the command ran; log: {:?}",
        d.log.lines()
    );
    (d, resp)
}

/// The id a notice's own text names in its `--before-refusal` command.
fn id_in(detail: &str) -> String {
    let at = detail
        .find("--before-refusal ")
        .expect("the notice names its command");
    detail[at + "--before-refusal ".len()..]
        .chars()
        .take(26)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn b881_a_refused_storage_brings_one_notice_and_the_run_stays_the_engines() {
    // The same run without the fault: what the engine's run reads like.
    let (plain, plain_resp) = stepped(false, "b881-plain").await;
    let plain_summary = plain.summary();
    assert!(storage_notices(&plain).is_empty());

    let (d, resp) = stepped(true, "b881-fault").await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(resp.outcome, plain_resp.outcome);

    // One notice, at the tool result that named no space, with the size of the files and both facts about the copies.
    let notes = storage_notices(&d);
    assert_eq!(notes.len(), 1, "{:?}", d.events_of(EventType::TurnProgress));
    println!("B881 TRIGGER PRESENT: the run recorded one storage_refused notice");
    let n = &notes[0];
    assert_eq!(n["storage"]["seen"], "tool");
    assert_eq!(n["storage"]["session"], 1);
    assert_eq!(n["storage"]["stretch"], 1);
    assert_eq!(n["storage"]["quotaMb"], 1024);
    assert_eq!(
        n["storage"]["testSwitch"], true,
        "a record of a test is marked as one"
    );
    assert!(
        n["storage"]["usedMb"].as_u64().is_some_and(|mb| mb < 864),
        "{n}"
    );
    let detail = n["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("The session's storage refused a write while its files add up to ")
            && detail.contains(&format!(
                " MiB of 1024 MiB (no space left on device). {KNOWN} A file that a command wrote meanwhile can be empty; ham does not know which."
            )),
        "{detail}"
    );
    assert!(!detail.contains("cannot read why"), "{detail}");
    assert!(
        detail.contains(&format!(
            "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
        )),
        "{detail}"
    );
    assert!(
        detail.ends_with(
            "This notice was made by the stage's test switch; the storage refused no write."
        ),
        "{detail}"
    );
    // The copy the notice names is a copy of this session that the copy task marked through the watch, and the command names the
    // notice's own id.
    let copy = &n["storage"]["copyBefore"];
    assert_eq!(copy["session"], 1, "{n}");
    let count = copy["count"].as_u64().expect("the copy has its number");
    assert!(count >= 1, "{n}");
    let id = id_in(detail);
    assert!(
        id.chars().count() == 26 && harness_protocol::common::is_ulid(&id),
        "{id}"
    );
    assert!(
        detail.contains(&format!("Copy {count} (")) && detail.contains(&format!(
            " UTC) is the last copy saved before ham first saw this: ham pull {RUN} --before-refusal {id} downloads it."
        )),
        "{detail}"
    );
    let lines = storage_lines(&d);
    assert!(
        lines.iter().any(|l| l.contains("the stage's test switch answered this test write with no space left on device; the storage refused no write")),
        "{lines:?}"
    );

    // The run stays the engine's: its label and its detail are byte for byte those of the same run without the fault, nothing was
    // sent to the engine, and no ladder event followed.
    let s = d.summary();
    assert_eq!(s.outcome, plain_summary.outcome);
    assert_eq!(s.label, plain_summary.label);
    assert_eq!(s.detail, plain_summary.detail);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(
        d.prompts_received().len(),
        1,
        "the engine got its one prompt and nothing else"
    );
    assert_eq!(d.prompts_received(), plain.prompts_received());
    let progress = d.events_of(EventType::TurnProgress);
    for kind in [
        "needs-input",
        "provider-retry",
        "provider-switch",
        "new-session",
        "engine-stop",
    ] {
        assert!(
            !progress.iter().any(|v| v["kind"] == kind),
            "{kind}: {progress:?}"
        );
    }
    assert_eq!(
        progress.iter().filter(|v| v["kind"] == "notify").count(),
        plain
            .events_of(EventType::TurnProgress)
            .iter()
            .filter(|v| v["kind"] == "notify")
            .count()
            + 1,
        "the storage notice is the only notice the fault added"
    );
    // The copy of the end is stored, as in the run without the fault. (How many copies a run stores before it follows the clock of
    // the copy task, so the two runs are not held to one number.)
    assert!(end_copy_stored(&d), "the copy of the end");
    assert!(
        end_copy_stored(&plain),
        "the copy of the end of the plain run"
    );
    // The switch answered one test write and never again: the later test writes were real, and none left a file.
    assert_eq!(run_folder(&d), run_folder(&plain));
    assert!(
        !run_folder(&d).iter().any(|n| n.contains("storage-test")),
        "{:?}",
        run_folder(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b881_the_notice_names_the_version_the_bucket_answered_and_the_bundle_is_hashed_once() {
    let (d, resp) = stepped(true, "b881-version").await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let notes = storage_notices(&d);
    assert_eq!(notes.len(), 1, "{:?}", d.events_of(EventType::TurnProgress));
    println!("B881 TRIGGER PRESENT: the run recorded one storage_refused notice that names a copy");
    let copy = &notes[0]["storage"]["copyBefore"];
    let count = usize::try_from(copy["count"].as_u64().unwrap()).unwrap();
    let head = copy["headSha"].as_str().unwrap();
    let version = copy["versionId"].as_str().unwrap();
    assert!(
        head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()),
        "{copy}"
    );
    // The bucket keeps every version of the one key the copies of a session are stored at. The notice names the version the bucket
    // answered for the upload of exactly that copy: the n-th copy is the n-th version, and its bytes are a bundle of the commit the
    // notice names. Later copies replaced it at the key, the copy of the end among them.
    let versions = d.aws.versions_of(&keys::snapshot_bundle_key(RUN));
    assert!(
        versions.len() > count,
        "the copy of the end replaced it: {}",
        versions.len()
    );
    let (stored_version, body) = &versions[count - 1];
    assert_eq!(stored_version, version, "{copy}");
    let header = String::from_utf8_lossy(&body[..body.len().min(8192)]).to_string();
    assert!(
        header.contains(&format!("{head} refs/heads/snapshot")),
        "the version holds the commit the notice names: {}",
        header.lines().take(4).collect::<Vec<_>>().join(" | ")
    );
    assert_ne!(
        d.aws.get(&keys::snapshot_bundle_key(RUN)).as_deref(),
        Some(body.as_slice()),
        "the newest object at the key is another copy"
    );
    assert_eq!(
        copy["at"].as_str().map(str::len),
        Some(20),
        "the copy's time, to the second: {copy}"
    );

    // The upload that returns the version hashes nothing: the hash the copy task made of its bundle is the only one. The stand-in
    // answers a version for the write, and the function returns that one.
    let file = common::scratch("b881-upload").join("copy.bundle");
    std::fs::write(&file, vec![7u8; 300 * 1024]).unwrap();
    let digest = agentd::s3::digest_file(&file).await.unwrap();
    let objects = agentd::s3::S3Objects::new("us-west-2".into(), Some(d.aws.base.clone()));
    let key = "runs/b881/agents/direct/turns/1/snapshot.bundle";
    let hashed_before = agentd::s3::digests_made();
    let answered = objects
        .put_file_retrying_with_version(
            &format!("s3://{}/{key}", direct_support::BUCKET),
            &common::creds("2099-01-01T00:00:00Z"),
            &file,
            &digest,
            2,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(
        agentd::s3::digests_made() - hashed_before,
        0,
        "the upload hashed the bundle again"
    );
    let stored = d.aws.versions_of(key);
    assert_eq!(stored.len(), 1);
    assert_eq!(answered.as_deref(), Some(stored[0].0.as_str()));
    // A bucket that keeps no versions answers none, and the upload says so.
    d.aws.without_version_ids();
    let none = objects
        .put_file_retrying_with_version(
            &format!("s3://{}/{key}", direct_support::BUCKET),
            &common::creds("2099-01-01T00:00:00Z"),
            &file,
            &digest,
            0,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(none, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn b881_a_result_that_only_quotes_the_words_brings_no_notice() {
    let d = Direct::start(Opts {
        scenario: "no_space_result",
        // What a session can set does not reach the switch: the variable in the engine's own environment and files of the work tree
        // that carry its name change nothing.
        engine_env: vec![("AGENTD_TEST_STORAGE_REFUSAL", "1".into())],
        repo_files: vec![
            (".env", "AGENTD_TEST_STORAGE_REFUSAL=1\n"),
            ("AGENTD_TEST_STORAGE_REFUSAL", "1\n"),
            (".ham/test-storage-refusal", "1\n"),
        ],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(storage_notices(&d).is_empty(), "{:?}", storage_notices(&d));
    let lines = storage_lines(&d);
    assert!(
        lines.iter().any(|l| l == "direct turn storage: a tool result named no space left on device; a test write to the session's storage worked"),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("test switch")),
        "{lines:?}"
    );
    assert_eq!(d.summary().label, "Cline reported done");
    assert!(
        !run_folder(&d).iter().any(|n| n.contains("storage-test")),
        "{:?}",
        run_folder(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b881_a_run_without_a_refusal_has_the_summary_it_had() {
    // The run of the golden summary (`writes_files`): the watch makes its test writes at the prompt boundary, says nothing and leaves
    // nothing. The bytes of the summary are held by the golden tests of `direct_golden.rs`, which run with the watch on.
    let d = Direct::start(Opts::default()).await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    assert!(storage_notices(&d).is_empty());
    assert!(storage_lines(&d).is_empty(), "{:?}", storage_lines(&d));
    assert_eq!(d.summary().label, "Cline reported done");
    assert!(
        !run_folder(&d).iter().any(|n| n.contains("storage-test")),
        "{:?}",
        run_folder(&d)
    );
    // The marker of the copies was written through the watch and is the newest copy's, as before the watch.
    assert!(marked_copy(&d) >= 1, "the copy of the end is marked");
}

#[tokio::test(flavor = "multi_thread")]
async fn b881_with_both_disk_checks_off_there_is_no_watch() {
    let d = Direct::start(Opts {
        scenario: "no_space_result",
        edit_cfg: Some(Box::new(|c| {
            c.direct.disk_low_mb = 0;
            c.direct.workspace_limit_mb = 0;
            c.direct.storage_test_refusal = true;
        })),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    assert!(storage_notices(&d).is_empty());
    assert!(storage_lines(&d).is_empty(), "{:?}", storage_lines(&d));
    // The marker is written as before the watch.
    assert!(marked_copy(&d) >= 1, "the copy of the end is marked");
}
