//! A newer `ham` against an older stage. `StartDirectRunRequest` of the stage as it was deployed refuses a member it does not know
//! (`deny_unknown_fields`), so a request with an `engineConfig` member meets a `400 bad-request` whose text is serde's "unknown field". `ham`
//! maps it to "this stage predates engine settings; run `ham engine <name> unset <key>` or update the stage", names the keys the user set, and
//! never retries without the member: a setting that a run does not get must not look as if it did.
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
        shown.contains("this stage predates engine settings; run `ham engine cline unset <key>` or update the stage"),
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
