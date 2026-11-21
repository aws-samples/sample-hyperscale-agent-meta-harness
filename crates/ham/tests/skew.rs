//! A newer `ham` against an older stage. `StartDirectRunRequest` of the stage as it was deployed refuses a member it does not know
//! (`deny_unknown_fields`), so a request with an `engineConfig` member meets a `400 bad-request` whose text is serde's "unknown field". `ham`
//! maps it to "this stage predates engine settings; run `ham engine <name> unset <key>` or update the stage", names the keys the user set, and
//! never retries without the member: a setting that a run does not get must not look as if it did.
//!
//! A newer `ham` against a stage whose download route predates copies that need the run's base. Such a route ignores the
//! `parts` member of the request and signs whatever the worker saved, with no word about a base. A copy that holds the whole history
//! downloads as it always did. A copy that holds what the run changed (a worker newer than its route) is kept as it came, and the
//! text says what it needs in place of a command that would fail.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::command::{Command, parse};
use ham_core::engine_file;
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_run_request};
use harness_protocol::api::v1::StartDirectRunRequest;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// What the stage of protocol 1.38.0 answers to a start request with an `engineConfig` member: control-api wraps serde's error of
/// `StartDirectRunRequest` as `400 bad-request`. Captures the text from the deployed stage again.
const RC4_400: &str = r#"{"code":"bad-request","message":"StartDirectRunRequest: unknown field `engineConfig`, expected one of `prompt`, `target`, `baseRef`, `model`, `rules`, `nudge`, `inputWaitS`, `skills`, `continueFrom`, `research`, `reportTests`, `goal`, `thirdParty`, `attachments` at line 1 column 234"}"#;

struct Stage {
    status: u16,
    body: String,
    calls: Mutex<Vec<Value>>,
}

impl Stage {
    fn new(status: u16, body: &str) -> Arc<Stage> {
        Arc::new(Stage {
            status,
            body: body.to_string(),
            calls: Mutex::new(Vec::new()),
        })
    }
}

impl ControlPlane for Stage {
    async fn call(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&body).unwrap());
        Ok(Response {
            status: self.status,
            headers: BTreeMap::new(),
            body: self.body.clone().into_bytes(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("not used".into()))
    }
}

fn request(engine_file_text: Option<&str>) -> StartDirectRunRequest {
    let Command::StartDirect(args) = parse(&["Add a function", "--s3-only", "--direct"])
        .unwrap()
        .command
    else {
        panic!("not a start");
    };
    let inputs = DirectInputs {
        engine_config: engine_file_text.and_then(|t| engine_file::read_text(t, "cline").config),
        ..DirectInputs::default()
    };
    direct_run_request(&args, inputs, &ModelPrefs::default()).unwrap()
}

const SET: &str =
    "schema = 1\n[reasoning]\neffort = \"low\"\n[tools]\ndisable = [\"spawn_agent\"]\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_stage_400_becomes_the_stage_predates_message_and_no_retry_without_the_member() {
    let stage = Stage::new(400, RC4_400);
    let api = Api::new(stage.clone());
    let e = api
        .start_direct_run(&request(Some(SET)), "key-1")
        .await
        .unwrap_err();
    let shown = e.to_string();
    assert!(
        shown.contains("this stage predates engine settings; run `ham engine cline unset <key>` or update the stage (run ham update; ask the person who runs the stage if that is not you) (keys set: "),
        "{shown}"
    );
    // The keys the user set are named, so the person knows what to unset.
    assert!(
        shown.contains("reasoning.effort") && shown.contains("tools.disable"),
        "{shown}"
    );
    assert_eq!(e.status(), Some(400));
    // One request, and it still carried the member: the client never strips a setting to get a run started.
    let calls = stage.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "the request count is 1");
    assert!(calls[0].get("engineConfig").is_some(), "{}", calls[0]);
    println!("SKEW TRIGGER PRESENT: 400 body mapped, 1 request, member kept: {shown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_without_the_member_meeting_the_same_answer_keeps_the_stage_words() {
    let stage = Stage::new(400, RC4_400);
    let api = Api::new(stage.clone());
    let e = api
        .start_direct_run(&request(None), "key-2")
        .await
        .unwrap_err();
    let shown = e.to_string();
    assert!(shown.contains("unknown field `engineConfig`"), "{shown}");
    assert!(!shown.contains("predates"), "{shown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_400_with_the_member_present_keeps_the_stage_words() {
    for body in [
        r#"{"code":"bad-request","message":"StartDirectRunRequest: unknown field `goal`, expected one of `prompt`"}"#,
        r#"{"code":"invalid-engine-config","message":"engine config: key \"aws.profile\" is not allowed","problem":"denied_key"}"#,
        r#"{"code":"bad-request","message":"prompt is empty"}"#,
    ] {
        let stage = Stage::new(400, body);
        let api = Api::new(stage.clone());
        let e = api
            .start_direct_run(&request(Some(SET)), "key-3")
            .await
            .unwrap_err();
        assert!(!e.to_string().contains("predates"), "{body}: {e}");
        assert!(matches!(e, ApiError::Http { status: 400, .. }));
        assert_eq!(stage.calls.lock().unwrap().len(), 1);
    }
}

// ----: the download route of an older stage ---------------------------------------------------------------------------------

const COPY_RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const COPY_HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const COPY_BASE: &str = "1111111111111111111111111111111111111111";

/// A stage whose download route predates the member `parts`: it answers with the copy the worker saved and no `base`, whatever the
/// request says, and serves the object in ranges as the store does.
struct OlderRoute {
    object: Vec<u8>,
    paths: Mutex<Vec<String>>,
}

impl ControlPlane for OlderRoute {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.paths.lock().unwrap().push(path.to_string());
        let answer = serde_json::json!({
            "url": "https://bucket.s3.us-east-1.amazonaws.com/runs/x/agents/direct/turns/1/snapshot.bundle?X-Amz-Signature=deadbeef",
            "expiresAt": "2099-10-15T13:25:00Z", "headSha": COPY_HEAD, "baseSha": COPY_BASE, "takenAt": "2026-11-17T13:10:00Z",
            "count": 3, "sizeBytes": self.object.len(), "etag": "\"abc\"", "runStatus": "active"
        });
        Ok(Response {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&answer).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        let size = self.object.len() as u64;
        let (s, e) = range.unwrap_or((0, size - 1));
        let e = e.min(size - 1);
        Ok(Response {
            status: 206,
            headers: BTreeMap::from([(
                "content-range".to_string(),
                format!("bytes {s}-{e}/{size}"),
            )]),
            body: self.object[usize::try_from(s).unwrap()..=usize::try_from(e).unwrap()].to_vec(),
        })
    }
}

fn copy_bytes(needs_base: bool) -> Vec<u8> {
    let needs = if needs_base {
        format!("-{COPY_BASE} harness: base\n")
    } else {
        String::new()
    };
    let mut b =
        format!("# v2 git bundle\n{needs}{COPY_HEAD} refs/heads/snapshot\n\nPACK").into_bytes();
    b.extend((0..4_000).map(|i| (i % 251) as u8));
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b860_pull_says_when_a_stage_sent_no_base() {
    let object = copy_bytes(true);
    let stage = Arc::new(OlderRoute {
        object: object.clone(),
        paths: Mutex::new(Vec::new()),
    });
    let dir = support::scratch("b860-skew-no-base");
    let got = ham::chat_routes::fetch_snapshot(&Api::new(stage.clone()), COPY_RUN, &dir)
        .await
        .unwrap();
    // The client asked for the copy in parts; the stage ignored it and sent a copy that needs the base without one.
    let paths = stage.paths.lock().unwrap().clone();
    assert_eq!(
        paths,
        vec![format!("/v1/runs/{COPY_RUN}/snapshot/download-url?parts=1")]
    );
    println!(
        "SKEW TRIGGER PRESENT: the stage ignored parts=1 and sent a copy whose header names a commit it needs, with no base"
    );
    // The file is kept as it came, under its usual name.
    let path = dir.join(format!("{COPY_RUN}-snapshot-3.bundle"));
    assert_eq!(got.path, path);
    assert_eq!(std::fs::read(&path).unwrap(), object);
    let left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec![format!("{COPY_RUN}-snapshot-3.bundle")]);
    // The text says what the copy needs, word for word, and prints no line that would fail.
    let text = ham::snapshot_cmd::text(COPY_RUN, &got);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.contains(
            &"Note      this copy holds what the run changed and needs the repository the run started from, which this stage did not send; see \"Pull the work in progress\" in the user guide"
        ),
        "{text}"
    );
    assert!(!lines.iter().any(|l| l.starts_with("Look ")), "{text}");
    assert!(!lines.iter().any(|l| l.starts_with("Needs ")), "{text}");
    assert!(
        lines.contains(&format!("Size      {} bytes", object.len()).as_str()),
        "{text}"
    );
    let v = ham::snapshot_cmd::json_of(COPY_RUN, &got);
    assert_eq!(v["snapshot"]["needsBase"], true, "{v}");
    assert!(v["snapshot"].get("base").is_none(), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b860_pull_from_an_older_stage_is_unchanged() {
    let object = copy_bytes(false);
    let stage = Arc::new(OlderRoute {
        object: object.clone(),
        paths: Mutex::new(Vec::new()),
    });
    let dir = support::scratch("b860-skew-whole");
    let got = ham::chat_routes::fetch_snapshot(&Api::new(stage.clone()), COPY_RUN, &dir)
        .await
        .unwrap();
    println!(
        "SKEW TRIGGER PRESENT: the stage ignored parts=1 and sent a copy that holds the whole history"
    );
    let path = dir.join(format!("{COPY_RUN}-snapshot-3.bundle"));
    assert_eq!(got.path, path);
    assert_eq!(std::fs::read(&path).unwrap(), object);
    // The text and the JSON object of today.
    let text = ham::snapshot_cmd::text(COPY_RUN, &got);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.contains(
            &format!("Look      git clone -b snapshot {} <dir>", path.display()).as_str()
        ),
        "{text}"
    );
    assert!(
        lines.contains(&format!("Size      {} bytes", object.len()).as_str()),
        "{text}"
    );
    assert!(
        !text.contains("the repository the run started from"),
        "{text}"
    );
    let v = ham::snapshot_cmd::json_of(COPY_RUN, &got);
    assert!(v["snapshot"].get("needsBase").is_none(), "{v}");
    assert!(v["snapshot"].get("base").is_none(), "{v}");
}
