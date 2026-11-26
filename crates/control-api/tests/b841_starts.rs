//! An update may let the stage keep starting runs. The refusal of new runs while a deploy holds the deploy lock
//! is the default, and `./setup.sh --update --allow-active-runs --allow-new-runs` turns it off for that one update: setup writes the line `starts=yes`
//! into the lock it holds, and the stage's API service starts runs under a lock with that exact line. Each such start logs one line, because the
//! run begins while a deploy changes the stage. A lock without the line closes the stage as before; a lock nobody renews and a lock that deploys
//! nothing keep their own answers.
//!
//! The stand-in behind `Store`, `Blobs` and `Execution` is the one the other chat-client routes use; the handlers are the real ones.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use control_api::admission::{MESSAGE, STAGE_UPDATING, STALE_AFTER_S};
use control_api::handler::{DeployLock, V1Response, handle_logged};
use serde_json::{Value, json};

/// The lock as setup writes it for a deploy: no `deploys` line and no `starts` line.
const LOCK: &str = "token=tok-b841\nholder=laptop:4242\nidentity=arn:aws:sts::123456789012:assumed-role/Admin/operator\nroute=runner\nstarted=2026-11-14T16:00:00Z\ncommit=8dd391d43f9011813f3c5f8988ec562436ff4b2e\ninputs=aa\nbuild=";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The stage holds a lock with `value` that SSM wrote `age_s` seconds before the clock read `at` (Unix seconds).
fn hold_at(w: &World, value: &str, age_s: i64, at: i64) {
    *w.deploy_lock.lock().unwrap() = Some(Ok(DeployLock {
        value: value.to_string(),
        written_epoch_s: Some(at - age_s),
    }));
}

/// The stage holds a lock with `value` that SSM wrote `age_s` seconds ago.
fn hold(w: &World, value: &str, age_s: i64) {
    hold_at(w, value, age_s, now());
}

async fn start(w: &World, key: &str) -> (V1Response, Value, Vec<String>) {
    let body = json!({"prompt": "Add percent_change(a, b) with tests", "target": {"mode": "s3-only"}, "model": SONNET, "rules": []});
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &body.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let (out, logs) = handle_logged(r, &deps(w)).await;
    let v = serde_json::from_str(&out.body).unwrap_or(Value::Null);
    (out, v, logs)
}

fn about_the_lock(logs: &[String]) -> Vec<&String> {
    logs.iter().filter(|l| l.contains("deploy lock")).collect()
}

/// How often a test that reads the lock's age in the service's line asks again, when the clock's second turned while the service answered.
const TRIES: u32 = 5;

/// The age in whole seconds that a line about the lock names: the number in "written <n> s ago".
fn named_age(line: &str) -> i64 {
    line.split_once("written ")
        .and_then(|(_, rest)| rest.split_once(" s ago"))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or_else(|| panic!("the line names no age of the lock: {line}"))
}

#[tokio::test]
async fn b841_starts_a_lock_that_lets_runs_start_does_not_refuse_and_logs_one_line() {
    // The age in the line is counted to the service's own read of the system clock, in whole seconds. The test reads the clock before it
    // writes the lock and again after the answer, holds the age to the seconds between the two reads, and asks again on a new stage while
    // a second turned between them, until it has seen the exact 60 (`b804_admit_a_start_is_refused_while_a_renewed_lock_is_held` in
    // `tests/b804_admit.rs` says why).
    for _ in 0..TRIES {
        let w = World::new();
        let before = now();
        hold_at(&w, &format!("{LOCK}\nstarts=yes"), 60, before);
        println!(
            "B841-STARTS TRIGGER PRESENT: a renewed lock with no deploys=no line is held: {}",
            w.deploy_lock.lock().unwrap().is_some()
        );
        let (out, v, logs) = start(&w, "k-1").await;
        let turned = now() - before;
        assert_eq!(out.status, 201, "the update lets new runs start: {v}");
        assert_eq!(out.retry_after_s, None);
        assert_eq!(w.runs.lock().unwrap().len(), 1, "the run was created");
        assert_eq!(w.started.lock().unwrap().len(), 1, "its execution started");
        let about = about_the_lock(&logs);
        assert_eq!(
            about.len(),
            1,
            "one line for a start under a deploy: {logs:?}"
        );
        assert!(
            about[0].contains("starts=yes"),
            "the line names the reason: {logs:?}"
        );
        let age = named_age(about[0]);
        assert!(
            (60..=60 + turned).contains(&age),
            "the line names the lock's age at the service's read of the clock. The lock was written 60 s before this test's read, and the \
             clock's second turned {turned} time(s) before the answer came, so the age is 60 at the least and {} at the most, never {age}: \
             {logs:?}",
            60 + turned
        );
        assert!(
            !about[0].contains("tok-b841"),
            "the token of the lock stays out of the log: {logs:?}"
        );
        if turned == 0 {
            return;
        }
    }
    panic!(
        "the clock's second turned while the service answered in each of {TRIES} tries, so the test never saw the exact age of 60 s"
    );
}

#[tokio::test]
async fn b841_starts_only_the_exact_line_opens_a_held_lock() {
    for value in [
        format!("{LOCK}\nstarts=yes-extra"),
        format!("{LOCK}\nstarts=YES"),
        format!("{LOCK}\nstarts=true"),
        format!("{LOCK}\nstarts=no"),
        format!("{LOCK}\nstarts="),
        format!("{LOCK}\nnote=starts=yes"),
        format!("{LOCK}\n starts=yes"),
        format!("{LOCK}\nstarts=yes "),
    ] {
        let w = World::new();
        hold(&w, &value, 60);
        let (out, v, _) = start(&w, "k-1").await;
        assert_eq!(out.status, 409, "{value:?} closes the stage: {v}");
        assert_eq!(v["code"], json!(STAGE_UPDATING), "{v}");
        assert_eq!(v["message"], json!(MESSAGE), "{v}");
    }
    // the line may stand anywhere, and a line end of either kind is no part of it
    for value in [
        format!("starts=yes\n{LOCK}"),
        format!("{LOCK}\r\nstarts=yes\r\nparties=a:1@2026-11-14T16:00:00Z"),
        format!("{LOCK}\nstarts=yes\nrenewed=2026-11-14T16:05:00Z"),
    ] {
        let w = World::new();
        hold(&w, &value, 60);
        let (out, v, _) = start(&w, "k-1").await;
        assert_eq!(out.status, 201, "{value:?} leaves the stage open: {v}");
    }
}

#[tokio::test]
async fn b841_starts_a_lock_without_the_line_refuses_as_before() {
    let w = World::new();
    hold(&w, LOCK, 60);
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 409, "{v}");
    assert_eq!(v["code"], json!(STAGE_UPDATING), "{v}");
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert_eq!(about_the_lock(&logs).len(), 1, "{logs:?}");
    assert!(!logs[0].contains("starts=yes"), "{logs:?}");
}

#[tokio::test]
async fn b841_starts_the_other_answers_of_a_lock_keep_their_own_lines() {
    // a hold that deploys nothing is the normal case of a client update and logs nothing, with the line or without it
    let w = World::new();
    hold(&w, &format!("{LOCK}\ndeploys=no\nstarts=yes"), 60);
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    assert!(about_the_lock(&logs).is_empty(), "{logs:?}");
    // a lock nobody renews is said as such: the line names the limit, not the option of an update that ended long ago
    let w = World::new();
    let age = i64::try_from(STALE_AFTER_S).unwrap() + 5;
    hold(&w, &format!("{LOCK}\nstarts=yes"), age);
    let (out, v, logs) = start(&w, "k-1").await;
    assert_eq!(out.status, 201, "{v}");
    let about = about_the_lock(&logs);
    assert_eq!(about.len(), 1, "{logs:?}");
    assert!(about[0].contains("nobody renews it"), "{logs:?}");
}

#[test]
fn b841_starts_the_judgement_of_a_lock_with_the_line() {
    use control_api::admission::{Verdict, judge};
    let at = 1_800_000_000;
    let lock = |value: &str, written: Option<i64>| DeployLock {
        value: value.into(),
        written_epoch_s: written,
    };
    let starts = format!("{LOCK}\nstarts=yes");
    assert_eq!(
        judge(&lock(&starts, Some(at - 60)), at),
        Verdict::StartsAllowed { age_s: 60 }
    );
    assert_eq!(
        judge(&lock(&starts, Some(at - 1800)), at),
        Verdict::StartsAllowed { age_s: 1800 },
        "the limit itself is still renewed"
    );
    // a lock nobody renews is judged by its age first: the option of an update that ended long ago is not what opens the stage
    assert_eq!(
        judge(&lock(&starts, Some(at - 1801)), at),
        Verdict::NotRenewed { age_s: Some(1801) }
    );
    assert_eq!(
        judge(&lock(&starts, None), at),
        Verdict::NotRenewed { age_s: None }
    );
    // a hold that deploys nothing needs no option to leave the stage open
    assert_eq!(
        judge(&lock(&format!("{starts}\ndeploys=no"), Some(at - 60)), at),
        Verdict::DeploysNothing
    );
    assert_eq!(
        judge(&lock(LOCK, Some(at - 60)), at),
        Verdict::Closed { age_s: 60 }
    );
}
