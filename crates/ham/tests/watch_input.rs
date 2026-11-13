//! `ham watch` in line mode tells the user when Cline needs them and what became of it: the question's card
//! and the commands that answer it print once when it opens, one line prints when it closes, a fresh session prints
//! `restarted from the work tree: session <n>`, and `--json` carries `input` and `sessions` only for a run that has them.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::watch::{WatchOptions, watch};
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Fake, MODEL, REGION, creds};

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
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
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": MODEL}),
    )
    .unwrap();
    let run = api(fake)
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id;
    // The direct session has started: the watch keeps reading the direct agent's shard.
    fake.mem.harness(
        &run,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
    );
    run
}

fn spawn_watch(fake: &Fake, run: &str, json: bool) -> (Shared, tokio::task::JoinHandle<()>) {
    let out = Shared::default();
    let (api, run, o) = (api(fake), run.to_string(), out.clone());
    let opts = WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: None,
    };
    let h = tokio::spawn(async move {
        let (mut out, mut err) = (o, Vec::new());
        watch(&api, &run, &opts, &mut out, &mut err).await.unwrap();
    });
    (out, h)
}

async fn until(out: &Shared, needle: &str) {
    let started = std::time::Instant::now();
    while !out.text().contains(needle) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no {needle:?} in\n{}",
            out.text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn finish(fake: &Fake, run: &str, h: tokio::task::JoinHandle<()>) {
    fake.mem
        .harness(run, EventType::RunCompleted, json!({"status": "blocked"}));
    tokio::time::timeout(Duration::from_secs(10), h)
        .await
        .expect("the watch ends on run.completed")
        .unwrap();
}

#[tokio::test]
async fn a_question_prints_its_card_and_commands_once_and_one_line_when_it_closes() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "watch asks").await;
    let (out, h) = spawn_watch(&fake, &run, false);
    until(&out, "agents 1").await;
    let q = fake.mem.needs_input(&run, "engine_stop");
    until(&out, "Cline needs you: Cline stopped its own prompt").await;
    // Several polls pass with the question open: nothing prints twice.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = out.text();
    assert_eq!(text.matches("Cline needs you:").count(), 1, "{text}");
    for want in [
        "Cline ended the prompt itself, twice.",
        "41 tool calls, 3 files changed.",
        "Last words: I will keep polling the build until it finishes, then append",
        "            step 4 to PROGRESS.md.",
        "Files: PROGRESS.md src/app.py tests/test_app.py",
        "Answer with one of:",
        &format!("ham say {run} '<message>'"),
        &format!("ham new-session {run} ['<message>']"),
        &format!("ham try-model {run} <model> ['<message>']"),
        &format!("ham finish {run}"),
        "min left to answer",
        "needs input: Cline stopped its own prompt",
    ] {
        assert!(text.contains(want), "{want:?} missing from\n{text}");
    }
    assert!(!text.contains("closed:"), "{text}");
    // The user answers; the question closes with one line.
    fake.mem.input_event(
        &run,
        "input-received",
        &q,
        json!({"action": "continue_with_prompt", "textBytes": 5}),
    );
    until(&out, "Question 1 closed: answered with a message.").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let text = out.text();
    assert_eq!(text.matches("Question 1 closed").count(), 1, "{text}");
    finish(&fake, &run, h).await;
}

#[tokio::test]
async fn a_dead_session_lists_only_the_commands_that_can_work() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "dead session").await;
    fake.mem.needs_input_with(&run, "engine_died", |p| {
        p.title = "The Cline process stopped".into();
        p.options = vec![
            harness_protocol::direct::InstructAction::NewSessionWithPrompt,
            harness_protocol::direct::InstructAction::Finish,
        ];
        p.can_continue = false;
        p.models = vec![];
    });
    let (out, h) = spawn_watch(&fake, &run, false);
    until(&out, "Answer with one of:").await;
    let text = out.text();
    assert!(
        text.contains(&format!("ham new-session {run}"))
            && text.contains(&format!("ham finish {run}")),
        "{text}"
    );
    assert!(
        !text.contains("ham say") && !text.contains("ham try-model"),
        "{text}"
    );
    finish(&fake, &run, h).await;
}

#[tokio::test]
async fn the_outage_notice_says_no_answer_is_needed() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "outage notice").await;
    fake.mem.needs_input_with(&run, "provider_outage", |p| {
        p.title = "Every model route is down".into();
        p.blocking = false;
        p.options = vec![];
        p.models = vec![];
        p.can_continue = false;
        p.wait_s = 0;
        p.expires_at = None;
        p.last_message = None;
    });
    let (out, h) = spawn_watch(&fake, &run, false);
    until(&out, "Notice: Every model route is down").await;
    let text = out.text();
    assert!(
        text.contains("No answer is needed.") && text.contains(&format!("ham cancel {run}")),
        "{text}"
    );
    assert!(
        !text.contains("Answer with one of:") && !text.contains("needs input:"),
        "{text}"
    );
    finish(&fake, &run, h).await;
}

#[tokio::test]
async fn a_fresh_session_prints_one_line() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "fresh session line").await;
    let (out, h) = spawn_watch(&fake, &run, false);
    until(&out, "agents 1").await;
    fake.mem.new_session(&run, 2, "harness", None);
    until(&out, "restarted from the work tree: session 2").await;
    fake.mem.new_session(&run, 3, "user", None);
    until(&out, "restarted from the work tree: session 3").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let text = out.text();
    assert_eq!(
        text.matches("restarted from the work tree: session 2")
            .count(),
        1,
        "{text}"
    );
    finish(&fake, &run, h).await;
}

#[tokio::test]
async fn a_watch_started_late_does_not_replay_questions_it_never_saw_open() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "late watch").await;
    let q = fake.mem.needs_input(&run, "engine_stop");
    fake.mem.input_event(
        &run,
        "input-received",
        &q,
        json!({"action": "finish", "textBytes": 0}),
    );
    fake.mem
        .input_event(&run, "input-closed", &q, json!({"how": "finish"}));
    fake.mem.new_session(&run, 2, "user", Some(&q));
    let (out, h) = spawn_watch(&fake, &run, false);
    until(&out, "restarted from the work tree: session 2").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let text = out.text();
    assert!(
        !text.contains("Cline needs you") && !text.contains("closed:"),
        "{text}"
    );
    finish(&fake, &run, h).await;
}

fn json_lines(out: &Shared) -> Vec<Value> {
    out.text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[tokio::test]
async fn json_mode_carries_input_and_sessions_only_when_the_run_has_them() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "json input").await;
    let (out, h) = spawn_watch(&fake, &run, true);
    until(&out, "\"agents\"").await;
    for l in json_lines(&out) {
        assert!(
            l["run"].get("input").is_none() && l["run"].get("sessions").is_none(),
            "{l}"
        );
    }
    let q = fake.mem.needs_input(&run, "engine_stop");
    until(&out, &q).await;
    let open = json_lines(&out)
        .into_iter()
        .rev()
        .find(|l| l["run"]["input"]["open"].is_object())
        .expect("a line with the open question");
    let o = &open["run"]["input"]["open"];
    assert_eq!(o["requestId"], q);
    assert_eq!(o["reason"], "engine_stop");
    assert_eq!(o["blocking"], true);
    assert_eq!(o["options"].as_array().unwrap().len(), 4);
    assert_eq!(o["models"].as_array().unwrap().len(), 2);
    assert!(
        o.get("secsLeft").is_none(),
        "a changing member would print a line on every poll"
    );
    fake.mem.input_event(
        &run,
        "input-received",
        &q,
        json!({"action": "new_session_with_prompt", "textBytes": 0}),
    );
    fake.mem.new_session(&run, 2, "user", Some(&q));
    until(&out, "\"sessions\":2").await;
    let last = json_lines(&out).pop().unwrap();
    assert_eq!(last["run"]["input"]["open"], Value::Null);
    assert_eq!(
        last["run"]["input"]["history"][0]["how"],
        "answered:new_session_with_prompt"
    );
    finish(&fake, &run, h).await;
}

fn summary(extra: Value) -> harness_protocol::api::v1::RunSummary {
    let mut v = json!({"runId": "01M4ZHFBV5SDX7J92E6YRH4QTW", "status": "active", "phase": "planning",
        "target": {"mode": "s3-only"}, "createdAt": "2026-10-15T13:00:00Z",
        "ownerPrincipalArn": "arn:aws:sts::800000000022:assumed-role/Admin/testuser"});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn ham_runs_marks_a_run_that_waits_for_the_user() {
    let waiting = summary(
        json!({"needsInput": {"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped its own prompt\u{1b}[2J", "expiresAt": "2026-10-15T14:11:07Z"}}),
    );
    assert_eq!(
        ham::cli::runs_line(&waiting),
        "01M4ZHFBV5SDX7J92E6YRH4QTW  active (planning)  s3-only  2026-10-15T13:00:00Z  needs input: Cline stopped its own prompt"
    );
    // A run that asks nothing prints the line it printed before.
    assert_eq!(
        ham::cli::runs_line(&summary(json!({}))),
        "01M4ZHFBV5SDX7J92E6YRH4QTW  active (planning)  s3-only  2026-10-15T13:00:00Z"
    );
    // The JSON of `ham runs --json` carries the member for the run that waits and none for the others.
    let j = serde_json::to_value(&waiting).unwrap();
    assert_eq!(j["needsInput"]["reason"], "engine_stop");
    assert!(
        serde_json::to_value(summary(json!({})))
            .unwrap()
            .get("needsInput")
            .is_none()
    );
}
