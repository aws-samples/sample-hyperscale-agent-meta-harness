//! `ham resume <runId>` and `ham resume --all [--dry-run]`. `--all` reads the caller's runs, takes the paused and stalled ones that no run of
//! the list continues, and sends `POST /v1/runs/{runId}/resume` for each with the key `resume-<run id>` and an empty body; a named run is sent as it
//! stands and the service decides. `--dry-run` sends no POST. Every run gets a line (or a JSON object) that says what it did, a refusal is told in
//! the service's words with exit code 1, and `ham runs --json` carries the reason, the resume mode and the session of a run that stopped.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::cli::{ExecContext, run_captured};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::command::parse;
use ham_core::config::Config;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";
const R4: &str = "01M5A00000000000000000R004";
const R5: &str = "01M5A00000000000000000R005";
const N1: &str = "01M5B00000000000000000N001";
const N2: &str = "01M5B00000000000000000N002";

type Call = (String, String, Vec<(String, String)>, String);

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push((
            method.to_string(),
            path.to_string(),
            headers.to_vec(),
            String::from_utf8_lossy(&body).to_string(),
        ));
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((500, json!({"code": "unscripted", "message": path})));
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        })
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("ham resume fetches nothing")
    }
}

fn run(id: &str, status: &str, mode: Option<&str>, continues: Option<&str>) -> Value {
    let mut v = json!({"runId": id, "status": status, "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-11-03T08:00:00Z", "ownerPrincipalArn": ARN});
    if let Some(m) = mode {
        v["resumeMode"] = json!(m);
        v["statusReason"] = json!("total_time: the run used the total time it was given");
        v["session"] = json!(3);
    }
    if let Some(c) = continues {
        v["continueFrom"] = json!(c);
    }
    v
}

struct Printed {
    code: i32,
    out: String,
}

async fn ham(replies: Vec<(u16, Value)>, argv: &[&str]) -> (Printed, Arc<Scripted>) {
    let plane = Arc::new(Scripted::default());
    *plane.replies.lock().unwrap() = replies.into();
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
    let invocation = parse(&argv).unwrap();
    let env = |_: &str| None;
    let downloads = std::env::temp_dir();
    let out = run_captured(
        &invocation,
        &ExecContext {
            api: &api,
            cfg: &Config::default(),
            downloads: &downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: None,
        },
    )
    .await;
    (
        Printed {
            code: out.code,
            out: out.stdout_text(),
        },
        plane,
    )
}

fn started(id: &str) -> (u16, Value) {
    (201, json!({"runId": id, "status": "active"}))
}

fn list(runs: Vec<Value>) -> (u16, Value) {
    (200, json!({"runs": runs}))
}

fn posts(plane: &Scripted) -> Vec<Call> {
    plane
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 == "POST")
        .cloned()
        .collect()
}

fn key(call: &Call) -> &str {
    call.2
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("idempotency-key"))
        .map(|(_, v)| v.as_str())
        .unwrap()
}

#[tokio::test]
async fn b689_resume_all_posts_one_resume_for_each_paused_or_stalled_run_with_its_key() {
    let runs = vec![
        run(R1, "paused", Some("follow_up"), None),
        run(R2, "stalled", Some("follow_up"), None),
        run(R3, "complete", None, None),
        run(R4, "active", None, None),
    ];
    let (p, plane) = ham(
        vec![list(runs), started(N1), started(N2)],
        &["resume", "--all"],
    )
    .await;
    assert_eq!(p.code, 0, "{}", p.out);
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("GET", "/v1/runs")
    );
    let posts = posts(&plane);
    assert_eq!(posts.len(), 2, "{calls:?}");
    assert_eq!(posts[0].1, format!("/v1/runs/{R1}/resume"));
    assert_eq!(posts[1].1, format!("/v1/runs/{R2}/resume"));
    assert_eq!(key(&posts[0]), format!("resume-{R1}"));
    assert_eq!(
        posts[0].3, "{}",
        "no prompt, rules or settings leave the client"
    );
    assert!(
        p.out.contains(R1) && p.out.contains(N1) && p.out.contains(R2) && p.out.contains(N2),
        "{}",
        p.out
    );
}

#[tokio::test]
async fn b689_a_dry_run_names_the_plan_and_sends_no_post() {
    let runs = vec![
        run(R1, "paused", Some("follow_up"), None),
        run(R2, "stalled", Some("none"), None),
    ];
    let (p, plane) = ham(vec![list(runs)], &["resume", "--all", "--dry-run"]).await;
    assert_eq!(p.code, 0, "{}", p.out);
    assert!(posts(&plane).is_empty());
    assert!(
        p.out
            .contains(&format!("run {R1}: would start a follow-up run")),
        "{}",
        p.out
    );
    assert!(p.out.contains(&format!("run {R2}: skipped")), "{}", p.out);
}

#[tokio::test]
async fn b689_a_run_that_a_run_of_the_list_continues_is_left_alone() {
    let runs = vec![
        run(R5, "stalled", Some("follow_up"), Some(R1)),
        run(R1, "paused", Some("follow_up"), None),
    ];
    let (p, plane) = ham(vec![list(runs), started(N1)], &["resume", "--all"]).await;
    assert_eq!(p.code, 0, "{}", p.out);
    let posts = posts(&plane);
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].1,
        format!("/v1/runs/{R5}/resume"),
        "the newest run of the line goes on"
    );
    assert!(p.out.contains(&format!("run {R1}: skipped")), "{}", p.out);
}

#[tokio::test]
async fn b689_a_named_run_is_sent_as_it_stands_and_the_list_is_not_read() {
    let (p, plane) = ham(vec![started(N1)], &["resume", R1]).await;
    assert_eq!(p.code, 0, "{}", p.out);
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("POST", format!("/v1/runs/{R1}/resume").as_str())
    );
    assert!(p.out.contains(N1), "{}", p.out);
}

#[tokio::test]
async fn b689_a_refusal_is_told_in_the_services_words_and_the_exit_code_is_1() {
    let refused = (
        409,
        json!({"code": "run-active", "message": format!("run {R1} has not ended; there is nothing to resume yet")}),
    );
    let (p, _) = ham(vec![refused], &["resume", R1]).await;
    assert_eq!(p.code, 1);
    assert!(p.out.contains("has not ended"), "{}", p.out);
    assert!(p.out.contains("not started"), "{}", p.out);
}

#[tokio::test]
async fn b689_json_prints_one_object_per_run() {
    let runs = vec![
        run(R1, "paused", Some("follow_up"), None),
        run(R2, "stalled", Some("none"), None),
    ];
    let (p, _) = ham(
        vec![list(runs), started(N1)],
        &["resume", "--all", "--json"],
    )
    .await;
    assert_eq!(p.code, 0);
    let lines: Vec<Value> = p
        .out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines[0],
        json!({"runId": R1, "result": "resumed", "mode": "follow_up", "newRunId": N1})
    );
    assert_eq!(lines[1]["runId"], json!(R2));
    assert_eq!(lines[1]["result"], json!("skipped"));
}

#[tokio::test]
async fn b689_ham_runs_json_carries_the_reason_the_mode_and_the_session_of_a_stopped_run() {
    let runs = vec![run(R1, "stalled", Some("follow_up"), None)];
    let (p, _) = ham(vec![list(runs)], &["runs", "--json"]).await;
    assert_eq!(p.code, 0, "{}", p.out);
    let v: Value = serde_json::from_str(p.out.lines().next().unwrap()).unwrap();
    let r = &v["runs"][0];
    assert_eq!(r["status"], json!("stalled"));
    assert_eq!(
        r["statusReason"],
        json!("total_time: the run used the total time it was given")
    );
    assert_eq!(r["resumeMode"], json!("follow_up"));
    assert_eq!(r["session"], json!(3));
}

#[tokio::test]
async fn b689_a_list_that_cannot_be_read_fails_resume_all_and_posts_nothing() {
    let down = (503, json!({"code": "unavailable", "message": "try again"}));
    let (p, plane) = ham(vec![down], &["resume", "--all"]).await;
    assert_ne!(p.code, 0, "{}", p.out);
    assert!(posts(&plane).is_empty());
}

#[tokio::test]
async fn b689_a_dry_run_as_json_prints_the_plan_one_object_per_run() {
    let runs = vec![
        run(R1, "paused", Some("follow_up"), None),
        run(R2, "stalled", Some("none"), None),
    ];
    let (p, plane) = ham(
        vec![list(runs)],
        &["resume", "--all", "--dry-run", "--json"],
    )
    .await;
    assert_eq!(p.code, 0, "{}", p.out);
    assert!(posts(&plane).is_empty());
    let lines: Vec<Value> = p
        .out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines[0],
        json!({"runId": R1, "result": "would-resume", "mode": "follow_up"})
    );
    assert_eq!(lines[1]["result"], json!("skipped"));
}

#[tokio::test]
async fn b689_a_list_with_nothing_to_resume_says_so_in_a_dry_run_and_in_a_real_run() {
    for argv in [
        vec!["resume", "--all", "--dry-run"],
        vec!["resume", "--all"],
    ] {
        let runs = vec![
            run(R1, "complete", None, None),
            run(R2, "active", None, None),
        ];
        let (p, plane) = ham(vec![list(runs)], &argv).await;
        assert_eq!(p.code, 0, "{argv:?}: {}", p.out);
        assert!(
            p.out.contains("no paused or stalled run to resume"),
            "{argv:?}: {}",
            p.out
        );
        assert!(posts(&plane).is_empty(), "{argv:?}");
    }
}

#[tokio::test]
async fn b689_an_answer_that_cannot_be_read_says_the_resume_may_not_have_reached_the_service() {
    let (p, _) = ham(vec![(201, json!({"unexpected": true}))], &["resume", R1]).await;
    assert_eq!(p.code, 1, "{}", p.out);
    assert!(p.out.contains("not started"), "{}", p.out);
    assert!(
        p.out.contains("may not have reached the service"),
        "{}",
        p.out
    );
}
