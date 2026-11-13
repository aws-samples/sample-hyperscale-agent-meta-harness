//! `ham runs` with run names. The text line keeps every byte it had up to and including `createdAt`, so `cut -d ' ' -f 1` still
//! returns the id; the name follows after two spaces, and the trailers (`ended early`, `needs input`, `probe`) follow the name. A run
//! without a name prints the line it printed before. An escape sequence in a name is removed. `--json` adds `name` and `nameSource` to a
//! run that has them and no other member. The runs come from `GET /v1/runs` as rows; a backend without names sends bare summaries.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::cli::{ExecContext, run_captured};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::command::parse;
use ham_core::config::Config;
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<String>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push(path.to_string());
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
        unreachable!("ham runs fetches nothing")
    }
}

fn run(id: &str, extra: Value) -> Value {
    let mut v = json!({"runId": id, "status": "complete", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-20T12:41:03Z", "ownerPrincipalArn": ARN});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    v
}

fn named(id: &str, name: &str, source: &str) -> Value {
    run(
        id,
        json!({"title": "Prompt title", "model": "Claude Sonnet 5.5", "lastActivity": "2026-10-20T12:50:00Z",
            "outcome": "Cline reported done", "name": name, "nameSource": source}),
    )
}

struct Printed {
    code: i32,
    out: String,
}

async fn ham_runs(pages: Vec<Value>, extra: &[&str]) -> (Printed, Arc<Scripted>) {
    let plane = Arc::new(Scripted::default());
    *plane.replies.lock().unwrap() = pages.into_iter().map(|b| (200, b)).collect();
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let mut argv = vec!["runs".to_string()];
    argv.extend(extra.iter().map(ToString::to_string));
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

const TODAY: &str = "complete (planning)  s3-only  2026-10-20T12:41:03Z";

#[tokio::test]
async fn a_run_with_a_name_prints_it_after_the_created_at_and_the_first_field_is_still_the_id() {
    let (p, plane) = ham_runs(
        vec![json!({"runs": [named(R1, "Login redirect loop fix for the OAuth callback", "ai")]})],
        &[],
    )
    .await;
    assert_eq!(p.code, 0);
    assert_eq!(
        p.out,
        format!("{R1}  {TODAY}  Login redirect loop fix for the OAuth callback\n")
    );
    assert_eq!(p.out.split(' ').next().unwrap(), R1);
    assert_eq!(plane.calls.lock().unwrap().clone(), ["/v1/runs"]);
}

#[tokio::test]
async fn a_seed_is_printed_like_a_name_and_a_run_without_a_name_prints_the_line_it_printed_before()
{
    let (p, _) = ham_runs(
        vec![json!({"runs": [
            named(R1, "Fix the login redirect loop", "seed"),
            run(R2, json!({})),
            run(R3, json!({"title": "A title and no name on an older backend"}))
        ]})],
        &[],
    )
    .await;
    let lines: Vec<&str> = p.out.lines().collect();
    assert_eq!(
        lines,
        [
            format!("{R1}  {TODAY}  Fix the login redirect loop"),
            format!("{R2}  {TODAY}"),
            format!("{R3}  {TODAY}"),
        ]
    );
}

#[tokio::test]
async fn the_trailers_follow_the_name() {
    let waiting = json!({"needsInput": {"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped", "expiresAt": "2026-10-20T13:00:00Z"}});
    let mut r1 = named(R1, "Fix the login redirect loop", "ai");
    r1["needsInput"] = waiting["needsInput"].clone();
    let mut r2 = named(R2, "Order list pagination with cursors", "ai");
    r2["kind"] = json!("probe");
    let (p, _) = ham_runs(vec![json!({"runs": [r1, r2]})], &["--probes"]).await;
    let lines: Vec<&str> = p.out.lines().collect();
    assert_eq!(
        lines[0],
        format!("{R1}  {TODAY}  Fix the login redirect loop  needs input: Cline stopped")
    );
    assert_eq!(
        lines[1],
        format!("{R2}  {TODAY}  Order list pagination with cursors  probe")
    );
}

#[tokio::test]
async fn an_escape_sequence_in_a_name_is_removed() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [named(R1, "Fix \u{1b}[31mthe\u{1b}[0m login \u{1b}]52;c;AAAA\u{7}bug", "ai")]})],
        &[],
    )
    .await;
    assert_eq!(p.out, format!("{R1}  {TODAY}  Fix the login bug\n"));
    assert!(!p.out.contains('\u{1b}') && !p.out.contains('\u{7}'));
}

#[tokio::test]
async fn json_adds_name_and_name_source_only_to_a_run_that_has_them() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [
            named(R1, "Login redirect loop fix for the OAuth callback", "ai"),
            run(R2, json!({}))
        ]})],
        &["--json"],
    )
    .await;
    let v: Value = serde_json::from_str(p.out.trim()).unwrap();
    let runs = v["runs"].as_array().unwrap();
    // The keys a summary prints on its own (what the command printed before names).
    let summary_keys = |id: &str| -> Vec<String> {
        let s: RunSummary = serde_json::from_value(run(id, json!({}))).unwrap();
        let mut k: Vec<String> = serde_json::to_value(&s)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        runs[0]["name"],
        "Login redirect loop fix for the OAuth callback"
    );
    assert_eq!(runs[0]["nameSource"], "ai");
    let mut want = summary_keys(R1);
    want.extend(["name".to_string(), "nameSource".to_string()]);
    want.sort();
    assert_eq!(
        keys(&runs[0]),
        want,
        "the summary's members plus the two, nothing else"
    );
    for gone in ["title", "model", "lastActivity", "outcome"] {
        assert!(
            runs[0].get(gone).is_none(),
            "{gone} is not a member of `ham runs --json`"
        );
    }
    // A run without a name prints the members it printed before.
    assert_eq!(keys(&runs[1]), summary_keys(R2));
}

#[tokio::test]
async fn a_backend_without_names_sends_bare_summaries_and_ham_prints_today_s_output() {
    let (text, _) = ham_runs(vec![json!({"runs": [run(R1, json!({}))]})], &[]).await;
    assert_eq!(text.out, format!("{R1}  {TODAY}\n"));
    let (json_out, _) = ham_runs(vec![json!({"runs": [run(R1, json!({}))]})], &["--json"]).await;
    let v: Value = serde_json::from_str(json_out.out.trim()).unwrap();
    assert!(v["runs"][0].get("name").is_none() && v["runs"][0].get("nameSource").is_none());
}

#[tokio::test]
async fn every_page_is_read_and_a_name_does_not_cost_a_second_request_per_run() {
    let (p, plane) = ham_runs(
        vec![
            json!({"runs": [named(R1, "First run named here today", "ai")], "nextToken": "t2"}),
            json!({"runs": [run(R2, json!({}))]}),
        ],
        &[],
    )
    .await;
    assert_eq!(p.out.lines().count(), 2);
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1].contains("nextToken=t2"), "{calls:?}");
}
