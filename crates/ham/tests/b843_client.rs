//! What the commands say to a person who follows a run without the run screen. `ham cancel` of a run that never started exits 0 and
//! names the snapshot command only for a run that has a saved copy; `ham watch` in line mode says when a newer copy of the work was saved
//! (at most once a minute) and prints the card of a blocked run; the `--json` view carries the copy and writes the phase of a delivered run
//! as the line does. The command tests run the compiled binary and the library watch against the stand-in `/v1` API (the real control-api
//! handler over an in-memory store); the last tests hold the guide to what the commands do.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::watch::{WatchOptions, exit_of, watch};
use ham_core::snapshot::SNAPSHOT_REF;
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::EventType;
use harness_protocol::goal::GoalStatus;
use home::{Home, ham, home};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Fake, REGION, creds};

const BASE: &str = "f6dc8e0f72e20000000000000000000000000000";
const HEAD: &str = "ad2645e679230000000000000000000000000000";

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

async fn shell(h: &Home, args: &[&str]) -> (i32, String, String) {
    let o = ham(h, args).await;
    (
        o.status.code().unwrap(),
        home::text(&o.stdout),
        home::text(&o.stderr),
    )
}

/// The copy agentd stores in the bucket: the bundle, then the record.
fn store_copy(fake: &Fake, run: &str, count: u32, last: bool) {
    let prefix = harness_protocol::direct::s3::turn_prefix(run);
    let mut bundle = format!("# v2 git bundle\n{HEAD} {SNAPSHOT_REF}\n\nPACK").into_bytes();
    bundle.extend((0..2_000).map(|i| (i % 251) as u8));
    let mut record = json!({"schemaVersion": 1, "headSha": HEAD, "baseSha": BASE, "at": "2026-11-12T00:47:40Z", "count": count});
    if last {
        record["final"] = json!(true);
    }
    let mut g = fake.mem.inner.lock().unwrap();
    g.objects.insert(
        format!("{prefix}snapshot.bundle"),
        (bundle, "\"etag-b\"".into()),
    );
    g.objects.insert(
        format!("{prefix}snapshot.json"),
        (record.to_string().into_bytes(), "\"etag-j\"".into()),
    );
}

/// The event agentd records after it stored a copy: a progress event with no `kind` and a `savedCopy` member.
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

// ---- ham cancel of a run that has no live session ------------------------------------------------------------------------------------

/// A run that never started is cleared by a cancel. The cancel was accepted, so `ham cancel` exits 0 (exit 15 is the code of a start
/// command or `ham watch` that follows the cleared run), and it does not send the person to a copy the run never saved.
#[tokio::test]
async fn b843_cancel_of_a_run_that_never_started_exits_0_and_names_no_copy() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run that never starts").await;
    let h = home(&fake, "b843-cancel-none");
    let (code, out, err) = shell(&h, &["cancel", &run]).await;
    assert_eq!(code, 0, "{out} {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        [
            format!("run {run} cleared").as_str(),
            "The run saved no copy of its work, so there is nothing to download.",
        ],
        "{out}"
    );
    // The command the old line named answers exactly that.
    let (code, _, err) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("no copy of this run's work was saved"),
        "TRIGGER PRESENT: the run has no copy: {err}"
    );
    // A follower of the cleared run ends with the code of the status.
    let (code, _, _) = shell(&h, &["watch", &run]).await;
    assert_eq!(code, 15, "the watch of a cleared run exits 15");
    // The JSON form is what it was.
    let other = start(&fake, "Another run that never starts").await;
    let (code, out, _) = shell(&h, &["cancel", &other, "--json"]).await;
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v, json!({"runId": other, "status": "cleared"}));
}

/// A cleared run that saved a copy of its work: the line names the command and the time of the copy, and the command downloads it.
#[tokio::test]
async fn b843_cancel_of_a_cleared_run_with_a_copy_names_the_command_and_the_time_of_the_copy() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run with a copy").await;
    store_copy(&fake, &run, 4, false);
    let h = home(&fake, "b843-cancel-copy");
    let (code, out, err) = shell(&h, &["cancel", &run]).await;
    assert_eq!(code, 0, "{out} {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        [
            format!("run {run} cleared"),
            format!(
                "ham pull {run} --snapshot downloads the copy of its work saved at 2026-11-12 00:47 UTC."
            ),
        ],
        "{out}"
    );
    let (code, out, err) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(code, 0, "{out} {err}");
    assert!(out.contains(&format!("Commit    {HEAD}")), "{out}");
}

// ---- ham watch in line mode and in JSON ------------------------------------------------------------------------------------------------

fn opts(json: bool) -> WatchOptions {
    WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: None,
    }
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

async fn wait_for(out: &Shared, needle: &str) {
    for _ in 0..500 {
        if out.text().contains(needle) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never saw {needle:?} in:\n{}", out.text());
}

/// While a run works, the line output says when a copy of its work was saved, with the command that downloads it; a second copy within the
/// same minute adds no line. The card at the end carries the copy of the end.
#[tokio::test]
async fn b843_watch_without_a_terminal_says_when_a_copy_was_saved_at_most_once_a_minute() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run that saves copies").await;
    let out = Shared::default();
    let task = {
        let (a, run, mut out) = (api(&fake), run.clone(), out.clone());
        tokio::spawn(async move {
            let mut err = Vec::new();
            watch(&a, &run, &opts(false), &mut out, &mut err).await
        })
    };
    working(&fake, &run);
    wait_for(&out, &format!("run {run} active (green)")).await;
    assert!(
        !out.text().contains("saved:"),
        "no copy yet:\n{}",
        out.text()
    );
    saved_event(&fake, &run, "2026-11-12T00:40:22Z", 1, false);
    let line = format!(
        "saved: a copy of the work at 2026-11-12 00:40 UTC; ham pull {run} --snapshot downloads it"
    );
    wait_for(&out, &line).await;
    // A newer copy a moment later: the view has it, and line mode stays quiet about it.
    saved_event(&fake, &run, "2026-11-12T00:40:52Z", 2, false);
    tokio::time::sleep(Duration::from_millis(400)).await;
    saved_event(&fake, &run, "2026-11-12T00:41:10Z", 3, true);
    fake.mem.complete_run(&run, true);
    let done = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(done.view.saved.as_ref().map(|c| c.count), Some(3));
    let text = out.text();
    assert_eq!(
        text.lines().filter(|l| l.starts_with("saved:")).count(),
        1,
        "one line for the copies of one minute:\n{text}"
    );
    assert!(
        text.contains(&format!(
            "Saved     2026-11-12 00:41 UTC, when Cline stopped; ham pull {run} --snapshot downloads it"
        )),
        "the card carries the copy of the end:\n{text}"
    );
}

/// The `--json` view carries the newest copy, and the phase of a delivered run reads as the line writes it.
#[tokio::test]
async fn b843_the_json_view_carries_the_saved_copy_and_writes_a_delivered_run_as_the_line_does() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run in JSON").await;
    working(&fake, &run);
    saved_event(&fake, &run, "2026-11-12T00:40:22Z", 7, true);
    fake.mem.complete_run(&run, true);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &run, &opts(true), &mut out, &mut err)
        .await
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(last["run"]["status"], "complete");
    assert_eq!(
        last["run"]["savedCopy"],
        json!({"at": "2026-11-12T00:40:22Z", "count": 7, "headSha": HEAD, "session": 1, "final": true}),
        "{last}"
    );
    assert_eq!(
        last["run"]["phase"], "delivered",
        "the line says `complete (delivered)`, and so does the view: {last}"
    );
    // The line output of the same run.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &run, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("run {run} complete (delivered)")),
        "TRIGGER PRESENT: {text}"
    );
    // A run that works keeps the stored phase, and a run without a copy has no member.
    let other = start(&fake, "Another run in JSON").await;
    working(&fake, &other);
    let mut state = ham_core::state::RunState::new(&other);
    api(&fake).poll(&mut state).await.unwrap();
    let v = ham::watch::view_json(&state.view());
    assert_eq!(v["phase"], "green");
    assert!(v.get("savedCopy").is_none(), "{v}");
}

/// A blocked run delivered no zip, and its work is saved: line mode prints the card, so a person who follows the run through a pipe sees
/// where the work is and what the run used.
#[tokio::test]
async fn b843_a_blocked_run_prints_its_card_in_line_mode() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run that ends blocked").await;
    working(&fake, &run);
    saved_event(&fake, &run, "2026-11-12T00:47:40Z", 9, true);
    fake.mem.record_direct_usage(
        &run,
        support::MODEL,
        harness_protocol::common::Usage {
            input_tokens: 1200,
            output_tokens: 300,
            ..Default::default()
        },
    );
    fake.mem.block_direct_run(
        &run,
        "bundle validation failed: workflow-file .github/workflows/ci.yml",
        Some(DirectOutcome::ReportedDone),
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(&api(&fake), &run, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    assert_eq!(done.view.status, Some(GoalStatus::Blocked));
    assert_eq!(exit_of(&done.view), 12);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("blocked: bundle validation failed"),
        "TRIGGER PRESENT: {text}"
    );
    for line in [
        format!("Run       {run} blocked (delivering)"),
        "Zip       n/a".to_string(),
        format!(
            "Saved     2026-11-12 00:47 UTC, when Cline stopped; ham pull {run} --snapshot downloads it"
        ),
        "Tokens    1500  USD ".to_string(),
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(&line)),
            "no line {line:?} in:\n{text}"
        );
    }
    // The card says nothing about a zip that does not exist.
    assert!(!text.contains("ham clone"), "{text}");
    // A blocked run with nothing saved and nothing used still names itself.
    let bare = start(&fake, "A blocked run with nothing").await;
    fake.mem
        .block_direct_run(&bare, "the model is not in the catalog", None);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &bare, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("Run       {bare} blocked (delivering)")),
        "{text}"
    );
    assert!(!text.contains("Saved"), "{text}");
}

// ---- the guide -------------------------------------------------------------------------------------------------------------------------

fn guide() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/user-guide.md");
    std::fs::read_to_string(path).unwrap()
}

/// The text of the section under `heading` (a line `## heading` or deeper), up to the next heading of the same or a higher level.
fn section(guide: &str, heading: &str) -> String {
    let mut out = String::new();
    let mut level = 0;
    for line in guide.lines() {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        let title = line.trim_start_matches('#').trim();
        if level == 0 {
            if hashes > 0 && title == heading {
                level = hashes;
            }
            continue;
        }
        if hashes > 0 && hashes <= level && line.as_bytes().get(hashes) == Some(&b' ') {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(level > 0, "the guide has no section {heading:?}");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn b843_the_guide_gives_the_exit_code_of_cancel_and_of_a_follower_of_the_cleared_run() {
    let body = section(&guide(), "Stopping a run and partial results");
    assert!(
        !body.contains("stops its workflow. The exit code is 15."),
        "exit 15 is the code of a command that follows the run, not of ham cancel: {body}"
    );
    for words in [
        "`ham cancel` itself exits 0",
        "A start command or `ham watch` that follows the run ends with exit 15",
        "The run saved no copy of its work, so there is nothing to download.",
        "downloads the copy of its work saved at",
    ] {
        assert!(body.contains(words), "{words:?} is missing from: {body}");
    }
}

#[test]
fn b843_the_guide_says_what_line_mode_and_json_show_of_a_saved_copy_and_of_a_blocked_run() {
    let all = guide().split_whitespace().collect::<Vec<_>>().join(" ");
    for words in [
        "saved: a copy of the work at <time>; ham pull <runId> --snapshot downloads it",
        "at most once a minute",
        "`savedCopy`",
        "A blocked run prints its card too",
        "`ham watch --json` writes `delivered` as the line does",
    ] {
        assert!(all.contains(words), "{words:?} is missing from the guide");
    }
    assert!(
        !all.contains("`--json` prints the stored phase."),
        "the sentence of before named every --json form"
    );
}

#[test]
fn b843_the_guide_says_that_a_check_leaves_nothing_behind_and_when_an_update_waits() {
    let body = section(&guide(), "Update with ham update");
    for words in [
        "leaves nothing in your download folder",
        "An update that deploys nothing on the stage waits for no run",
        "asks that setup for its plan",
    ] {
        assert!(body.contains(words), "{words:?} is missing from: {body}");
    }
}
