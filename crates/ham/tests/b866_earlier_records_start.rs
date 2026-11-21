//! The switch of the earlier run's records against a stage of an older release. `run.remove_earlier_records = false` reaches the
//! request of a follow-up (`ham continue`) as `removeEarlierRecords: false`, and a stage that predates the member refuses the whole request: its strict request
//! type answers `400 bad-request` with the fixed phrase "the body has a member this API does not know (line 1, column N)", which names
//! the place of the member and not its name. `ham` reads the place in the body it sent, finds the member there, and says in words that
//! such a stage leaves the earlier records in place anyway, with the two ways out. The request is never sent again without the member.
//! A user who keeps the default sends no member, and that request starts on such a stage. A first request never carries the member,
//! whatever the key says, so it starts on such a stage too.
//!
//! The older stage here is a plane that reads the body as the stage would: it parses the bytes with the strict request type of the protocol
//! after the member's name is replaced by a name of the same length that the type does not know, so the line and the column of its answer
//! are the parser's own numbers for that body.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError};
use ham::http::{ControlPlane, Response, TransportError};
use ham::start::start_direct;
use ham_core::command::{Command, parse};
use ham_core::config::parse_config;
use ham_core::request::{DirectInputs, resolve_run_settings};
use harness_protocol::api::v1::StartDirectRunRequest;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";
const MEMBER: &str = "\"removeEarlierRecords\"";
/// A name of the same length that the request type does not know: the stand-in for "a member this stage does not know yet".
const UNKNOWN: &str = "\"removeEarlierRecordZ\"";
const PHRASE: &str = "the body has a member this API does not know";

struct Stage {
    /// `Some`: the stage answers this, whatever the body. `None`: it reads the body as a stage of the release before the member.
    fixed: Option<(u16, String)>,
    calls: Mutex<Vec<Vec<u8>>>,
}

impl Stage {
    fn older() -> Arc<Stage> {
        Arc::new(Stage {
            fixed: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn fixed(status: u16, text: String) -> Arc<Stage> {
        Arc::new(Stage {
            fixed: Some((status, text)),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn bodies(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect()
    }

    fn refused(line: usize, column: usize) -> (u16, String) {
        (
            400,
            json!({
                "code": "bad-request",
                "message": format!("StartDirectRunRequest: {PHRASE} (line {line}, column {column})"),
            })
            .to_string(),
        )
    }

    fn as_the_older_type_reads_it(body: &[u8]) -> (u16, String) {
        let text = String::from_utf8(body.to_vec())
            .unwrap()
            .replace(MEMBER, UNKNOWN);
        match serde_json::from_str::<StartDirectRunRequest>(&text) {
            Ok(_) => (201, json!({"runId": RUN, "status": "active"}).to_string()),
            Err(e) => {
                assert!(
                    e.to_string()
                        .starts_with("unknown field `removeEarlierRecordZ`"),
                    "the stand-in refuses the one member only: {e}"
                );
                Stage::refused(e.line(), e.column())
            }
        }
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
        let (status, answer) = match &self.fixed {
            Some((status, text)) => (*status, text.clone()),
            None => Stage::as_the_older_type_reads_it(&body),
        };
        self.calls.lock().unwrap().push(body);
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: answer.into_bytes(),
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

/// The run a follow-up continues.
const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

fn built(config: &str, argv: &[&str]) -> StartDirectRunRequest {
    let args = match parse(argv).unwrap().command {
        Command::StartDirect(args) | Command::Continue(args, _) => args,
        other => panic!("not a start: {other:?}"),
    };
    let cfg = parse_config(config).unwrap();
    let run = resolve_run_settings(&cfg, &|_| None, &args, false).unwrap();
    run.request(DirectInputs::default()).unwrap()
}

/// The request `ham continue <run> '<text>'` builds with this config file, through the one function that resolves a run: the request
/// the switch rides in.
fn request(config: &str) -> StartDirectRunRequest {
    let request = built(
        config,
        &["continue", SOURCE, "Now make it upper case", "--s3-only"],
    );
    assert!(
        request.continue_from.is_some(),
        "TRIGGER PRESENT: a follow-up"
    );
    request
}

/// A first request with this config file.
fn first_request(config: &str) -> StartDirectRunRequest {
    built(config, &["Add a health check", "--s3-only", "--direct"])
}

const OFF: &str = "configVersion = 1\n[run]\nremove_earlier_records = false\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b866_an_older_stage_is_named_in_words() {
    let stage = Stage::older();
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &request(OFF), "n1").await.unwrap_err();
    let bodies = stage.bodies();
    assert_eq!(
        bodies[0]["removeEarlierRecords"], false,
        "TRIGGER PRESENT: the request carried the switch: {}",
        bodies[0]
    );
    let shown = e.to_string();
    for words in [
        "this stage predates the switch for the earlier run's records",
        "so it leaves them in place anyway",
        "`ham config unset run.remove_earlier_records`",
        "or update the stage (run ham update; ask the person who runs the stage if that is not you)",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
    assert!(
        !shown.contains("removeEarlierRecords"),
        "the member's name is no word for the user: {shown}"
    );
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-remove-earlier-records"),
        "{e:?}"
    );
    assert_eq!(
        bodies.len(),
        1,
        "the request is not sent again without the member"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b866_a_user_who_keeps_the_default_starts_on_a_stage_that_predates_the_switch() {
    for config in [
        "configVersion = 1\n",
        "configVersion = 1\n[run]\nremove_earlier_records = true\n",
    ] {
        let stage = Stage::older();
        let api = Api::new(stage.clone());
        let started = start_direct(&api, &request(config), "n2")
            .await
            .unwrap_or_else(|e| panic!("{config:?}: the stage starts the run: {e}"));
        assert_eq!(started.run_id, RUN);
        assert!(
            stage.bodies()[0].get("removeEarlierRecords").is_none(),
            "{config:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b866_r2_a_first_request_with_the_switch_off_starts_on_a_stage_that_predates_it() {
    // TRIGGER PRESENT: the same file makes a follow-up carry the member, which such a stage refuses.
    assert_eq!(request(OFF).remove_earlier_records, Some(false));
    let stage = Stage::older();
    let api = Api::new(stage.clone());
    let first = first_request(OFF);
    assert!(first.continue_from.is_none());
    let started = start_direct(&api, &first, "n5").await.unwrap_or_else(|e| {
        panic!("a first request starts on a stage that predates the switch: {e}")
    });
    assert_eq!(started.run_id, RUN);
    let bodies = stage.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].get("removeEarlierRecords").is_none(),
        "the member means nothing in a first request and is not sent: {}",
        bodies[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b866_the_serde_words_of_a_stage_before_the_fixed_phrases_are_mapped_too() {
    let stage = Stage::fixed(
        400,
        json!({"code": "bad-request", "message": "StartDirectRunRequest: unknown field `removeEarlierRecords`, expected one of `prompt`, `target` at line 1 column 90"}).to_string(),
    );
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &request(OFF), "n3").await.unwrap_err();
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-remove-earlier-records"),
        "{e:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b866_another_refusal_keeps_the_words_of_the_stage() {
    // The same phrase at a place that is not the member, and another 400 altogether.
    for (status, text) in [
        Stage::refused(1, 3),
        (
            400,
            json!({"code": "invalid-rules", "message": "rules: a name is not allowed"}).to_string(),
        ),
    ] {
        let stage = Stage::fixed(status, text.clone());
        let api = Api::new(stage.clone());
        let e = start_direct(&api, &request(OFF), "n4").await.unwrap_err();
        assert!(
            !matches!(&e, ApiError::Http { error, .. } if error.code == "stage-predates-remove-earlier-records"),
            "{text}: {e:?}"
        );
    }
}
