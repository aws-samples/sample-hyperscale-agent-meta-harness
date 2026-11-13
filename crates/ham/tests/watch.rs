//! `ham watch <runId>`: cursor polling of the event shards folded
//! through `ham_core::state::RunState`, rehydrated from the API alone; plain lines or `--json`
//! lines; only harness-authored events end the watch; on delivery the zip is downloaded and the
//! final card printed; the exit code is `ham_core::exit::exit_code` of the final status.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::watch::{WatchError, WatchOptions, exit_of, finished, watch};
use ham_core::exit::exit_code;
use ham_core::state::RunState;
use harness_protocol::envelope::EventType;
use harness_protocol::goal::GoalStatus;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Fake, REGION, creds, sha256_hex};

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
}

fn opts(dir: Option<std::path::PathBuf>, json: bool) -> WatchOptions {
    WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: dir,
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

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{prompt}"))
        .await
        .unwrap()
        .run_id
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

#[tokio::test]
async fn a_finished_run_rehydrates_from_the_api_downloads_the_zip_and_prints_the_card() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "rehydrate").await;
    let (artifact, sha) = fake.mem.complete_run(&run, true).unwrap();
    let dir = support::scratch("watch-dl");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(
        &api(&fake),
        &run,
        &opts(Some(dir.clone()), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    assert_eq!(done.view.status, Some(GoalStatus::Complete));
    assert_eq!(exit_code(done.view.status), 0);
    let pulled = done.pulled.unwrap();
    assert_eq!(pulled.artifact.artifact_id, artifact);
    assert!(pulled.path.starts_with(&dir));
    assert_eq!(sha256_hex(&std::fs::read(&pulled.path).unwrap()), sha);
    let text = String::from_utf8(out).unwrap();
    for want in [
        &format!("run {run} complete (delivered)"),
        &format!("Run       {run} complete (delivered)"),
        &format!(
            "PR        https://{REGION}.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc/pull-requests/7/details"
        ),
        &format!("Zip       {}", pulled.path.display()),
        "(expires ",
        "Outcome   Cline reported done",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(
        text.contains(&format!("Link      {}", pulled.url.url)),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'));
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    // Every /v1 call of the watch was signed and verified.
    assert!(fake.mem.seen().iter().all(|s| s.signature.is_ok()));
}

#[tokio::test]
async fn progress_lines_follow_the_run_and_agent_authored_status_events_change_nothing() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "progress").await;
    let out = Shared::default();
    let task = {
        let (a, run, mut out) = (api(&fake), run.clone(), out.clone());
        tokio::spawn(async move {
            let mut err = Vec::new();
            watch(&a, &run, &opts(None, false), &mut out, &mut err).await
        })
    };
    wait_for(&out, &format!("run {run} active (planning)")).await;
    // An agent principal claims completion and delivery: stored with authority=agent, ignored.
    fake.mem.agent(
        &run,
        "A9",
        EventType::GoalStatus,
        json!({"status": "complete"}),
    );
    fake.mem.agent(
        &run,
        "A9",
        EventType::RunCompleted,
        json!({"status": "complete"}),
    );
    fake.mem.agent(
        &run,
        "A9",
        EventType::DeliveryPr,
        json!({"url": "https://example.test/other-pr"}),
    );
    wait_for(&out, "ignored 3").await;
    assert!(
        !task.is_finished(),
        "an agent-authored completion does not end the watch"
    );
    fake.mem.harness(
        &run,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": support::MODEL}),
    );
    wait_for(&out, "agents 1").await;
    fake.mem.complete_run(&run, false);
    let done = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(done.view.status, Some(GoalStatus::Complete));
    assert!(done.pulled.is_none());
    let text = out.text();
    assert!(!text.contains("example.test/other-pr"), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    let unique: std::collections::BTreeSet<&&str> = lines.iter().collect();
    assert_eq!(
        unique.len(),
        lines.len(),
        "a line is printed only on change:\n{text}"
    );
}

#[tokio::test]
async fn json_lines_are_machine_readable_and_carry_no_escape_sequences() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "json").await;
    fake.mem.complete_run(&run, true);
    let dir = support::scratch("watch-json");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(
        &api(&fake),
        &run,
        &opts(Some(dir), true),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let last = lines.last().unwrap();
    assert_eq!(last["exitCode"], 0);
    assert_eq!(last["run"]["runId"], run.as_str());
    assert_eq!(last["run"]["status"], "complete");
    assert_eq!(last["run"]["outcome"], "reported_done");
    assert_eq!(last["run"]["label"], "Cline reported done");
    assert!(last["run"].get("gates").is_none() && last["run"].get("verdicts").is_none());
    assert_eq!(last["run"]["agents"][0]["agentId"], "direct");
    assert_eq!(last["run"]["agents"][0]["modelId"], support::MODEL);
    assert_eq!(
        last["run"]["agents"][0]["lastSummary"],
        "added percent_change with tests"
    );
    assert!(last["download"]["path"].as_str().unwrap().ends_with(".zip"));
    assert!(last["download"]["expiresAt"].is_string());
    assert!(!text.contains('\u{1b}') && !text.contains("\\u001b") && !text.contains("sentinel"));
}

#[tokio::test]
async fn transient_errors_while_polling_are_survived() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "transient").await;
    fake.mem.complete_run(&run, false);
    fake.faults
        .events_503
        .store(40, std::sync::atomic::Ordering::SeqCst);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(&api(&fake), &run, &opts(None, false), &mut out, &mut err)
        .await
        .unwrap();
    assert_eq!(done.view.status, Some(GoalStatus::Complete));
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("retrying"), "{err}");
}

#[tokio::test]
async fn an_unknown_run_is_an_error_and_a_blocked_run_exits_with_its_code() {
    let fake = Fake::start().await;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let e = watch(
        &api(&fake),
        "01K5ZQ4T8M3N6P7R9S0V1W2X3Y",
        &opts(None, false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, WatchError::Api(_)), "{e:?}");
    assert!(e.to_string().contains("no run"), "{e}");

    let run = new_run(&fake, "blocked").await;
    fake.mem.harness(
        &run,
        EventType::GoalBlocked,
        json!({"reason": "the same failing test three times"}),
    );
    fake.mem
        .harness(&run, EventType::RunCompleted, json!({"status": "blocked"}));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(&api(&fake), &run, &opts(None, false), &mut out, &mut err)
        .await
        .unwrap();
    assert_eq!(exit_code(done.view.status), 12);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("blocked: the same failing test three times"),
        "{text}"
    );
}

#[tokio::test]
async fn a_full_shard_page_is_drained_in_one_cycle() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "drain").await;
    for i in 0..1200 {
        fake.mem.agent(
            &run,
            "A1",
            EventType::TurnProgress,
            json!({"text": format!("step {i}")}),
        );
    }
    let mut state = RunState::new(&run);
    let accepted = api(&fake).poll(&mut state).await.unwrap();
    assert_eq!(accepted, 1201, "run.created plus 1,200 progress events");
    assert_eq!(state.view().agents["A1"].last_summary, "step 1199");
    assert_eq!(api(&fake).poll(&mut state).await.unwrap(), 0);
}

#[test]
fn a_run_is_finished_on_run_completed_cleared_or_a_delivered_completion() {
    let mut v = RunState::new("R1").view();
    assert!(!finished(&v));
    v.status = Some(GoalStatus::Complete);
    assert!(!finished(&v), "complete but not yet delivered");
    v.zip = Some(ham_core::state::ZipLink {
        artifact_id: "A".into(),
        sha256: "ab".repeat(32),
        size_bytes: 1,
    });
    assert!(finished(&v));
    let mut v = RunState::new("R1").view();
    v.status = Some(GoalStatus::Cleared);
    assert!(finished(&v));
    let mut v = RunState::new("R1").view();
    v.status = Some(GoalStatus::Blocked);
    assert!(!finished(&v), "blocked is resumable");
    v.completed = true;
    assert!(finished(&v));
}

#[tokio::test]
async fn a_delivery_whose_sha256_disagrees_is_refused_and_a_missing_zip_id_falls_back() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "mismatch").await;
    let (artifact, _) = fake.mem.complete_run(&run, true).unwrap();
    fake.mem
        .inner
        .lock()
        .unwrap()
        .artifacts
        .get_mut(&(run.clone(), artifact))
        .unwrap()
        .sha256 = "0".repeat(64);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let e = watch(
        &api(&fake),
        &run,
        &opts(Some(support::scratch("watch-mismatch")), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, WatchError::Pull(_)), "{e:?}");
    assert!(e.to_string().contains("delivery.ready announced"), "{e}");

    // delivery.ready names an artifact the list lacks: the newest zip is taken, checked the same way.
    let run = new_run(&fake, "fallback").await;
    let (artifact, sha) = fake.mem.deliverable(&run, support::zip_bytes(100));
    fake.mem.harness(
        &run,
        EventType::DeliveryReady,
        json!({"artifactId": "01K6ZZZZZZZZZZZZZZZZZZZZZZ", "sha256": sha, "sizeBytes": 100}),
    );
    fake.mem
        .harness(&run, EventType::RunCompleted, json!({"status": "complete"}));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(
        &api(&fake),
        &run,
        &opts(Some(support::scratch("watch-fallback")), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    assert_eq!(done.pulled.unwrap().artifact.artifact_id, artifact);

    // A cleared run has no zip: nothing downloads and no card prints.
    let run = new_run(&fake, "cleared").await;
    fake.mem
        .harness(&run, EventType::GoalStatus, json!({"status": "cleared"}));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let done = watch(
        &api(&fake),
        &run,
        &opts(Some(support::scratch("watch-cleared")), false),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    assert_eq!(exit_code(done.view.status), 15);
    assert!(done.pulled.is_none());
    assert!(!String::from_utf8(out).unwrap().contains("Evidence"));
}

#[tokio::test]
async fn a_run_that_disappears_mid_watch_ends_the_watch_with_the_apis_reason() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "vanish").await;
    let out = Shared::default();
    let task = {
        let (a, run, mut out) = (api(&fake), run.clone(), out.clone());
        tokio::spawn(async move {
            let mut err = Vec::new();
            watch(&a, &run, &opts(None, false), &mut out, &mut err).await
        })
    };
    wait_for(&out, "active (planning)").await;
    fake.mem.inner.lock().unwrap().runs.clear();
    let e = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(e, WatchError::Api(_)) && e.to_string().contains("404"),
        "{e}"
    );
}

const OPUS: &str = "us.anthropic.claude-opus-5-5";

fn spawn_watch(
    fake: &std::sync::Arc<Fake>,
    run: &str,
    out: &Shared,
    json: bool,
) -> tokio::task::JoinHandle<Result<ham::watch::WatchOutcome, WatchError>> {
    let (a, run, mut out) = (api(fake), run.to_string(), out.clone());
    tokio::spawn(async move {
        let mut err = Vec::new();
        watch(&a, &run, &opts(None, json), &mut out, &mut err).await
    })
}

#[test]
fn the_terminal_table_covers_every_status_word() {
    // One row per status word: the exit code when the run is finished, and when it is finished.
    // `finished` reads the status, `run.completed` and the zip.
    let words = [
        ("active", 10),
        ("paused", 11),
        ("blocked", 12),
        ("usage_limited", 13),
        ("budget_limited", 14),
        ("complete", 0),
        ("cleared", 15),
    ];
    let zip = ham_core::state::ZipLink {
        artifact_id: "A".into(),
        sha256: "ab".repeat(32),
        size_bytes: 1,
    };
    let mut seen = 0;
    for (word, code) in words {
        let status: GoalStatus = serde_json::from_value(json!(word)).unwrap();
        for completed in [false, true] {
            for with_zip in [false, true] {
                let mut v = RunState::new("R1").view();
                v.status = Some(status);
                v.completed = completed;
                v.zip = with_zip.then(|| zip.clone());
                let want_finished = completed
                    || status == GoalStatus::Cleared
                    || (status == GoalStatus::Complete && with_zip);
                let case = format!("{word} completed={completed} zip={with_zip}");
                assert_eq!(finished(&v), want_finished, "{case}");
                assert_eq!(exit_code(v.status), code, "{case}");
                assert_eq!(exit_of(&v), code, "{case}");
                assert!(
                    ham_core::render::status_line(&v).contains(&format!(" {word} (")),
                    "the status word is the first word after the run id: {case}"
                );
                seen += 1;
            }
        }
    }
    assert_eq!(seen, 7 * 4);
}

#[tokio::test]
async fn json_watch_names_each_models_window_only_while_a_session_reports_one() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "window-json").await;
    let out = Shared::default();
    let task = spawn_watch(&fake, &run, &out, true);
    wait_for(&out, "\"runId\"").await;
    assert!(
        !out.text().contains("\"windows\""),
        "no window, no key:\n{}",
        out.text()
    );
    fake.mem.harness(
        &run,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": OPUS, "contextTokens": 1_000_000}),
    );
    wait_for(&out, "\"windows\"").await;
    let known = out
        .text()
        .lines()
        .rev()
        .find(|l| l.contains("\"windows\""))
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&known).unwrap();
    assert_eq!(v["run"]["windows"][OPUS], 1_000_000);
    assert_eq!(v["run"]["agents"][0]["contextTokens"], 1_000_000);
    fake.mem
        .harness(&run, EventType::RunCompleted, json!({"status": "blocked"}));
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
