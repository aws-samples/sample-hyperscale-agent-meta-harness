//! Golden event streams of the direct driver: the record of what a run says and does, taken BEFORE the module
//! seam (`direct/hooks.rs`) existed and kept byte for byte afterwards. Each scenario runs the whole agent in process against the fake
//! AWS and the fake ACP engine and compares one normalized document with `tests/golden/<name>.json`:
//!
//! - `wire`: every line of the fake engine's own log, i.e. what agentd wrote to the engine and what the engine answered, in order;
//! - `events`: every agent event agentd posted, in posting order (type and payload);
//! - `complete`: the `turn-complete` request; `summary`: the uploaded `summary.json`; `prompts`: the text of each `session/prompt`;
//! - `agentd_log`: the lines agentd logged (with their numbers normalized).
//!
//! Normalization removes only what differs between two runs of the same code: event ids and ULIDs, timestamps, git object ids,
//! scratch paths and every number whose key names a duration in seconds or milliseconds (`waitS`, `backoffS`, `runS`, ...) or a byte size (`promptBytes`, `bundleBytes`, ...). Order,
//! counts, kinds, texts and every other value stay.
//!
//! A change to the golden files is a change of the driver's observable behaviour and needs a reason in its commit. To record them:
//! `GOLDEN_UPDATE=1 cargo nextest run -p agentd --test direct_golden` (on a build host, from the tree whose behaviour is the baseline).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use direct_support::{Answer, CfgEdit, Direct, Opts, sha};
use harness_protocol::api::v1::ContinueSettings;
use harness_protocol::common::Protocol;
use harness_protocol::direct::{DirectModelSpec, InstructAction};
use harness_protocol::envelope::EventType;
use regex::Regex;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

const OPUS_GLOBAL: &str = "global.anthropic.claude-opus-5-5";
const FRESH: &str = "fresh_session_done";

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"))
}

/// True for a key whose number differs between two runs of the same code: a duration, a clock reading, or the size of a text that
/// holds a scratch path.
fn is_variable_key(key: &str) -> bool {
    key.ends_with('S')
        || key.ends_with("Bytes")
        || key.ends_with("Ms")
        || key.ends_with("_s")
        || key.ends_with("_ms")
        || matches!(key, "ms" | "seq" | "bytes")
}

struct Norm {
    ulid: Regex,
    time: Regex,
    sha40: Regex,
    sha64: Regex,
    dir: String,
}

impl Norm {
    fn new(dir: &str) -> Self {
        Self {
            ulid: Regex::new(r"\b[0-9A-HJKMNP-TV-Z]{26}\b").unwrap(),
            time: Regex::new(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z").unwrap(),
            sha40: Regex::new(r"\b[0-9a-f]{40}\b").unwrap(),
            sha64: Regex::new(r"\b[0-9a-f]{64}\b").unwrap(),
            dir: dir.to_string(),
        }
    }

    fn text(&self, s: &str) -> String {
        let s = s.replace(&self.dir, "<dir>");
        let s = self.time.replace_all(&s, "<time>");
        let s = self.sha64.replace_all(&s, "<sha256>");
        let s = self.sha40.replace_all(&s, "<sha>");
        // The run id of the test harness is a 26-character ULID-shaped constant; it is kept readable.
        self.ulid
            .replace_all(&s, |c: &regex::Captures| {
                if &c[0] == direct_support::RUN {
                    "<run>".to_string()
                } else {
                    "<ulid>".to_string()
                }
            })
            .to_string()
    }

    /// A log line: texts normalized, and every number that follows a word like `in`, `after` or `for` left alone except those that
    /// are durations in seconds written as `<n> s`.
    fn log_line(&self, s: &str) -> String {
        let t = self.text(s);
        let secs = Regex::new(r"\b\d+(\.\d+)? ?(s|ms)\b").unwrap();
        let t = secs.replace_all(&t, "<n>s").to_string();
        // counts of dropped or buffered bytes and pids vary; the test only needs the shape of the line
        let pid = Regex::new(r"\b(pid|owner_uid)[= ]\d+").unwrap();
        pid.replace_all(&t, "$1=<n>").to_string()
    }

    fn value(&self, key: &str, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Number(_) if is_variable_key(key) => json!("<n>"),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(key, x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (self.text(k), self.value(k, x)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// The document one scenario produced.
fn document(d: &Direct) -> Value {
    let n = Norm::new(d.dir.to_str().unwrap());
    // What the fake engine saw and said, one entry per line of its log; the parsed JSON when the line carries it.
    let wire: Vec<Value> = d
        .fake_engine_log()
        .lines()
        .map(|l| {
            for dir in ["in ", "out "] {
                if let Some(j) = l.strip_prefix(dir)
                    && let Ok(v) = serde_json::from_str::<Value>(j)
                {
                    return json!({ dir.trim(): n.value("", &v) });
                }
            }
            json!(n.log_line(l))
        })
        .collect();
    let events: Vec<Value> = d
        .aws
        .events()
        .into_iter()
        .map(|(t, v)| json!({ "type": serde_json::to_value(t).unwrap(), "payload": n.value("", &v) }))
        .collect();
    let complete: Vec<Value> = d
        .aws
        .complete()
        .iter()
        .map(|c| n.value("", &serde_json::to_value(c).unwrap()))
        .collect();
    let summary = d
        .aws
        .get(&harness_protocol::direct::s3::summary_key(
            direct_support::RUN,
        ))
        .map(|b| serde_json::from_slice::<Value>(&b).unwrap())
        .map(|v| n.value("", &v));
    let log: Vec<String> = d
        .log
        .lines()
        .iter()
        .map(|l| n.log_line(l))
        // Lines of the test harness itself (server start, listener) and nothing else are left out.
        .collect();
    json!({
        "wire": wire,
        "events": events,
        "complete": complete,
        "summary": summary,
        "prompts": d.prompts_received().iter().map(|p| n.text(p)).collect::<Vec<_>>(),
        "agentd_log": log,
    })
}

/// The value with the keys of every object in sorted order. `serde_json` keeps keys sorted unless some crate of the build turns on
/// `preserve_order`, which a workspace build does, and then it keeps insertion order. The
/// files must read the same either way.
fn sorted(v: Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut entries: Vec<(String, Value)> = o.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, x)| (k, sorted(x))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sorted).collect()),
        other => other,
    }
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(&sorted(v.clone())).unwrap();
    s.push('\n');
    s
}

/// Compares the scenario with its golden file, or writes the file when `GOLDEN_UPDATE` is set.
fn check(name: &str, d: &Direct) {
    let got = pretty(&document(d));
    let path = golden_path(name);
    if std::env::var_os("GOLDEN_UPDATE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no golden file {}: {e}", path.display()));
    if got != want {
        // The first differing line is the quickest way to the cause; the whole new document is written next to the scratch dir.
        let out = d.dir.join(format!("{name}.actual.json"));
        std::fs::write(&out, &got).unwrap();
        let first = got
            .lines()
            .zip(want.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| got.lines().count().min(want.lines().count()));
        panic!(
            "golden {name} differs at line {} (actual document: {})\n  golden: {:?}\n  actual: {:?}",
            first + 1,
            out.display(),
            want.lines().nth(first),
            got.lines().nth(first)
        );
    }
}

async fn run(d: &Direct) {
    d.invoke().await;
    d.wait_healthy().await;
}

fn route_spec(id: &str) -> DirectModelSpec {
    DirectModelSpec {
        id: id.into(),
        region: "us-east-1".into(),
        protocol: Protocol::Converse,
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
        default_reasoning_effort: None,
    }
}

fn quick(more: impl FnOnce(&mut Config) + 'static) -> Option<CfgEdit> {
    Some(Box::new(move |c: &mut Config| {
        c.direct.provider_retries = 3;
        c.direct.engine_stop_resumes = 1;
        c.direct.provider_retry_pause = Duration::from_millis(20);
        c.direct.failover_wait = Duration::from_millis(100);
        c.direct.failover_wait_cap = Duration::from_millis(200);
        c.direct.failover_probe = false;
        c.direct.input_margin = Duration::ZERO;
        c.direct.input_min_window = Duration::from_secs(1);
        more(c);
    }))
}

// ---- the scenarios ------------------------------------------------------------------------------------------------------------

/// The plain run: one prompt, tool calls, files written, reported done, everything delivered.
#[tokio::test(flavor = "multi_thread")]
async fn golden_reported_done() {
    let d = Direct::start(Opts::default()).await;
    run(&d).await;
    check("reported_done", &d);
}

/// The optional nudge: a second prompt after an end_turn that did not say DONE.
#[tokio::test(flavor = "multi_thread")]
async fn golden_nudge() {
    let text = "Re-read my request. Reply DONE if it is all done.";
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        nudge: Some(ContinueSettings {
            text: text.into(),
            sha256: sha(text.as_bytes()),
            max_nudges: 3,
        }),
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("nudge", &d);
}

/// The provider ladder: three resumes on the model, a switch to the other profile, one more resume, done.
#[tokio::test(flavor = "multi_thread")]
async fn golden_retry_and_switch() {
    let d = Direct::start(Opts {
        scenario: "provider_down_then_done",
        edit_cfg: quick(|_| {}),
        failover: vec![route_spec(OPUS_GLOBAL)],
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("retry_and_switch", &d);
}

/// The engine's own brake: the loop guard stops the prompt `cancelled`, the harness resumes once on the same route and model.
#[tokio::test(flavor = "multi_thread")]
async fn golden_engine_stop() {
    let d = Direct::start(Opts {
        scenario: "loop_guard_always",
        edit_cfg: quick(|_| {}),
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("engine_stop", &d);
}

/// The user's cancel (`ham stop`) while the prompt is open.
#[tokio::test(flavor = "multi_thread")]
async fn golden_user_cancel() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    d.aws.cancel_on(EventType::TurnStarted);
    run(&d).await;
    check("user_cancel", &d);
}

/// A give-up, a question to the user, the user's words in the same session, the work done.
#[tokio::test(flavor = "multi_thread")]
async fn golden_question_and_message() {
    let d = Direct::start(Opts {
        scenario: "gave_up_then_done",
        input_wait_s: Some(30),
        edit_cfg: quick(|_| {}),
        ..Opts::default()
    })
    .await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::ContinueWithPrompt)
            .text("The tests contradict nothing: add the function and fix the test."),
    );
    run(&d).await;
    check("question_and_message", &d);
}

/// A loop-guard stop, a question, the user's request for a fresh session, the new session finishing the work.
#[tokio::test(flavor = "multi_thread")]
async fn golden_question_and_fresh_session() {
    let d = Direct::start(Opts {
        scenario: "stuck_engine_stop_then_done",
        engine_env: vec![
            ("FAKE_ACP_NEW_SESSION_SCENARIO", FRESH.into()),
            ("FAKE_ACP_RESTART_SCENARIO", FRESH.into()),
        ],
        input_wait_s: Some(30),
        edit_cfg: quick(|_| {}),
        ..Opts::default()
    })
    .await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt).text("Poll the log file instead."),
    );
    run(&d).await;
    check("question_and_fresh_session", &d);
}

/// A failure before the prompt: the request on S3 is not the one the invocation hashed.
#[tokio::test(flavor = "multi_thread")]
async fn golden_setup_failed() {
    let d = Direct::start(Opts {
        tamper_request: true,
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("setup_failed", &d);
}

/// A JSON-RPC error answering the prompt, with the provider retry off: the provider failing.
#[tokio::test(flavor = "multi_thread")]
async fn golden_rpc_error() {
    let d = Direct::start(Opts {
        scenario: "rpc_error",
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("rpc_error", &d);
}

/// The model's refusal.
#[tokio::test(flavor = "multi_thread")]
async fn golden_refusal() {
    let d = Direct::start(Opts {
        scenario: "refusal",
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("refusal", &d);
}

/// An engine that exits before it answers `initialize`.
#[tokio::test(flavor = "multi_thread")]
async fn golden_engine_exits_at_start() {
    let d = Direct::start(Opts {
        rules: vec![],
        engine_env: vec![("WRAP_EXIT", "7".into())],
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("engine_exits_at_start", &d);
}

/// The deadline while the prompt is held open.
#[tokio::test(flavor = "multi_thread")]
async fn golden_deadline() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 15,
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("deadline", &d);
}

/// Silence past the provider limit: the cancel, the one resume of the session, the second silence.
#[tokio::test(flavor = "multi_thread")]
async fn golden_silence() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3600".into())],
        provider_silence_s: Some(1),
        ..Opts::default()
    })
    .await;
    run(&d).await;
    check("silence", &d);
}
