//! The storage watch in whole runs whose storage refuses the worker's own writes: the real driver in process, the fake engine,
//! a fake AWS. No AWS call is made.
//!
//! The whole runs (`b881_whole_run.rs`) use the switch of the live test row, which answers one test write of the watch. Here
//! the two writes the watch reads (its test write and the write of the copies' marker) are the fault switch of the test support
//! (`direct_support::StorageFaults`, given to the run through `App::storage_writes`): the host's writes, except that it answers as
//! many of each kind as the test says with errno 28 without touching the storage. The last test also refuses the write of the git
//! settings at the session's end (`App::settings_write`, as `b890_whole_run.rs` does), so that no write of the worker is taken.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::checked_write::FileCalls;
use agentd::direct::git_settings::Repeat;
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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

/// The names in the run's own folder on the session's storage.
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

#[tokio::test(flavor = "multi_thread")]
async fn b884_a_storage_that_refuses_the_test_write_and_the_marker_write_keeps_the_run_and_its_copies()
 {
    // The run of the golden summary (`writes_files`) on a storage that refuses every test write of the watch and every write of the
    // copies' marker, from the marker of the session's start on.
    let d = Direct::start(Opts::default()).await;
    let faults = d.refuse_storage(u32::MAX, u32::MAX);
    let (_, resp) = d.invoke().await;
    let (tests, markers) = (
        faults.tests.load(Ordering::SeqCst),
        faults.markers.load(Ordering::SeqCst),
    );
    assert!(
        tests >= 1 && markers >= 2,
        "TRIGGER PRESENT is not shown: the watch made {tests} test writes and {markers} marker writes; log: {:?}",
        d.log.lines()
    );
    println!(
        "B884 TRIGGER PRESENT: the storage refused {tests} test writes and {markers} writes of the copies' marker"
    );

    println!("B884 the lines of the watch: {:#?}", storage_lines(&d));
    // The run is the engine's: it ends as the run without a refusal ends, and its work is in the bucket.
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().label, "Cline reported done");
    assert!(end_copy_stored(&d), "the copy of the end is in the bucket");

    // The first refused write is the marker of the session's start: it opens the session's first stretch, and the notice is the
    // full one. No marker write was taken, so no notice names a copy, and every notice names the command for the newest copy.
    let notes = storage_notices(&d);
    // Two notices and no other number: the full one of the stretch the refused marker of the start opened, and the short one of
    // the stretch a refused marker of a copy opened after the storage took the write of the git settings (see below). The number
    // is held first: with one notice the assertions on the later notice would check nothing.
    assert_eq!(
        notes.len(),
        2,
        "{:?}; the lines of the watch: {:?}",
        d.events_of(EventType::TurnProgress),
        storage_lines(&d)
    );
    assert_eq!(notes[0]["storage"]["session"], 1);
    assert_eq!(notes[0]["storage"]["stretch"], 1);
    assert!(notes[0]["storage"].get("first").is_none(), "{}", notes[0]);
    assert_eq!(notes[0]["storage"]["seen"], "start");
    assert!(
        notes[0]["detail"]
            .as_str()
            .unwrap()
            .starts_with("The session's storage refused a write while its files add up to "),
        "{}",
        notes[0]
    );
    for (i, n) in notes.iter().enumerate() {
        assert_eq!(n["storage"]["stretch"], i + 1, "{n}");
        assert!(n["storage"].get("copyBefore").is_none(), "{n}");
        assert!(n["storage"].get("testSwitch").is_none(), "{n}");
        let detail = n["detail"].as_str().unwrap();
        assert!(
            detail.contains("This session had saved no copy before ham first saw"),
            "{detail}"
        );
        assert!(
            detail.ends_with(&format!(
                "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
            )),
            "{detail}"
        );
    }
    // One notice per stretch: a refusal inside an open stretch says nothing, so the refused test write of the prompt boundary
    // brings none. The stretch of the start closes when the storage takes a write the worker makes anyway and the fault switch
    // does not stand in for (the git settings at the session's end); the refused marker of the copy of the end then opens the
    // next one, whose notice is the short one.
    let closed = storage_lines(&d)
        .iter()
        .filter(|l| l.contains("takes writes again"))
        .count();
    assert_eq!(
        closed,
        1,
        "{} notices and {closed} closed stretches: {:?}",
        notes.len(),
        storage_lines(&d)
    );
    assert!(
        u32::try_from(notes.len()).unwrap() < tests + markers,
        "{} notices for {tests} test writes and {markers} marker writes",
        notes.len()
    );
    assert_eq!(notes[1]["storage"]["stretch"], 2, "{}", notes[1]);
    for n in notes.iter().skip(1) {
        assert!(
            n["detail"]
                .as_str()
                .unwrap()
                .starts_with("The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session."),
            "{n}"
        );
        // A later notice carries the id of the session's first one; the first carries none.
        assert!(n["storage"]["first"].is_string(), "{n}");
        // It was a copy's marker that met the refusal: the word of the start is the start marker's alone.
        assert_eq!(n["storage"]["seen"], "copy", "{n}");
    }

    // Nothing of the watch reached the session's storage: no marker of the copies and no test file.
    let folder = run_folder(&d);
    assert!(
        !folder
            .iter()
            .any(|n| n == "copies.json" || n.contains("storage-test")),
        "{folder:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b884_a_refused_start_marker_brings_one_notice_with_the_word_start() {
    // The storage refuses the first write of the copies' marker, which is the marker of the session's start, and takes every write
    // after it.
    let d = Direct::start(Opts::default()).await;
    let faults = d.refuse_storage(1, 0);
    let (_, resp) = d.invoke().await;
    let start_line = d
        .log
        .lines()
        .into_iter()
        .find(|l| l.starts_with("direct turn start marker: "));
    assert!(
        start_line.is_some(),
        "TRIGGER PRESENT is not shown: the driver logged no start marker that failed; log: {:?}",
        d.log.lines()
    );
    println!("B884 TRIGGER PRESENT: {}", start_line.unwrap());
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");

    // One notice, the full one. Its `seen` member names the marker of the start, and it names no copy.
    let notes = storage_notices(&d);
    assert_eq!(notes.len(), 1, "{:?}", d.events_of(EventType::TurnProgress));
    let s = &notes[0]["storage"];
    assert_eq!(s["seen"], "start");
    assert_eq!(s["session"], 1);
    assert_eq!(s["stretch"], 1);
    assert!(s.get("copyBefore").is_none(), "{}", notes[0]);
    assert!(
        notes[0]["detail"]
            .as_str()
            .unwrap()
            .contains("This session had saved no copy before ham first saw this."),
        "{}",
        notes[0]
    );
    let lines = storage_lines(&d);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("refused a write (seen at the marker of the session's start)")),
        "{lines:?}"
    );

    // The storage took the writes after it: the stretch closed, the copies were marked, and the run is the engine's.
    assert!(
        lines.iter().any(|l| l.contains("takes writes again")),
        "{lines:?}"
    );
    assert!(faults.markers.load(Ordering::SeqCst) >= 2);
    assert!(marked_copy(&d) >= 1, "the copy of the end is marked");
    assert_eq!(d.summary().label, "Cline reported done");
    assert!(end_copy_stored(&d), "the copy of the end is in the bucket");
}

// ---- a storage that refuses every write of the worker -----------------------------------------------------------------------------

/// The session's storage as it was seen while it refuses data, for the files of the git settings: the write call works, no byte
/// arrives and the sync answers errno 28 (the stand-in of `b890_whole_run.rs`).
#[derive(Debug, Default)]
struct RefusesEvery {
    files: AtomicU32,
}

impl FileCalls for RefusesEvery {
    fn write(&self, _file: &mut std::fs::File, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }

    fn sync(&self, _file: &std::fs::File) -> std::io::Result<()> {
        self.files.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::from_raw_os_error(28))
    }
}

/// A pause of one second and a bound of two, so that the test waits two seconds where a session waits minutes.
const SHORT: Repeat = Repeat {
    every: Duration::from_secs(1),
    for_at_most: Duration::from_secs(2),
};

#[tokio::test(flavor = "multi_thread")]
async fn b884_a_storage_that_refuses_every_write_of_the_worker_brings_one_notice() {
    // The first test's storage takes the write of the git settings at the session's end, which closes the first stretch and lets a
    // second notice come. Here the storage refuses that write too, to the bound of its repeat: every test write of the watch, every
    // write of the copies' marker and every write of the git settings is answered with errno 28.
    let d = Direct::start(Opts::default()).await;
    let faults = d.refuse_storage(u32::MAX, u32::MAX);
    let settings = Arc::new(RefusesEvery::default());
    assert!(d.app.settings_write.set((settings.clone(), SHORT)).is_ok());
    let (_, resp) = d.invoke().await;
    let (tests, markers, files) = (
        faults.tests.load(Ordering::SeqCst),
        faults.markers.load(Ordering::SeqCst),
        settings.files.load(Ordering::SeqCst),
    );
    assert!(
        tests >= 1 && markers >= 2 && files >= 2,
        "TRIGGER PRESENT is not shown: {tests} test writes, {markers} marker writes and {files} writes of the git settings; log: {:?}",
        d.log.lines()
    );
    println!(
        "B884 TRIGGER PRESENT: the storage refused {tests} test writes, {markers} writes of the copies' marker and {files} writes of the git settings"
    );

    // No write of the worker was taken, so the stretch that the refused marker of the start opened never closed: the session has
    // its one notice, the full one, and no short one after it.
    let notes = storage_notices(&d);
    println!(
        "B884 COUNT: a storage that refuses every write of the worker brought {} notice(s) in one session; the lines of the watch: {:#?}",
        notes.len(),
        storage_lines(&d)
    );
    assert_eq!(notes.len(), 1, "{:?}", d.events_of(EventType::TurnProgress));
    let s = &notes[0]["storage"];
    assert_eq!(s["stretch"], 1);
    assert_eq!(s["seen"], "start");
    assert!(s.get("first").is_none(), "{}", notes[0]);
    assert!(s.get("copyBefore").is_none(), "{}", notes[0]);
    assert!(
        notes[0]["detail"]
            .as_str()
            .unwrap()
            .starts_with("The session's storage refused a write while its files add up to "),
        "{}",
        notes[0]
    );
    let closed = storage_lines(&d)
        .iter()
        .filter(|l| l.contains("takes writes again"))
        .count();
    assert_eq!(closed, 0, "{:?}", storage_lines(&d));

    // The run ends with the storage's words about the git settings, and the end state of the work has its copy in the
    // bucket (root invariant 9).
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    assert!(
        d.summary().label.starts_with(
            "Cline stopped: the harness ended the run (the session's storage refused the write of .git/config for "
        ),
        "{}",
        d.summary().label
    );
    assert!(end_copy_stored(&d), "the copy of the end is in the bucket");
    // Nothing of the watch reached the session's storage.
    let folder = run_folder(&d);
    assert!(
        !folder
            .iter()
            .any(|n| n == "copies.json" || n.contains("storage-test")),
        "{folder:?}"
    );
}
