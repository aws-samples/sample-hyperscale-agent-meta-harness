//! The copy of the end writes the worker's save line, as every look that stored a copy or failed does
//! (`direct turn copy look n=<n> stored by=final ...`, or `failed why=<step>` in place of `stored`). Release 5.28's changelog says that
//! the worker logs one line for each save that stored a copy or failed; before this item the copy of the end, the last save of every
//! session, wrote none when it was stored, and only `direct turn final copy failed: <error>` when it failed.
//!
//! `by=final` tells it apart from the looks of the copy task (`timer`, `tool`, `retry`, `stop`): it is the save whose record says
//! `"final": true`. It reads no head of the engine, so its `engine` is `unread`, and it hashes its bundle in its store, so its `hash`
//! is 0. The turns here run in process, with the stand-in engine and the stand-in bucket; the copies while the engine works are off,
//! so every save of a turn is a copy of the end.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use harness_protocol::runtime::TurnOutcome;
use std::time::Duration;

/// The worker's save lines: one per look or copy of the end that stored a copy or failed.
fn save_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn copy look n="))
        .collect()
}

/// The value of `name=` in a save line.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(name)?.strip_prefix('='))
}

/// Every field of the save line is in `line` once, as in the line of a look.
fn has_every_field_once(line: &str) {
    for name in [
        "n", "by", "engine", "total", "tidy", "tree", "head", "commit", "bundle", "hash", "store",
        "keep", "bytes", "kept",
    ] {
        let times = line
            .split_whitespace()
            .filter(|w| {
                w.strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with('='))
            })
            .count();
        assert_eq!(times, 1, "the field {name} of the line: {line}");
    }
    for name in [
        "total", "tidy", "tree", "head", "commit", "bundle", "hash", "store", "bytes", "kept",
    ] {
        assert!(
            field(line, name).unwrap().parse::<u64>().is_ok(),
            "{name} is a number: {line}"
        );
    }
}

/// The lines of the copies of the end.
fn end_lines(d: &Direct) -> Vec<String> {
    save_lines(d)
        .into_iter()
        .filter(|l| field(l, "by") == Some("final"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn b930_a_stored_copy_of_the_end_writes_its_save_line() {
    let d = Direct::start(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let record: serde_json::Value =
        serde_json::from_slice(&d.aws.get(&keys::snapshot_json_key(RUN)).unwrap()).unwrap();
    assert_eq!(
        record["final"], true,
        "TRIGGER PRESENT is not shown: the copy of the end was not stored: {record}"
    );
    let bundle = d.aws.get(&keys::snapshot_bundle_key(RUN)).unwrap();

    let lines = end_lines(&d);
    assert_eq!(
        lines.len(),
        1,
        "TRIGGER PRESENT: the copy of the end, stored as copy {}, has no save line of its own; save lines:\n{}",
        record["count"],
        save_lines(&d).join("\n")
    );
    let line = &lines[0];
    has_every_field_once(line);
    assert!(line.contains(" stored by=final "), "{line}");
    assert_eq!(
        field(line, "n"),
        Some(record["count"].to_string().as_str()),
        "the number of the copy: {line}"
    );
    assert_eq!(
        field(line, "engine"),
        Some("unread"),
        "the copy of the end reads no head of the engine: {line}"
    );
    assert_eq!(
        field(line, "head"),
        Some("0"),
        "the copy of the end reads no head of the engine, so that step has no time: {line}"
    );
    assert_eq!(
        field(line, "keep"),
        Some("0"),
        "no keep follows the copy of the end: {line}"
    );
    assert_eq!(
        field(line, "hash"),
        Some("0"),
        "the store of the copy of the end hashes its bundle: {line}"
    );
    assert_eq!(
        field(line, "bytes"),
        Some(bundle.len().to_string().as_str()),
        "the size of the bundle in the bucket: {line}"
    );
    assert!(
        !d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn final copy failed")),
        "a stored copy of the end logs no failure"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b930_a_copy_of_the_end_that_is_not_stored_writes_the_failed_form_of_its_line() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        snapshot_every_s: Some(0),
        ..Opts::default()
    })
    .await;
    d.aws.fail_puts("snapshot.bundle", u32::MAX);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.aws.get(&keys::snapshot_json_key(RUN)).is_none(),
        "TRIGGER PRESENT is not shown: a copy of the end was stored"
    );

    // Each try of the copy of the end that failed has its save line, beside the line that names the error.
    let errors = d
        .log
        .lines()
        .iter()
        .filter(|l| l.contains("direct turn final copy failed: "))
        .count();
    let lines = end_lines(&d);
    assert!(
        !lines.is_empty() && lines.len() == errors,
        "TRIGGER PRESENT: {errors} tries of the copy of the end failed and {} have a save line; save lines:\n{}",
        lines.len(),
        save_lines(&d).join("\n")
    );
    for (i, line) in lines.iter().enumerate() {
        has_every_field_once(line);
        assert!(line.contains(" failed why=store by=final "), "{line}");
        assert!(
            field(line, "bytes").unwrap().parse::<u64>().unwrap() > 0,
            "{line}"
        );
        // Each try made its copy before its store failed, and the copy has its number, one above the try before.
        let n: u64 = field(line, "n").unwrap().parse().unwrap();
        if i > 0 {
            let before: u64 = field(&lines[i - 1], "n").unwrap().parse().unwrap();
            assert_eq!(n, before + 1, "{line}");
        } else {
            assert!(n >= 1, "{line}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b930_a_copy_of_the_end_given_up_at_its_budget_writes_its_line_with_the_step_it_was_in() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        snapshot_every_s: Some(0),
        edit_cfg: Some(Box::new(|c| {
            c.direct.final_copy_budget = Duration::from_secs(10)
        })),
        ..Opts::default()
    })
    .await;
    // The store takes 60 s to answer the upload of the copy: each try of the copy of the end is given up at its budget of 10 s, in
    // its store. The turns of this file run real git and the stand-in bucket in real time and drive no clock, so the test holds
    // on a crowded machine by room: the git work of the copy of the end on the small tree of this turn takes well under a second
    // on a quiet machine and has ten here, and the store answers six budgets later.
    d.aws.delay_puts("snapshot.bundle", Duration::from_secs(60));
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let error = d.summary().error.unwrap_or_default();
    assert!(
        error.contains("final copy: not confirmed within"),
        "TRIGGER PRESENT is not shown: the copy of the end was not given up at its budget: {error}"
    );

    // Each try of the copy of the end that was given up at its budget has one save line, beside the line that names the error: as
    // many save lines as tries, so none is written twice.
    let failed = d
        .log
        .lines()
        .iter()
        .filter(|l| l.contains("direct turn final copy failed: "))
        .count();
    let errors = d
        .log
        .lines()
        .iter()
        .filter(|l| l.contains("direct turn final copy failed: not confirmed within"))
        .count();
    assert!(
        errors >= 1 && errors == failed,
        "TRIGGER PRESENT is not shown: {failed} tries of the copy of the end failed and {errors} of them were given up at the budget"
    );
    let lines = end_lines(&d);
    assert_eq!(
        lines.len(),
        errors,
        "TRIGGER PRESENT: {errors} tries of the copy of the end were given up at the budget and {} save lines name them; save lines:\n{}",
        lines.len(),
        save_lines(&d).join("\n")
    );
    for (i, line) in lines.iter().enumerate() {
        has_every_field_once(line);
        assert!(
            line.contains(" failed why=store by=final "),
            "the step it was in: {line}"
        );
        // Each try made its copy before it was given up in its store, and the copy has its number, one above the try before.
        let n: u64 = field(line, "n").unwrap().parse().unwrap();
        if i > 0 {
            let before: u64 = field(&lines[i - 1], "n").unwrap().parse().unwrap();
            assert_eq!(n, before + 1, "{line}");
        } else {
            assert!(n >= 1, "the copy was made: {line}");
        }
        let total: u64 = field(line, "total").unwrap().parse().unwrap();
        assert!(
            total >= 9900,
            "the line is written when the budget of 10 s has run out: {line}"
        );
    }
}
