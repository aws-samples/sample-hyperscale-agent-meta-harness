//! The start of a run on a second engine: the request carries the engine in its own `engine` member, the idempotency key follows it, and a
//! stage that predates the member is named and not asked again without it. The stage here is a plane that records the body it receives.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError};
use ham::http::{ControlPlane, Response, TransportError};
use ham::start::start_direct;
use ham_core::command::{Command, parse};
use ham_core::config::Config;
use ham_core::request::{DirectInputs, direct_idempotency_key_on, resolve_run_settings};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::Engine;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// What the stage of protocol 1.57.0 answers to a start request with an `engine` member: control-api wraps serde's error of its strict request
/// type as `400 bad-request`.
const OLD_STAGE_400: &str = r#"{"code":"bad-request","message":"StartDirectRunRequest: unknown field `engine`, expected one of `prompt`, `target`, `baseRef`, `model`, `rules`, `nudge`, `inputWaitS`, `skills` at line 1 column 300"}"#;
const RUN: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";

struct Stage {
    status: u16,
    body: String,
    calls: Mutex<Vec<Vec<u8>>>,
}

impl Stage {
    fn new(status: u16, body: &str) -> Arc<Stage> {
        Arc::new(Stage {
            status,
            body: body.to_string(),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn accepting() -> Arc<Stage> {
        Stage::new(201, &json!({"runId": RUN, "status": "active"}).to_string())
    }

    fn bodies(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect()
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
        self.calls.lock().unwrap().push(body);
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

/// The request `ham` builds for a start with these flags, through the one function that resolves a run (the command line, the composer and the
/// follow-up all call it).
fn request(extra: &[&str]) -> StartDirectRunRequest {
    let mut words = vec!["Add a function", "--s3-only", "--direct"];
    words.extend_from_slice(extra);
    let Command::StartDirect(args) = parse(&words).unwrap().command else {
        panic!("not a start: {words:?}");
    };
    let run = resolve_run_settings(&Config::default(), &|_| None, &args, false).unwrap();
    run.request(DirectInputs::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_a_start_on_a_second_engine_posts_the_engine_member_and_a_start_on_the_default_posts_none()
 {
    let stage = Stage::accepting();
    let api = Api::new(stage.clone());
    let default = start_direct(&api, &request(&[]), "n1").await.unwrap();
    let other = start_direct(&api, &request(&["--engine", "opencode"]), "n1")
        .await
        .unwrap();
    let bodies = stage.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[0].get("engine").is_none(),
        "the default engine sends the request of an older client: {}",
        bodies[0]
    );
    assert_eq!(bodies[1]["engine"], "opencode", "{}", bodies[1]);
    // The member is the field of the protocol type: the body the stage got reads back as that type, with the engine kept.
    let sent: StartDirectRunRequest =
        serde_json::from_slice(&stage.calls.lock().unwrap()[1]).unwrap();
    assert_eq!(sent.engine, Some(Engine::OPENCODE));
    // The engine is part of the request, so it is part of the key: one prompt sent to two engines is two starts, and the key is `ham_core`'s.
    assert_ne!(default.idempotency_key, other.idempotency_key);
    assert_eq!(
        other.idempotency_key,
        direct_idempotency_key_on(&sent, "n1", None)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_a_stage_that_predates_the_engine_member_is_named_and_the_request_is_not_sent_again_without_it()
 {
    let stage = Stage::new(400, OLD_STAGE_400);
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &request(&["--engine", "opencode"]), "n1")
        .await
        .unwrap_err();
    let shown = e.to_string();
    assert!(
        shown.contains("this stage predates the choice of an engine, so it cannot run on opencode"),
        "{shown}"
    );
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-engine-choice")
    );
    let bodies = stage.bodies();
    assert_eq!(
        bodies.len(),
        1,
        "one request, and no second one without the member"
    );
    assert_eq!(bodies[0]["engine"], "opencode");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_the_same_answer_to_a_request_on_the_default_engine_keeps_the_stage_words() {
    let stage = Stage::new(400, OLD_STAGE_400);
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &request(&[]), "n1").await.unwrap_err();
    let shown = e.to_string();
    assert!(shown.contains("unknown field `engine`"), "{shown}");
    assert!(!shown.contains("predates"), "{shown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_another_refusal_of_a_request_on_a_second_engine_keeps_the_stage_words() {
    for (status, body) in [
        (
            400,
            r#"{"code":"bad-request","message":"StartDirectRunRequest: unknown field `goal`, expected one of `prompt`"}"#,
        ),
        (
            422,
            r#"{"code":"unknown-engine","message":"engine \"opencode\" has no profile in this build"}"#,
        ),
        (
            422,
            r#"{"code":"model-not-smoked","message":"model m has no smoke record on opencode"}"#,
        ),
    ] {
        let stage = Stage::new(status, body);
        let api = Api::new(stage.clone());
        let e = start_direct(&api, &request(&["--engine", "opencode"]), "n1")
            .await
            .unwrap_err();
        assert!(!e.to_string().contains("predates"), "{body}: {e}");
        assert_eq!(e.status(), Some(status));
        assert_eq!(stage.bodies().len(), 1);
    }
}
