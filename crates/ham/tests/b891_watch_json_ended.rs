//! The lines `ham watch <run> --json` prints for a run that has ended. Every line is one object whose member `run` is the view
//! (`ham::watch::view_json`); the last line adds `exitCode` and `download` beside it. What the run said about the disks of its session
//! is in `run.diskNotices` of every line, each notice with its own time, and no line has that member at its top: a script that reads
//! the lines looks in `run`. Line mode of the same run prints the same sentences, once each, before the card.
//!
//! What the run said about its copies follows its own rule: a stored copy takes those notices away.
//! A run that ended with a stored copy of the end has no `copyNotices` and line mode prints no such sentence; a run whose last word
//! about a copy came after its newest stored copy keeps that notice in `run.copyNotices`, and line mode prints it.
//!
//! These cases passed when they were written. A scripted live row had looked for `diskNotices` at the top of the line and reported the
//! member as missing; the client carried it in `run`, as these cases hold.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::watch::{WatchOptions, watch};
use harness_protocol::envelope::{Envelope, EventType};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const HEAD: &str = "2d6e42d32a71bdcb8d3aa99ab84b28bfd80a3c28";
const W4: &str = "The workspace folder is nearly full: 920 MiB used against a stop mark of 960 MiB, over 90 percent of it. Nothing is deleted.";
const N1: &str = "The workspace folder holds 920 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.";
const N2: &str = "A new run with --compute instances, where the stage has such machines, keeps the workspace folder on the machine's own disk. That machine runs no containers.";
const COPY_NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
}

async fn start(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    let key: String = prompt.chars().filter(char::is_ascii_alphanumeric).collect();
    api(fake)
        .start_direct_run(&req, &format!("k-{key}"))
        .await
        .unwrap()
        .run_id
}

fn working(fake: &Fake, run: &str) {
    fake.mem.harness(
        run,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": support::MODEL}),
    );
    fake.mem.harness(
        run,
        EventType::GoalStatus,
        json!({"status": "active", "phase": "green"}),
    );
}

/// The worker's notice as the service stores it, with the time the worker gave it.
fn notify(fake: &Fake, run: &str, ts: &str, reason: &str, detail: &str) {
    let e: Envelope = serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": fake.mem.ulid(), "runId": run, "agentId": "direct", "seq": 0,
        "ts": ts, "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": {"turn": 1, "kind": "notify", "reason": reason, "outageMinutes": null, "detail": detail}}
    }))
    .unwrap();
    fake.mem.inner.lock().unwrap().events.push(e);
}

/// The event the worker records after it stored a copy of the work.
fn saved_event(fake: &Fake, run: &str, at: &str, count: u32, last: bool) {
    let mut copy = json!({"at": at, "count": count, "headSha": HEAD, "session": 1});
    if last {
        copy["final"] = json!(true);
    }
    fake.mem.agent(
        run,
        "direct",
        EventType::TurnProgress,
        json!({"turn": 1, "savedCopy": copy}),
    );
}

/// What `ham watch <run>` prints, in line mode or with `--json`, for a watch that starts now.
async fn printed(fake: &Fake, run: &str, json: bool) -> String {
    let opts = WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: None,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(fake), run, &opts, &mut out, &mut err)
        .await
        .unwrap();
    String::from_utf8(out).unwrap()
}

/// Every line `ham watch <run> --json` prints, each one JSON object with the view in its member `run`.
async fn json_lines(fake: &Fake, run: &str) -> Vec<Value> {
    let text = printed(fake, run, true).await;
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(!lines.is_empty(), "the watch printed no line");
    for line in &lines {
        assert!(line["run"].is_object(), "a line without a view: {line}");
    }
    lines
}

#[tokio::test]
async fn b891_an_ended_run_carries_its_disk_notices_in_the_run_member_of_every_json_line() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run on a large workspace folder").await;
    working(&fake, &run);
    notify(&fake, &run, "2026-11-22T16:19:25Z", "disk_low", W4);
    notify(&fake, &run, "2026-11-22T16:19:26Z", "disk_note", N1);
    notify(&fake, &run, "2026-11-22T16:19:27Z", "disk_note", N2);
    saved_event(&fake, &run, "2026-11-22T16:19:44Z", 2, true);
    fake.mem.complete_run(&run, true);

    let lines = json_lines(&fake, &run).await;
    for line in &lines {
        assert_eq!(
            line["run"]["diskNotices"],
            json!([
                {"at": "2026-11-22T16:19:25Z", "reason": "disk_low", "text": W4},
                {"at": "2026-11-22T16:19:26Z", "reason": "disk_note", "text": N1},
                {"at": "2026-11-22T16:19:27Z", "reason": "disk_note", "text": N2},
            ]),
            "the three notices, each with its time, in the view of the line: {line}"
        );
        assert!(
            line.get("diskNotices").is_none(),
            "the member is in the view, not at the top of the line: {line}"
        );
    }
    let last = lines.last().unwrap();
    assert_eq!(last["exitCode"], 0, "{last}");
    assert_eq!(last["run"]["status"], "complete");
    assert_eq!(last["run"]["completed"], true);
    assert_eq!(last["run"]["savedCopy"]["final"], true, "{last}");
    assert!(last.as_object().unwrap().contains_key("download"), "{last}");
    // The copy of the end was stored, and the run said nothing about a copy: no member for that.
    assert!(last["run"].get("copyNotices").is_none(), "{last}");

    // Line mode of the same run: the three sentences once each, in the order of their events, before the card.
    let text = printed(&fake, &run, false).await;
    let places: Vec<usize> = [W4, N1, N2]
        .iter()
        .map(|s| {
            assert_eq!(text.matches(s).count(), 1, "once: {s}\n{text}");
            text.find(s).unwrap()
        })
        .collect();
    assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}\n{text}");
    let card = text
        .find(&format!("Run       {run} "))
        .unwrap_or_else(|| panic!("no card:\n{text}"));
    assert!(
        places.iter().all(|p| *p < card),
        "{places:?} {card}\n{text}"
    );
}

#[tokio::test]
async fn b891_an_ended_run_carries_a_copy_notice_only_when_no_copy_was_stored_after_it() {
    let fake = Fake::start().await;
    // A copy that could not be saved, then the stored copy of the end: the stored copy took the notice away, in both modes.
    let kept = start(&fake, "A run whose copy of the end was stored").await;
    working(&fake, &kept);
    notify(
        &fake,
        &kept,
        "2026-11-22T16:19:30Z",
        "copy_not_saved",
        COPY_NOT_SAVED,
    );
    saved_event(&fake, &kept, "2026-11-22T16:19:44Z", 2, true);
    fake.mem.complete_run(&kept, true);
    for line in json_lines(&fake, &kept).await {
        assert!(line["run"].get("copyNotices").is_none(), "{line}");
        assert!(line.get("copyNotices").is_none(), "{line}");
    }
    let text = printed(&fake, &kept, false).await;
    assert!(!text.contains(COPY_NOT_SAVED), "{text}");

    // The notice came after the newest stored copy and no copy followed: it stands when the run has ended, in both modes.
    let late = start(&fake, "A run whose last copy was not stored").await;
    working(&fake, &late);
    saved_event(&fake, &late, "2026-11-22T16:19:20Z", 1, false);
    notify(
        &fake,
        &late,
        "2026-11-22T16:19:30Z",
        "copy_not_saved",
        COPY_NOT_SAVED,
    );
    fake.mem.complete_run(&late, true);
    let lines = json_lines(&fake, &late).await;
    let last = lines.last().unwrap();
    assert_eq!(last["run"]["status"], "complete");
    assert_eq!(
        last["run"]["copyNotices"],
        json!([{"at": "2026-11-22T16:19:30Z", "text": COPY_NOT_SAVED, "endRequest": false}]),
        "{last}"
    );
    assert!(last.get("copyNotices").is_none(), "{last}");
    let text = printed(&fake, &late, false).await;
    assert_eq!(text.matches(COPY_NOT_SAVED).count(), 1, "{text}");
}
