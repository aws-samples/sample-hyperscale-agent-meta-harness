//! `ham runs` with run tags. The text line keeps every byte it had up to and including the name, so `cut -d ' ' -f 1` still
//! returns the id; the tags follow the name as `  [a b c]` (the creation time when the run has no name), before the trailers (`ended early`,
//! `needs input`, `probe`). A run with no tags prints the line it printed before, no bracket. Every tag passes the cleaning of the client: an
//! escape sequence is removed and a tag that breaks the grammar (a server that sent text with spaces could otherwise forge a trailer) is left
//! out. `--json` gives a run `tags` when its row has the member, an empty list included, and no run any other new member.
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

fn tagged(id: &str, name: Option<&str>, tags: Value) -> Value {
    let mut v = match name {
        Some(n) => named(id, n, "ai"),
        None => run(id, json!({"title": "Prompt title"})),
    };
    v["tags"] = tags;
    v
}

#[tokio::test]
async fn a_run_with_tags_prints_them_in_brackets_after_the_name_and_the_first_field_is_still_the_id()
 {
    let (p, plane) = ham_runs(
        vec![json!({"runs": [tagged(R1, Some("Login redirect loop fix"), json!(["project:calc", "topic:oauth-callback"]))]})],
        &[],
    )
    .await;
    assert_eq!(p.code, 0);
    assert_eq!(
        p.out,
        format!("{R1}  {TODAY}  Login redirect loop fix  [project:calc topic:oauth-callback]\n")
    );
    assert_eq!(p.out.split(' ').next().unwrap(), R1);
    // Every byte up to the name is what the line printed before tags.
    let before = format!("{R1}  {TODAY}  Login redirect loop fix");
    assert!(p.out.starts_with(&before));
    assert_eq!(plane.calls.lock().unwrap().clone(), ["/v1/runs"]);
}

#[tokio::test]
async fn no_tags_no_bracket_and_an_old_backend_prints_todays_line() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [
            tagged(R1, Some("Login redirect loop fix"), json!([])),
            named(R2, "Order list pagination", "ai"),
            run(R3, json!({}))
        ]})],
        &[],
    )
    .await;
    let lines: Vec<&str> = p.out.lines().collect();
    assert_eq!(
        lines,
        [
            format!("{R1}  {TODAY}  Login redirect loop fix"),
            format!("{R2}  {TODAY}  Order list pagination"),
            format!("{R3}  {TODAY}"),
        ]
    );
}

#[tokio::test]
async fn tags_without_a_name_follow_the_creation_time() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [tagged(R1, None, json!(["project:calc"]))]})],
        &[],
    )
    .await;
    assert_eq!(p.out, format!("{R1}  {TODAY}  [project:calc]\n"));
    assert_eq!(p.out.split(' ').next().unwrap(), R1);
}

#[tokio::test]
async fn the_trailers_follow_the_tags() {
    let waiting = json!({"needsInput": {"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped", "expiresAt": "2026-10-20T13:00:00Z"}});
    let mut r1 = tagged(
        R1,
        Some("Fix the login redirect loop"),
        json!(["tech:go", "topic:login"]),
    );
    r1["needsInput"] = waiting["needsInput"].clone();
    let mut r2 = tagged(
        R2,
        Some("Order list pagination with cursors"),
        json!(["project:shop"]),
    );
    r2["kind"] = json!("probe");
    let mut r3 = tagged(R3, None, json!(["tech:go"]));
    r3["kind"] = json!("probe");
    let (p, _) = ham_runs(vec![json!({"runs": [r1, r2, r3]})], &["--probes"]).await;
    let lines: Vec<&str> = p.out.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "{R1}  {TODAY}  Fix the login redirect loop  [tech:go topic:login]  needs input: Cline stopped"
        )
    );
    assert_eq!(
        lines[1],
        format!("{R2}  {TODAY}  Order list pagination with cursors  [project:shop]  probe")
    );
    assert_eq!(lines[2], format!("{R3}  {TODAY}  [tech:go]  probe"));
}

#[tokio::test]
async fn an_escape_in_a_tag_is_removed_and_a_tag_that_breaks_the_grammar_is_left_out() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [tagged(
            R1,
            Some("Fix the login bug"),
            json!(["topic:ok", "topic:ev\u{1b}[31mi\u{1b}[0mdent\u{7}", "topic:two  spaces", "topic:ended early: x", "topic:ok"])
        )]})],
        &[],
    )
    .await;
    assert_eq!(
        p.out,
        format!("{R1}  {TODAY}  Fix the login bug  [topic:ok topic:evident]\n")
    );
    assert!(!p.out.contains('\u{1b}') && !p.out.contains('\u{7}'));
    // The JSON carries the same cleaned list.
    let (j, _) = ham_runs(
        vec![json!({"runs": [tagged(R1, None, json!(["topic:ok", "topic:ev\u{1b}[31mil", "bad tag"]))]})],
        &["--json"],
    )
    .await;
    let v: Value = serde_json::from_str(j.out.trim()).unwrap();
    assert_eq!(v["runs"][0]["tags"], json!(["topic:ok", "topic:evil"]));
}

#[tokio::test]
async fn json_adds_tags_only_to_a_row_that_has_the_member() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [
            tagged(R1, Some("Login redirect loop fix"), json!(["project:calc", "tech:go"])),
            tagged(R2, None, json!([])),
            run(R3, json!({}))
        ]})],
        &["--json"],
    )
    .await;
    let v: Value = serde_json::from_str(p.out.trim()).unwrap();
    let runs = v["runs"].as_array().unwrap();
    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
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
    assert_eq!(runs[0]["tags"], json!(["project:calc", "tech:go"]));
    let mut want = summary_keys(R1);
    want.extend([
        "name".to_string(),
        "nameSource".to_string(),
        "tags".to_string(),
    ]);
    want.sort();
    assert_eq!(
        keys(&runs[0]),
        want,
        "the summary's members, the name's two and tags; nothing else"
    );
    // An empty list is an answer and stays; a row without the member has none.
    assert_eq!(runs[1]["tags"], json!([]));
    assert!(runs[2].get("tags").is_none());
    assert_eq!(keys(&runs[2]), summary_keys(R3));
}

#[tokio::test]
async fn cut_on_the_first_space_still_returns_the_id_for_every_shape_of_line() {
    let (p, _) = ham_runs(
        vec![json!({"runs": [
            tagged(R1, Some("A name"), json!(["tech:go"])),
            tagged(R2, None, json!(["tech:go"])),
            tagged(R3, Some("Another"), json!([]))
        ]})],
        &[],
    )
    .await;
    let ids: Vec<&str> = p
        .out
        .lines()
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert_eq!(ids, [R1, R2, R3]);
}
