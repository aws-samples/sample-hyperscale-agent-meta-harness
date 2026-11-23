//! In whole runs: a later session whose storage refuses a write before it stored a copy of its own names the copy it started
//! from. The real driver in process, the fake engine, a fake AWS whose bucket keeps versions. No AWS call is made.
//!
//! Session 1 runs to its end and leaves its newest copy in the bucket (`.../turns/1/snapshot.bundle`) with the copy's record beside
//! it. Session 2 restores that object, as the service starts a session from a saved copy, on a storage that refuses the write of
//! the marker of its start (`direct_support::StorageFaults`): the first write a session makes there, before any copy of its own.
//!
//! A session that ended in order hands over its final bundle (`.../turns/1/bundle`), and it saved its copy of the end before that
//! bundle. A session 2 that restores the final bundle reads the record of that saved copy and the copy's first bytes. When both
//! name the commit the session restored, the notice names that saved copy at the version the bucket holds of it; otherwise the
//! notice keeps the sentence of a session without a copy. A session that started from files read back names no copy.
//!
//! Two runs of three sessions hold which session the copy is of: a session 3 that restored the final bundle of session 1 names
//! the saved copy of session 1, and the third session of a chain names the copy of the end of session 2 at the version the
//! bucket holds at the key of session 2 (the stand-in bucket tells the versions of two sessions' copies apart by their ids).
//! One more run holds the record a lost session leaves: session 1 saved two copies and ended between the two uploads of the
//! second, so the record of copy 1 stands beside the bundle of copy 2, and a session that restored that bundle names no copy.
//! The last run does the same with a session that was invoked twice: a copy of its second invocation descends from the copy of
//! the end of its first, and the record of that copy of the end stands beside the later copy's bundle.
//!
//! The service finds the copy a notice names by the notice's id: it builds the key from the session the copy names, reads the
//! version the notice names and requires the commit of the notice in that version's header (`crates/control-api/src/snapshot.rs`,
//! tested there in `b906_before_refusal.rs`). The first test holds each of those three against the stand-in bucket's own versions.
#![allow(clippy::unwrap_used)]

mod b860_thin_support;
mod common;
mod direct_support;

use b860_thin_support::{later_session, record_of, stored_copy, summary_of};
use direct_support::{BUCKET, Direct, Opts, RUN};
use harness_protocol::bundle_header::{HEADER_BYTES, SNAPSHOT_REF, bundle_head};
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The storage notices of the run of `d`, with their event ids.
fn storage_notices(d: &Direct) -> Vec<(String, Value)> {
    d.aws
        .envelopes()
        .into_iter()
        .filter_map(|e: Envelope| match &e.payload {
            Payload::Inline { value }
                if e.event_type == EventType::TurnProgress
                    && value["kind"] == "notify"
                    && value["reason"] == "storage_refused" =>
            {
                Some((e.event_id.clone(), value.clone()))
            }
            _ => None,
        })
        .collect()
}

fn storage_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn storage"))
        .collect()
}

/// The lines of the worker's log that say a later session found the copy it started from (`started_from` in
/// `src/direct/mod.rs`).
fn found_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("this session started from copy"))
        .collect()
}

/// The milliseconds in the one line of the log of `d` that says the session found copy `count` of `session`. `reads` is how
/// the line names what it timed: "the read" for a start from a saved copy (the copy's record), "the two reads" for a start
/// from a final bundle (the record and the first bytes of the saved copy).
fn found_once(d: &Direct, count: &Value, session: u32, reads: &str) -> u64 {
    let found = found_lines(d);
    assert_eq!(
        found.len(),
        1,
        "one line of the log says that the copy was found: {:?}",
        storage_lines(d)
    );
    let start = format!(
        "direct turn storage: this session started from copy {count} of session {session}; {reads} took "
    );
    let ms = found[0]
        .strip_prefix(start.as_str())
        .and_then(|rest| rest.strip_suffix(" ms"))
        .and_then(|n| n.parse::<u64>().ok());
    assert!(
        ms.is_some(),
        "the line names the copy, its session and a time in ms: {:?}; expected {start:?} then a number and ` ms`",
        found[0]
    );
    ms.unwrap()
}

/// `2026-11-21 00:37 UTC` for `2026-11-21T00:37:12Z`.
fn minute(at: &str) -> String {
    format!("{} UTC", at[..16].replacen('T', " ", 1))
}

/// Whether the newest record of a copy of `session` in the bucket is the copy of the end.
fn end_copy_stored(d: &Direct, session: u32) -> bool {
    d.aws
        .get(&keys::snapshot_json_key_t(RUN, session))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["final"] == true)
}

/// What session 1 left for a session that starts from its newest saved copy.
struct First {
    d1: Direct,
    /// The bundle of the newest copy of session 1.
    copy: Vec<u8>,
    /// That copy's own record.
    record: Value,
    /// The commit of the copy and the run's base.
    head: String,
    base: String,
}

/// Runs session 1 to its end.
async fn first_session() -> First {
    let d1 = Direct::start(Opts::default()).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let copy = stored_copy(&d1, 1);
    let record = record_of(&d1, 1);
    let head = record["headSha"].as_str().unwrap().to_string();
    let base = d1.summary().git.base_sha.clone().unwrap();
    assert!(
        record["count"].as_u64().unwrap() >= 1,
        "TRIGGER PRESENT is not shown: session 1 saved no numbered copy: {record}"
    );
    First {
        d1,
        copy,
        record,
        head,
        base,
    }
}

/// Session 2 of the run in a new machine, restoring the newest saved copy of session 1. `with_record` puts that copy's record into
/// the bucket, as session 1 left it.
async fn session_two(first: &First, with_record: bool) -> Direct {
    let d2 = later_session(
        Opts::default(),
        &first.d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        first.copy.clone(),
        &first.head,
        &first.base,
    )
    .await;
    if with_record {
        d2.aws.put(
            &keys::snapshot_json_key_t(RUN, 1),
            serde_json::to_vec(&first.record).unwrap(),
        );
    }
    d2
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_later_session_whose_start_marker_is_refused_names_the_copy_it_restored_at_the_buckets_version()
 {
    let first = first_session().await;
    let d2 = session_two(&first, true).await;
    // The storage of session 2 refuses the first write of the copies' marker, which is the marker of the session's start.
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    let markers = faults.markers.load(Ordering::SeqCst);
    assert!(
        markers >= 1,
        "TRIGGER PRESENT is not shown: the watch made no marker write; log: {:?}",
        d2.log.lines()
    );
    println!("B906 the lines of the watch: {:#?}", storage_lines(&d2));
    // The run is the engine's, and its work is in the bucket.
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );

    let notes = storage_notices(&d2);
    assert!(!notes.is_empty(), "{:?}", storage_lines(&d2));
    let (id, notice) = &notes[0];
    let s = &notice["storage"];
    assert_eq!(s["session"], 2, "{notice}");
    assert_eq!(
        s["seen"], "start",
        "TRIGGER PRESENT: the refusal came before session 2 stored a copy: {notice}"
    );
    println!(
        "B906 TRIGGER PRESENT: the marker of the start of session 2 was refused ({markers} marker writes)"
    );

    // The log says once that the copy was found, with the copy's number and session and the time of the one read.
    found_once(&d2, &first.record["count"], 1, "the read");

    // The copy named is the one session 1 saved and session 2 restored: its session, its number, its time, its commit.
    let copy = &s["copyBefore"];
    assert!(copy.is_object(), "the notice names a copy: {notice}");
    assert_eq!(copy["session"], 1, "{notice}");
    assert_eq!(copy["count"], first.record["count"], "{notice}");
    assert_eq!(copy["at"], first.record["at"], "{notice}");
    assert_eq!(copy["headSha"], first.record["headSha"], "{notice}");

    // What the service does with the notice's id, held against the bucket's own versions: the key is built from the session the
    // copy names; the version the notice names is the one the bucket answered when session 2 read the object; and the bytes of
    // that version are the bundle of the commit the notice names.
    let named_session = u32::try_from(copy["session"].as_u64().unwrap()).unwrap();
    let key = keys::snapshot_bundle_key_t(RUN, named_session);
    let versions = d2.aws.versions_of(&key);
    assert_eq!(versions.len(), 1, "one version of the restored object");
    let (stored_version, body) = &versions[0];
    assert_eq!(copy["versionId"], stored_version.as_str(), "{notice}");
    assert_eq!(
        body, &first.copy,
        "the version holds the copy session 1 saved"
    );
    let header = String::from_utf8_lossy(&body[..body.len().min(4096)]).to_string();
    assert!(
        header
            .lines()
            .any(|l| l.starts_with(first.head.as_str()) && l.contains("refs/")),
        "the header of that version names the commit of the notice: {header:?}"
    );

    // The words: the sentence a session without a copy of its own has, then the copy it started from with the command and this
    // notice's own id, then the command for the newest copy.
    let detail = notice["detail"].as_str().unwrap();
    let expected = format!(
        "This session had saved no copy before ham first saw this. Copy {} of session 1 ({}) is the copy this session started from: ham pull {RUN} --before-refusal {id} downloads it. ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are.",
        first.record["count"],
        minute(first.record["at"].as_str().unwrap()),
    );
    assert!(detail.ends_with(&expected), "{detail}");

    // Every later notice of the session names the same copy, with the id of the first.
    for (_, later) in &notes[1..] {
        assert_eq!(later["storage"]["copyBefore"], *copy, "{later}");
        assert_eq!(
            later["storage"]["first"],
            Value::String(id.clone()),
            "{later}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_bucket_that_answers_no_version_and_a_copy_without_its_record_keep_the_sentence_they_had()
 {
    let first = first_session().await;
    // (1) A bucket that keeps no versions answers none for the read of the copy. (2) The copy's record is not in the bucket (the
    // session that saved it ended between its two uploads), so the copy's time is not known. Either way the notice names no copy,
    // says what a session without a copy always said, and the run and its work are what they were.
    for (case, with_record, versions) in [("no version", true, false), ("no record", false, true)] {
        let d2 = session_two(&first, with_record).await;
        if !versions {
            d2.aws.without_version_ids();
        }
        let faults = d2.refuse_storage(1, 0);
        let (_, resp) = d2.invoke().await;
        assert!(
            faults.markers.load(Ordering::SeqCst) >= 1,
            "{case}: TRIGGER PRESENT is not shown: no marker write; log: {:?}",
            d2.log.lines()
        );
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{case}: {resp:?}");
        assert!(
            end_copy_stored(&d2, 2),
            "{case}: the copy of the end of session 2 is in the bucket"
        );
        assert!(
            found_lines(&d2).is_empty(),
            "{case}: the log says a copy was found: {:?}",
            found_lines(&d2)
        );
        let notes = storage_notices(&d2);
        assert!(!notes.is_empty(), "{case}: {:?}", storage_lines(&d2));
        for (_, notice) in &notes {
            assert_eq!(notice["storage"]["session"], 2, "{case}: {notice}");
            assert!(
                notice["storage"].get("copyBefore").is_none(),
                "{case}: {notice}"
            );
            let detail = notice["detail"].as_str().unwrap();
            assert!(
                detail.contains("This session had saved no copy before ham first saw"),
                "{case}: {detail}"
            );
            assert!(!detail.contains("--before-refusal"), "{case}: {detail}");
            assert!(!detail.contains("started from"), "{case}: {detail}");
            assert!(
                detail.ends_with(&format!(
                    "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
                )),
                "{case}: {detail}"
            );
        }
    }
}

/// What is wrong with the notices of `d` when they must name no copy and keep the sentence of a session without one. Empty when
/// every notice is as it was.
fn naming_a_copy(d: &Direct) -> Vec<String> {
    let mut wrong = Vec::new();
    let notes = storage_notices(d);
    if notes.is_empty() {
        wrong.push(format!("no notice: {:?}", storage_lines(d)));
    }
    if !found_lines(d).is_empty() {
        wrong.push(format!(
            "the log says a copy was found: {:?}",
            found_lines(d)
        ));
    }
    for (_, notice) in &notes {
        if notice["storage"]["session"] != 2 {
            wrong.push(format!("not a notice of session 2: {notice}"));
        }
        if let Some(copy) = notice["storage"].get("copyBefore") {
            wrong.push(format!("a copy is named: {copy}"));
        }
        let detail = notice["detail"].as_str().unwrap_or_default();
        if !detail.contains("This session had saved no copy before ham first saw") {
            wrong.push(format!(
                "the sentence of a session without a copy is missing: {detail}"
            ));
        }
        if detail.contains("--before-refusal") || detail.contains("started from") {
            wrong.push(format!("the words name a copy: {detail}"));
        }
        if !detail.ends_with(&format!(
            "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
        )) {
            wrong.push(format!("the command of the newest copy is missing: {detail}"));
        }
    }
    wrong
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_record_that_names_another_commit_than_the_restored_one_names_no_copy() {
    // A session that ended between the two uploads of a copy left the bundle of its newest copy beside the record of the copy
    // before it. Session 2 restored the bundle; the record names another commit and another number. Its number and its time
    // are not those of the restored copy, so the notice names no copy and keeps the sentence it had.
    let first = first_session().await;
    let d2 = session_two(&first, false).await;
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    let mut before = first.record.clone();
    before["headSha"] = Value::String("1".repeat(40));
    before["count"] = Value::from(first.record["count"].as_u64().unwrap() + 5);
    d2.aws
        .put(&record_key, serde_json::to_vec(&before).unwrap());
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1
            && d2
                .aws
                .gets_of(&record_key)
                .contains(&Some("bytes=0-4095".to_string())),
        "TRIGGER PRESENT is not shown: no marker write, or the record was not asked for; log: {:?}",
        d2.log.lines()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );
    assert!(
        d2.log.lines().iter().any(|l| l
            .contains("the record of session 1 does not name the copy this session started from")),
        "{:?}",
        storage_lines(&d2)
    );
    let wrong = naming_a_copy(&d2);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The bundle of a copy of another commit than any the run made: a header as `git bundle` writes it for the ref of a copy, and
/// a pack that nothing reads.
fn copy_of_another_commit() -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{} {SNAPSHOT_REF}\n\nPACK", "2".repeat(40)).into_bytes();
    b.extend(std::iter::repeat_n(7u8, 4_000));
    b
}

/// The range of the one read the worker makes of a saved copy it did not restore: the first bytes, which hold the header.
fn header_range() -> Option<String> {
    Some(format!("bytes=0-{}", HEADER_BYTES - 1))
}

/// Session 2 of the run in a new machine, restoring the final bundle of session 1, as the service starts a session after one
/// that ended in order. `copy` and `record` are what the bucket holds beside it: the newest saved copy of session 1 and that
/// copy's record. Session 1 saved its copy of the end before it wrote its bundle.
async fn session_two_from_the_final_bundle(
    first: &First,
    copy: Option<Vec<u8>>,
    record: Option<Value>,
) -> Direct {
    let bundle = first
        .d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("session 1 bundled its work");
    let head = first.d1.summary().git.head_sha.clone().unwrap();
    assert_eq!(
        head, first.head,
        "TRIGGER PRESENT is not shown: the final bundle of session 1 is not the commit of its copy of the end"
    );
    assert!(
        end_copy_stored(&first.d1, 1),
        "TRIGGER PRESENT is not shown: the newest record of session 1 is not the copy of its end"
    );
    let d2 = later_session(
        Opts::default(),
        &first.d1,
        &keys::bundle_key_t(RUN, 1),
        bundle,
        &head,
        &first.base,
    )
    .await;
    if let Some(copy) = copy {
        d2.aws.put(&keys::snapshot_bundle_key_t(RUN, 1), copy);
    }
    if let Some(record) = record {
        d2.aws.put(
            &keys::snapshot_json_key_t(RUN, 1),
            serde_json::to_vec(&record).unwrap(),
        );
    }
    d2
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_session_that_started_from_the_final_bundle_names_the_copy_of_the_end_the_session_before_saved()
 {
    // A session that ended in order saved its copy of the end and then wrote its final bundle, and the next session starts from
    // that bundle. The record of the copy of the end names the commit the session restored, so that saved copy is the copy the
    // session started from: the notice names it, at the version the bucket holds of it.
    let first = first_session().await;
    let d2 = session_two_from_the_final_bundle(
        &first,
        Some(first.copy.clone()),
        Some(first.record.clone()),
    )
    .await;
    let copy_key = keys::snapshot_bundle_key_t(RUN, 1);
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    let markers = faults.markers.load(Ordering::SeqCst);
    assert!(
        markers >= 1,
        "TRIGGER PRESENT is not shown: the watch made no marker write; log: {:?}",
        d2.log.lines()
    );
    println!("B906 the lines of the watch: {:#?}", storage_lines(&d2));
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );
    // The session restored the final bundle whole and read the saved copy once, in its first bytes only.
    assert_eq!(
        d2.aws.gets_of(&keys::bundle_key_t(RUN, 1)),
        vec![None],
        "TRIGGER PRESENT is not shown: the session did not restore the final bundle of session 1"
    );
    assert_eq!(d2.aws.gets_of(&copy_key), vec![header_range()]);
    println!(
        "B906 TRIGGER PRESENT: session 2 restored the final bundle of session 1 ({markers} marker writes)"
    );
    // The log says once that the copy was found, with the copy's number and session and the time of the two reads.
    found_once(&d2, &first.record["count"], 1, "the two reads");

    let notes = storage_notices(&d2);
    assert!(!notes.is_empty(), "{:?}", storage_lines(&d2));
    let (id, notice) = &notes[0];
    let s = &notice["storage"];
    assert_eq!(s["session"], 2, "{notice}");
    assert_eq!(s["seen"], "start", "{notice}");
    let copy = &s["copyBefore"];
    assert!(copy.is_object(), "the notice names a copy: {notice}");
    assert_eq!(copy["session"], 1, "{notice}");
    assert_eq!(copy["count"], first.record["count"], "{notice}");
    assert_eq!(copy["at"], first.record["at"], "{notice}");
    assert_eq!(copy["headSha"], first.record["headSha"], "{notice}");

    // What the service does with the notice's id, held against the bucket's own versions: the key is the one of the saved copy
    // of the session the copy names; the version the notice names is the one the bucket holds there; and the bytes of that
    // version are the bundle of the commit the notice names.
    let versions = d2.aws.versions_of(&copy_key);
    assert_eq!(versions.len(), 1, "one version of the saved copy");
    let (stored_version, body) = &versions[0];
    assert_eq!(copy["versionId"], stored_version.as_str(), "{notice}");
    assert_eq!(
        body, &first.copy,
        "the version holds the copy session 1 saved"
    );
    assert_eq!(
        bundle_head(&body[..body.len().min(usize::try_from(HEADER_BYTES).unwrap())]).unwrap(),
        first.head,
        "the header of that version names the commit of the notice"
    );
    // The version of the final bundle is another object's and is never named.
    for (bundle_version, _) in d2.aws.versions_of(&keys::bundle_key_t(RUN, 1)) {
        assert_ne!(copy["versionId"], bundle_version.as_str(), "{notice}");
    }

    let detail = notice["detail"].as_str().unwrap();
    let expected = format!(
        "This session had saved no copy before ham first saw this. Copy {} of session 1 ({}) is the copy this session started from: ham pull {RUN} --before-refusal {id} downloads it. ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are.",
        first.record["count"],
        minute(first.record["at"].as_str().unwrap()),
    );
    assert!(detail.ends_with(&expected), "{detail}");
    for (_, later) in &notes[1..] {
        assert_eq!(later["storage"]["copyBefore"], *copy, "{later}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_final_bundle_without_a_saved_copy_of_its_commit_keeps_the_sentence_it_had() {
    // The session restored the final bundle of session 1, and what the bucket holds beside it does not show a saved copy of
    // that commit. Each case names no copy and says why in the log; the run and its work are what they were.
    let first = first_session().await;
    let mut other_record = first.record.clone();
    other_record["headSha"] = Value::String("1".repeat(40));
    other_record["count"] = Value::from(first.record["count"].as_u64().unwrap() + 5);
    let cases = [
        // The copy of the end was not stored (or the copy of the head failed and the tree was copied): the record names
        // another commit. The saved copy is not read at all.
        (
            "the record names another commit",
            Some(first.copy.clone()),
            Some(other_record),
            true,
            "the record of session 1 does not name the commit this session started on",
            false,
        ),
        (
            "no record",
            Some(first.copy.clone()),
            None,
            true,
            "the record of the newest saved copy of session 1 could not be read",
            false,
        ),
        // The record names the commit, and the object at the key of the saved copy holds another one.
        (
            "the saved copy is of another commit",
            Some(copy_of_another_commit()),
            Some(first.record.clone()),
            true,
            "the newest saved copy of session 1 is not the commit this session started on",
            true,
        ),
        (
            "no version",
            Some(first.copy.clone()),
            Some(first.record.clone()),
            false,
            "the bucket answered no version for the newest saved copy of session 1",
            true,
        ),
    ];
    let mut wrong = Vec::new();
    for (case, copy, record, versions, line, copy_read) in cases {
        let d2 = session_two_from_the_final_bundle(&first, copy, record).await;
        if !versions {
            d2.aws.without_version_ids();
        }
        let faults = d2.refuse_storage(1, 0);
        let (_, resp) = d2.invoke().await;
        assert!(
            faults.markers.load(Ordering::SeqCst) >= 1,
            "{case}: TRIGGER PRESENT is not shown: no marker write; log: {:?}",
            d2.log.lines()
        );
        if resp.outcome != TurnOutcome::Completed {
            wrong.push(format!("{case}: the run did not complete: {resp:?}"));
        }
        if !end_copy_stored(&d2, 2) {
            wrong.push(format!(
                "{case}: the copy of the end of session 2 is not in the bucket"
            ));
        }
        if !d2.log.lines().iter().any(|l| l.contains(line)) {
            wrong.push(format!(
                "{case}: the log lacks {line:?}: {:?}",
                storage_lines(&d2)
            ));
        }
        let asked = d2.aws.gets_of(&keys::snapshot_bundle_key_t(RUN, 1));
        let expected = if copy_read {
            vec![header_range()]
        } else {
            Vec::new()
        };
        if asked != expected {
            wrong.push(format!(
                "{case}: the reads of the saved copy were {asked:?}"
            ));
        }
        wrong.extend(
            naming_a_copy(&d2)
                .into_iter()
                .map(|w| format!("{case}: {w}")),
        );
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_the_two_reads_for_a_final_bundle_share_one_limit_and_the_session_starts() {
    // The bucket answers the read of the record after 2 s and the read of the saved copy's first bytes after 4 s. Each fits the
    // 5 s the start of a session waits; together they do not, and the limit is one for both: the session starts without the
    // copy, and its notice names none.
    let first = first_session().await;
    let d2 = session_two_from_the_final_bundle(
        &first,
        Some(first.copy.clone()),
        Some(first.record.clone()),
    )
    .await;
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    let copy_key = keys::snapshot_bundle_key_t(RUN, 1);
    d2.aws.delay_gets(&record_key, Duration::from_secs(2));
    d2.aws.delay_gets(&copy_key, Duration::from_secs(4));
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1
            && d2.aws.gets_of(&copy_key) == vec![header_range()],
        "TRIGGER PRESENT is not shown: no marker write, or the record did not answer in time for the second read; log: {:?}",
        d2.log.lines()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );
    assert!(
        d2.log.lines().iter().any(|l| l.contains(
            "direct turn storage: the newest saved copy of session 1 got no answer within 5 s"
        )),
        "{:?}",
        storage_lines(&d2)
    );
    let wrong = naming_a_copy(&d2);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_session_that_started_from_files_read_back_names_no_copy() {
    // The service read the files back from the storage of a stopped session and starts the next session from that object. It
    // is no numbered copy of the run: the notice keeps the sentence it had, also when the bucket holds a saved copy of the
    // same commit with its record, and the worker reads neither for a notice.
    let first = first_session().await;
    let d2 = later_session(
        Opts::default(),
        &first.d1,
        &keys::recovery_bundle_key(RUN, 1),
        first.copy.clone(),
        &first.head,
        &first.base,
    )
    .await;
    let copy_key = keys::snapshot_bundle_key_t(RUN, 1);
    d2.aws.put(&copy_key, first.copy.clone());
    d2.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&first.record).unwrap(),
    );
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1
            && d2.aws.gets_of(&keys::recovery_bundle_key(RUN, 1)) == vec![None],
        "TRIGGER PRESENT is not shown: no marker write, or the files read back were not restored; log: {:?}",
        d2.log.lines()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );
    assert_eq!(
        d2.aws.gets_of(&copy_key),
        Vec::<Option<String>>::new(),
        "the saved copy is not read for a start from files read back"
    );
    assert!(
        !d2.aws
            .gets_of(&keys::snapshot_json_key_t(RUN, 1))
            .contains(&Some("bytes=0-4095".to_string())),
        "the record is not read for a notice"
    );
    let wrong = naming_a_copy(&d2);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_record_that_does_not_answer_in_time_names_no_copy_and_the_session_starts() {
    // The bucket answers the read of the copy's record after 20 s. The start of a session waits for that read no longer than the
    // watch waits for an answer of the storage (5 s): the session starts without the record, and its notice names no copy.
    let first = first_session().await;
    let d2 = session_two(&first, true).await;
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    d2.aws.delay_gets(&record_key, Duration::from_secs(20));
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1 && !d2.aws.gets_of(&record_key).is_empty(),
        "TRIGGER PRESENT is not shown: no marker write, or the record was not asked for; log: {:?}",
        d2.log.lines()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );
    assert!(
        d2.log.lines().iter().any(|l| l
            .contains("the record of the copy this session started from got no answer within 5 s")),
        "{:?}",
        storage_lines(&d2)
    );
    let notes = storage_notices(&d2);
    assert!(!notes.is_empty(), "{:?}", storage_lines(&d2));
    let notice = &notes[0].1;
    assert_eq!(notice["storage"]["seen"], "start", "{notice}");
    assert!(notice["storage"].get("copyBefore").is_none(), "{notice}");
    assert!(
        found_lines(&d2).is_empty(),
        "the log says a copy was found: {:?}",
        found_lines(&d2)
    );
    let detail = notice["detail"].as_str().unwrap();
    assert!(
        detail.contains("This session had saved no copy before ham first saw this."),
        "{detail}"
    );
    assert!(!detail.contains("started from"), "{detail}");
}

// ---- the one line of the log of a copy that was found ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b906_a_later_session_that_found_the_copy_it_started_from_says_so_once_in_the_log_with_the_time_of_the_reads()
 {
    // The reads that name the copy a session started from are made at the start of every later session, whether its storage
    // ever refuses a write or not. When they found the copy, the worker's log says so in one line: the copy's number, the
    // session that saved it and the time the reads took. So the log of any later session on a stage shows that the worker's own
    // reads worked and what they cost the start. No session here has a refusal. The stand-in bucket answers each read
    // after a pause, so the time the line names has a lower bound that holds on any machine. The pauses of a start from a
    // final bundle come to 800 ms, and the product waits 5 s at the most for both reads (`STARTED_FROM_RECORD_LIMIT`): the
    // margin of 4.2 s is accepted. A machine slow enough to spend it fails this test at the check for the one line, with the
    // line of no answer in the log. The first part and the first start of the second part name copy 1 of session 1, which a
    // line with a constant number and a constant session would name too. So the second part goes on to a session 3 that
    // starts from the final bundle of session 2: its own line names a copy with another number, saved by another session.
    let first = first_session().await;
    let count = &first.record["count"];
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    let copy_key = keys::snapshot_bundle_key_t(RUN, 1);
    let pause = Duration::from_millis(400);

    // A start from the newest saved copy: one read, of the copy's record.
    let d2 = session_two(&first, true).await;
    d2.aws.delay_gets(&record_key, pause);
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        storage_notices(&d2).is_empty()
            && d2
                .aws
                .gets_of(&record_key)
                .contains(&Some("bytes=0-4095".to_string())),
        "TRIGGER PRESENT is not shown: session 2 had a refusal, or the record was not asked for: {:?}",
        d2.log.lines()
    );
    let took = found_once(&d2, count, 1, "the read");
    println!(
        "B906 TRIGGER PRESENT: session 2 restored the saved copy of session 1 and had no refusal; the line: {:?}",
        found_lines(&d2)
    );
    assert!(
        took >= 400,
        "the time is that of the read of the record, which the bucket answered after 400 ms: {took} ms"
    );

    // A start from the final bundle: two reads, of the record and of the first bytes of the saved copy.
    let d2 = session_two_from_the_final_bundle(
        &first,
        Some(first.copy.clone()),
        Some(first.record.clone()),
    )
    .await;
    d2.aws.delay_gets(&record_key, pause);
    d2.aws.delay_gets(&copy_key, pause);
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        storage_notices(&d2).is_empty() && d2.aws.gets_of(&copy_key) == vec![header_range()],
        "TRIGGER PRESENT is not shown: session 2 had a refusal, or the saved copy was not read once in its first bytes: {:?}",
        d2.log.lines()
    );
    let took = found_once(&d2, count, 1, "the two reads");
    println!(
        "B906 TRIGGER PRESENT: session 2 restored the final bundle of session 1 and had no refusal; the line: {:?}",
        found_lines(&d2)
    );
    assert!(
        took >= 800,
        "the time is that of both reads, each of which the bucket answered after 400 ms: {took} ms"
    );

    // Session 2 ended in order. The copies of a run are numbered within the run, so its copy of the end has a number above 1.
    // Session 3 starts from the final bundle of session 2, and the bucket holds the saved copies of both earlier sessions.
    let record2 = record_of(&d2, 2);
    let copy2 = stored_copy(&d2, 2);
    let head2 = summary_of(&d2, 2).git.head_sha.clone().unwrap();
    let count2 = &record2["count"];
    assert!(
        end_copy_stored(&d2, 2)
            && record2["headSha"].as_str() == Some(head2.as_str())
            && count2.as_u64().is_some_and(|n| n != 1),
        "TRIGGER PRESENT is not shown: the copy of the end of session 2 is not the commit of its final bundle, or it has the \
         number 1, which a line with a constant number names too: {record2}, {head2}"
    );
    let bundle2 = d2
        .aws
        .get(&keys::bundle_key_t(RUN, 2))
        .expect("session 2 bundled its work");
    let record_key2 = keys::snapshot_json_key_t(RUN, 2);
    let copy_key2 = keys::snapshot_bundle_key_t(RUN, 2);
    let d3 = session_three(
        &first.d1,
        &keys::bundle_key_t(RUN, 2),
        bundle2,
        &head2,
        &first.base,
    )
    .await;
    d3.aws.put(&copy_key, first.copy.clone());
    d3.aws
        .put(&record_key, serde_json::to_vec(&first.record).unwrap());
    d3.aws.put(&copy_key2, copy2);
    d3.aws
        .put(&record_key2, serde_json::to_vec(&record2).unwrap());
    d3.aws.delay_gets(&record_key2, pause);
    d3.aws.delay_gets(&copy_key2, pause);
    let (_, resp) = d3.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        storage_notices(&d3).is_empty() && d3.aws.gets_of(&copy_key2) == vec![header_range()],
        "TRIGGER PRESENT is not shown: session 3 had a refusal, or the saved copy of session 2 was not read once in its first \
         bytes: {:?}",
        d3.log.lines()
    );
    let took = found_once(&d3, count2, 2, "the two reads");
    println!(
        "B906 TRIGGER PRESENT: session 3 restored the final bundle of session 2 and had no refusal; the line: {:?}",
        found_lines(&d3)
    );
    assert!(
        took >= 800,
        "the time is that of both reads, each of which the bucket answered after 400 ms: {took} ms"
    );
}

// ---- three sessions, and a record as a lost session leaves it ----------------------------------------------------------------------

/// Session 3 of the run of `first` in a new machine, restoring `object` (stored under `key`), which holds `head`. The bucket holds
/// the bundle the run started from, as it does for every session of a run. `later_session` builds the request of a session 2 and
/// checks it as such, so the request of a session 3 is built and checked here.
async fn session_three(
    first: &Direct,
    key: &str,
    object: Vec<u8>,
    head: &str,
    base: &str,
) -> Direct {
    let mut d3 = Direct::start(Opts::default()).await;
    d3.aws.put(
        &keys::repo_bundle_key(RUN),
        first.aws.get(&keys::repo_bundle_key(RUN)).unwrap(),
    );
    d3.aws.put(key, object);
    d3.req.turn = 3;
    d3.req.runtime_session_id = format!("{RUN}-direct-g00000003");
    d3.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, key),
    };
    d3.req.snapshot_every_s = Some(0);
    d3.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(2),
        brief: None,
    });
    validate_direct_invocation(&d3.req).expect("the invocation of session 3 is valid");
    d3
}

/// The sentence of a notice that names copy `count` of `session`, saved at `at`, for the notice `id`.
fn names_the_copy(count: &Value, session: u32, at: &Value, id: &str) -> String {
    format!(
        "This session had saved no copy before ham first saw this. Copy {count} of session {session} ({}) is the copy this session started from: ham pull {RUN} --before-refusal {id} downloads it. ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are.",
        minute(at.as_str().unwrap()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_session_three_that_started_from_the_final_bundle_of_session_one_names_session_one() {
    // Session 2 of the run left nothing of its own, so the service starts session 3 from the final bundle of session 1. The copy
    // the notice names is the saved copy of session 1, the session whose object session 3 restored, and not of the session
    // before session 3: the record and the first bytes are read at the keys of session 1.
    let first = first_session().await;
    let bundle = first
        .d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("session 1 bundled its work");
    assert_eq!(
        first.d1.summary().git.head_sha.clone().unwrap(),
        first.head,
        "TRIGGER PRESENT is not shown: the final bundle of session 1 is not the commit of its copy of the end"
    );
    assert!(
        end_copy_stored(&first.d1, 1),
        "TRIGGER PRESENT is not shown: the newest record of session 1 is not the copy of its end"
    );
    let d3 = session_three(
        &first.d1,
        &keys::bundle_key_t(RUN, 1),
        bundle,
        &first.head,
        &first.base,
    )
    .await;
    let copy_key = keys::snapshot_bundle_key_t(RUN, 1);
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    d3.aws.put(&copy_key, first.copy.clone());
    d3.aws
        .put(&record_key, serde_json::to_vec(&first.record).unwrap());
    let faults = d3.refuse_storage(1, 0);
    let (_, resp) = d3.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1,
        "TRIGGER PRESENT is not shown: the watch made no marker write; log: {:?}",
        d3.log.lines()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d3, 3),
        "the copy of the end of session 3 is in the bucket"
    );
    assert_eq!(
        d3.aws.gets_of(&keys::bundle_key_t(RUN, 1)),
        vec![None],
        "TRIGGER PRESENT is not shown: session 3 did not restore the final bundle of session 1"
    );
    assert!(
        d3.aws
            .versions_of(&keys::snapshot_bundle_key_t(RUN, 2))
            .is_empty()
            && d3.aws.get(&keys::snapshot_json_key_t(RUN, 2)).is_none(),
        "TRIGGER PRESENT is not shown: session 2 left a copy"
    );
    println!(
        "B906 TRIGGER PRESENT: session 3 restored the final bundle of session 1; session 2 left no copy; the lines of the watch: {:#?}",
        storage_lines(&d3)
    );

    let notes = storage_notices(&d3);
    assert!(!notes.is_empty(), "{:?}", storage_lines(&d3));
    let (id, notice) = &notes[0];
    let s = &notice["storage"];
    assert_eq!(s["session"], 3, "{notice}");
    let copy = &s["copyBefore"];
    assert!(copy.is_object(), "the notice names a copy: {notice}");
    assert_eq!(copy["session"], 1, "{notice}");
    assert_eq!(copy["count"], first.record["count"], "{notice}");
    assert_eq!(copy["at"], first.record["at"], "{notice}");
    assert_eq!(copy["headSha"], first.record["headSha"], "{notice}");
    // The record was read once, in its first bytes, at the key of session 1, and the saved copy once, in its header.
    assert!(
        d3.aws
            .gets_of(&record_key)
            .contains(&Some("bytes=0-4095".to_string())),
        "{:?}",
        d3.aws.gets_of(&record_key)
    );
    assert_eq!(d3.aws.gets_of(&copy_key), vec![header_range()]);
    let versions = d3.aws.versions_of(&copy_key);
    assert_eq!(versions.len(), 1, "one version of the saved copy");
    assert_eq!(copy["versionId"], versions[0].0.as_str(), "{notice}");
    let detail = notice["detail"].as_str().unwrap();
    let expected = names_the_copy(&first.record["count"], 1, &first.record["at"], id);
    assert!(detail.ends_with(&expected), "{detail}");
    // The log of session 3 says once that the copy was found, and names the session that saved it.
    found_once(&d3, &first.record["count"], 1, "the two reads");
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_three_sessions_in_a_chain_name_the_copy_of_the_end_of_session_two_at_its_own_version()
{
    // Session 1 ends in order. Session 2 starts from its final bundle, has no refusal and ends in order. Session 3 starts from
    // the final bundle of session 2, and its storage refuses the marker of its start. The bucket holds the saved copies of both
    // earlier sessions: the notice names the copy of the end of session 2, with that copy's number and time, at the version the
    // bucket holds at the key of session 2.
    let first = first_session().await;
    let d2 = session_two_from_the_final_bundle(
        &first,
        Some(first.copy.clone()),
        Some(first.record.clone()),
    )
    .await;
    let (_, resp2) = d2.invoke().await;
    assert_eq!(resp2.outcome, TurnOutcome::Completed, "{resp2:?}");
    assert!(
        storage_notices(&d2).is_empty(),
        "TRIGGER PRESENT is not shown: session 2 had a refusal: {:?}",
        storage_lines(&d2)
    );
    // Session 2 had no refusal, and its log says once that the copy it started from was found: the reads are made at the
    // start of every later session.
    found_once(&d2, &first.record["count"], 1, "the two reads");
    let record2 = record_of(&d2, 2);
    let copy2 = stored_copy(&d2, 2);
    let head2 = summary_of(&d2, 2).git.head_sha.clone().unwrap();
    assert!(
        end_copy_stored(&d2, 2) && record2["headSha"].as_str() == Some(head2.as_str()),
        "TRIGGER PRESENT is not shown: the copy of the end of session 2 is not the commit of its final bundle: {record2}, {head2}"
    );
    assert!(
        record2["count"].as_u64().unwrap() > first.record["count"].as_u64().unwrap(),
        "TRIGGER PRESENT is not shown: the copies of a run are numbered within the run: {} then {record2}",
        first.record
    );
    let bundle2 = d2
        .aws
        .get(&keys::bundle_key_t(RUN, 2))
        .expect("session 2 bundled its work");

    let d3 = session_three(
        &first.d1,
        &keys::bundle_key_t(RUN, 2),
        bundle2,
        &head2,
        &first.base,
    )
    .await;
    let key_one = keys::snapshot_bundle_key_t(RUN, 1);
    let key_two = keys::snapshot_bundle_key_t(RUN, 2);
    d3.aws.put(&key_one, first.copy.clone());
    d3.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&first.record).unwrap(),
    );
    d3.aws.put(&key_two, copy2.clone());
    d3.aws.put(
        &keys::snapshot_json_key_t(RUN, 2),
        serde_json::to_vec(&record2).unwrap(),
    );
    let faults = d3.refuse_storage(1, 0);
    let (_, resp3) = d3.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1,
        "TRIGGER PRESENT is not shown: the watch made no marker write; log: {:?}",
        d3.log.lines()
    );
    assert_eq!(resp3.outcome, TurnOutcome::Completed, "{resp3:?}");
    assert_eq!(
        d3.aws.gets_of(&keys::bundle_key_t(RUN, 2)),
        vec![None],
        "TRIGGER PRESENT is not shown: session 3 did not restore the final bundle of session 2"
    );
    println!(
        "B906 TRIGGER PRESENT: a chain of three sessions; the head of session 1 is {}, the head of session 2 is {head2}; record of session 2: {record2}; the lines of the watch: {:#?}",
        first.head,
        storage_lines(&d3)
    );

    let notes = storage_notices(&d3);
    assert!(!notes.is_empty(), "{:?}", storage_lines(&d3));
    let (id, notice) = &notes[0];
    let s = &notice["storage"];
    assert_eq!(s["session"], 3, "{notice}");
    let copy = &s["copyBefore"];
    assert!(copy.is_object(), "the notice names a copy: {notice}");
    assert_eq!(copy["session"], 2, "{notice}");
    assert_eq!(copy["count"], record2["count"], "{notice}");
    assert_eq!(copy["at"], record2["at"], "{notice}");
    assert_eq!(copy["headSha"], head2.as_str(), "{notice}");
    // The saved copy of session 2 was read once, in its header; the saved copy of session 1 was not read.
    assert_eq!(d3.aws.gets_of(&key_two), vec![header_range()]);
    assert!(
        d3.aws.gets_of(&key_one).is_empty(),
        "{:?}",
        d3.aws.gets_of(&key_one)
    );
    // The version is the one the bucket holds at the key of session 2. The bucket tells the versions of the two sessions' copies
    // apart by their ids, as a real bucket does, so the id alone says which object the notice names.
    let of_one = d3.aws.versions_of(&key_one);
    let of_two = d3.aws.versions_of(&key_two);
    assert_eq!((of_one.len(), of_two.len()), (1, 1));
    assert_ne!(
        of_one[0].0, of_two[0].0,
        "TRIGGER PRESENT is not shown: the stand-in bucket gives the saved copies of two sessions one version id"
    );
    assert_eq!(copy["versionId"], of_two[0].0.as_str(), "{notice}");
    assert_eq!(
        of_two[0].1, copy2,
        "the version holds the copy session 2 saved"
    );
    assert_eq!(
        bundle_head(&copy2[..copy2.len().min(usize::try_from(HEADER_BYTES).unwrap())]).unwrap(),
        head2,
        "the header of that version names the commit of the notice"
    );
    let detail = notice["detail"].as_str().unwrap();
    let expected = names_the_copy(&record2["count"], 2, &record2["at"], id);
    assert!(detail.ends_with(&expected), "{detail}");
    // The log of session 3 says once that the copy was found, and names session 2.
    found_once(&d3, &record2["count"], 2, "the two reads");
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
async fn until(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + limit;
    loop {
        if seen() {
            return true;
        }
        if std::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// What the file of `FAKE_ACP_STEP_FILE` holds while the engine may write its two edits and nothing after them (the first four
/// messages of its reply: a tool call and its update for each edit), and once it may go on to its end.
const TWO_EDITS: &str = "4\n";
const GO_ON: &str = "1000000\n";

/// The options of a session whose engine is stepped through the file `steps` and whose copies follow a change of the work tree
/// within a third of a second, so that a test decides what the session has saved when its engine goes on.
fn stepped(steps: &std::path::Path) -> Opts {
    Opts {
        engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
        })),
        ..Opts::default()
    }
}

/// Every version of the record of the copies of session 1 in the bucket of `d`, oldest first.
fn records_of_session_one(d: &Direct) -> Vec<Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key_t(RUN, 1))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_the_record_of_the_copy_before_beside_the_bundle_of_the_copy_a_session_restored_names_no_copy()
 {
    // A session uploads the bundle of a copy and then that copy's record. Session 1 saved copy 1 and copy 2 while its engine
    // worked and was lost between the two uploads of copy 2: the bucket holds the bundle of copy 2 beside the record of copy 1,
    // each as session 1 wrote it. Session 2 restored the bundle. The record names a copy of the run with a smaller number and
    // another commit, so its number and its time are not those of the restored copy: the notice names no copy.
    let steps = common::scratch("b906-two-copies").join("engine-steps");
    std::fs::write(&steps, TWO_EDITS).unwrap();
    let d1 = Direct::start(stepped(&steps)).await;
    // The engine makes its two edits and is held. Copy 1 follows them; the test then changes the work tree, copy 2 follows, and
    // only then the engine goes on to its end. Nothing here waits for a time of the engine's.
    let two_copies = async {
        let one = until(Duration::from_secs(90), || {
            !records_of_session_one(&d1).is_empty()
        })
        .await;
        if one {
            std::fs::write(
                d1.worktree().join("late.txt"),
                "written after the first copy\n",
            )
            .unwrap();
        }
        let two = until(Duration::from_secs(90), || {
            records_of_session_one(&d1).len() >= 2
        })
        .await;
        // The engine goes on either way, so that a run that saved no second copy still ends and the assertion below can say so.
        std::fs::write(&steps, GO_ON).unwrap();
        one && two
    };
    let ((_, resp), followed) = tokio::join!(d1.invoke(), two_copies);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        followed,
        "TRIGGER PRESENT is not shown: no second copy followed the first"
    );
    let records = records_of_session_one(&d1);
    let bundles = d1.aws.versions_of(&keys::snapshot_bundle_key_t(RUN, 1));
    let header =
        |b: &[u8]| bundle_head(&b[..b.len().min(usize::try_from(HEADER_BYTES).unwrap())]).unwrap();
    let (one, two) = (&records[0], &records[1]);
    assert!(
        bundles.len() >= 2
            && one.get("final").is_none()
            && two.get("final").is_none()
            && one["count"] == 1
            && two["count"] == 2
            && one["headSha"] != two["headSha"]
            && header(&bundles[0].1) == one["headSha"].as_str().unwrap()
            && header(&bundles[1].1) == two["headSha"].as_str().unwrap(),
        "TRIGGER PRESENT is not shown: session 1 did not save copy 1 and copy 2 while its engine worked, each bundle with its own \
         record: {records:?}"
    );
    let base = d1.summary().git.base_sha.clone().unwrap();
    let head = two["headSha"].as_str().unwrap().to_string();
    println!(
        "B906 TRIGGER PRESENT: session 1 saved copy 1 ({}, {}) and copy 2 ({head}, {}) while its engine worked; session 2 restores \
         the bundle of copy 2 beside the record of copy 1",
        one["headSha"], one["at"], two["at"]
    );

    let d2 = later_session(
        Opts::default(),
        &d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        bundles[1].1.clone(),
        &head,
        &base,
    )
    .await;
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    // The record of copy 1 as session 1 stored it: the bytes of the first version of the record in the bucket of session 1.
    let stored = d1.aws.versions_of(&record_key)[0].1.clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&stored).ok().as_ref(),
        Some(one),
        "TRIGGER PRESENT is not shown: the stored bytes put beside the bundle do not parse to the record of copy 1"
    );
    d2.aws.put(&record_key, stored.clone());
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1
            && d2
                .aws
                .gets_of(&record_key)
                .contains(&Some("bytes=0-4095".to_string())),
        "TRIGGER PRESENT is not shown: no marker write, or the record was not asked for; log: {:?}",
        d2.log.lines()
    );
    // What session 2 read at the key of session 1 is one version, byte for byte the record session 1 stored.
    let beside = d2.aws.versions_of(&record_key);
    assert!(
        beside.len() == 1 && beside[0].1 == stored,
        "TRIGGER PRESENT is not shown: the record beside the bundle is not, byte for byte, the one version session 1 stored: {:?}",
        beside
            .iter()
            .map(|(id, body)| (id.as_str(), String::from_utf8_lossy(body).to_string()))
            .collect::<Vec<_>>()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        end_copy_stored(&d2, 2),
        "the copy of the end of session 2 is in the bucket"
    );
    assert!(
        d2.log.lines().iter().any(|l| l
            .contains("the record of session 1 does not name the copy this session started from")),
        "{:?}",
        storage_lines(&d2)
    );
    let wrong = naming_a_copy(&d2);
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b906_the_record_of_a_copy_the_restored_copy_descends_from_names_no_copy() {
    // A session that is invoked again works on in the checkout of its first invocation, so a copy of its second invocation
    // descends from the copy of the end of the first. Session 1 ended its first invocation in order and was lost in its second
    // between the two uploads of a copy: the bucket holds the bundle of that copy beside the record of the copy of the end, each
    // as session 1 wrote it. Session 2 restored the bundle. The record names a commit the restored commit descends from, and it
    // is not the restored copy's record: the notice names no copy. A session that is invoked a second time numbers its copies
    // from 1 again, so the two records carry one number and differ in the commit only: the record of the copy of the end, which
    // the bucket holds, and the record the restored copy would have had. The commit alone tells them apart here; a comparison
    // of the numbers would not.
    let steps = common::scratch("b906-invoked-twice").join("engine-steps");
    std::fs::write(&steps, GO_ON).unwrap();
    let d1 = Direct::start(stepped(&steps)).await;
    let deps = agentd::direct::Deps::default();
    let first = agentd::direct::run_with(&d1.app, d1.req.clone(), &deps).await;
    assert_eq!(first.outcome, TurnOutcome::Completed, "{first:?}");
    let end = record_of(&d1, 1);
    let on_record = records_of_session_one(&d1).len();
    // The record of the copy of the end as session 1 stored it: the bytes of the version that is the newest after the first
    // invocation.
    let end_stored = d1.aws.versions_of(&keys::snapshot_json_key_t(RUN, 1))[on_record - 1]
        .1
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&end_stored).ok().as_ref(),
        Some(&end),
        "TRIGGER PRESENT is not shown: the stored bytes put beside the bundle do not parse to the record of the copy of the end"
    );
    let engine_said = d1.fake_engine_log().len();
    // The second invocation: its engine makes its two edits, which change nothing, and is held. The worker starts an engine
    // once the checkout is ready, so when the engine's log has moved the test changes the work tree; a copy follows, and only
    // then the engine goes on. Nothing here waits for a time of the engine's.
    std::fs::write(&steps, TWO_EDITS).unwrap();
    let later_copy = async {
        let works = until(Duration::from_secs(90), || {
            d1.fake_engine_log().len() != engine_said
        })
        .await;
        if works {
            std::fs::write(
                d1.worktree().join("late.txt"),
                "written in the second invocation\n",
            )
            .unwrap();
        }
        let copied = until(Duration::from_secs(90), || {
            records_of_session_one(&d1).len() > on_record
        })
        .await;
        let later = records_of_session_one(&d1).get(on_record).cloned();
        // The engine goes on either way, so that an invocation that saved no copy still ends and the assertion below can say so.
        std::fs::write(&steps, GO_ON).unwrap();
        later.filter(|_| works && copied)
    };
    let (second, later) = tokio::join!(
        agentd::direct::run_with(&d1.app, d1.req.clone(), &deps),
        later_copy
    );
    // A second invocation of a session ends with an error of its own, because the session's summary is in the bucket already;
    // what counts here is that its engine worked and that it saved a copy meanwhile.
    assert_eq!(
        second.stop_reason.as_deref(),
        Some("end_turn"),
        "TRIGGER PRESENT is not shown: the second invocation did not run its engine: {second:?}"
    );
    let later = later
        .expect("TRIGGER PRESENT is not shown: the second invocation saved no copy of its own");
    let head = later["headSha"].as_str().unwrap().to_string();
    let header =
        |b: &[u8]| bundle_head(&b[..b.len().min(usize::try_from(HEADER_BYTES).unwrap())]).unwrap();
    let bundle = d1
        .aws
        .versions_of(&keys::snapshot_bundle_key_t(RUN, 1))
        .into_iter()
        .map(|(_, body)| body)
        .find(|body| header(body) == head);
    assert!(
        end["final"] == true
            && later.get("final").is_none()
            && later["headSha"] != end["headSha"]
            && bundle.is_some(),
        "TRIGGER PRESENT is not shown: the first invocation left the record of its copy of the end, and the second saved a \
         copy of another commit while its engine worked: {end}, then {later}"
    );
    let base = d1.summary().git.base_sha.clone().unwrap();

    let d2 = later_session(
        Opts::default(),
        &d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        bundle.unwrap(),
        &head,
        &base,
    )
    .await;
    let record_key = keys::snapshot_json_key_t(RUN, 1);
    d2.aws.put(&record_key, end_stored.clone());
    let faults = d2.refuse_storage(1, 0);
    let (_, resp) = d2.invoke().await;
    assert!(
        faults.markers.load(Ordering::SeqCst) >= 1
            && d2
                .aws
                .gets_of(&record_key)
                .contains(&Some("bytes=0-4095".to_string())),
        "TRIGGER PRESENT is not shown: no marker write, or the record was not asked for; log: {:?}",
        d2.log.lines()
    );
    // What session 2 read at the key of session 1 is one version, byte for byte the record session 1 stored.
    let beside = d2.aws.versions_of(&record_key);
    assert!(
        beside.len() == 1 && beside[0].1 == end_stored,
        "TRIGGER PRESENT is not shown: the record beside the bundle is not, byte for byte, the one version session 1 stored: {:?}",
        beside
            .iter()
            .map(|(id, body)| (id.as_str(), String::from_utf8_lossy(body).to_string()))
            .collect::<Vec<_>>()
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The commit the record names is in the history of the commit session 2 restored.
    let descends = std::process::Command::new("git")
        .arg("-C")
        .arg(d2.worktree())
        .args(["merge-base", "--is-ancestor"])
        .arg(end["headSha"].as_str().unwrap())
        .arg(&head)
        .status()
        .unwrap();
    assert!(
        descends.success(),
        "TRIGGER PRESENT is not shown: the restored commit {head} does not descend from the commit of the record {end}"
    );
    println!(
        "B906 TRIGGER PRESENT: the restored copy {later} descends from the copy of the end the record names: {end}; their numbers are {} and {}",
        later["count"], end["count"]
    );
    assert!(
        d2.log.lines().iter().any(|l| l
            .contains("the record of session 1 does not name the copy this session started from")),
        "{:?}",
        storage_lines(&d2)
    );
    let wrong = naming_a_copy(&d2);
    assert!(wrong.is_empty(), "{wrong:#?}");
}
