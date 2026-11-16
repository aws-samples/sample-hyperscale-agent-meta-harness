//! The copy interval of a run against a stage of an older release. The start request carries `run.copy_every_minutes` as `snapshotEveryS`,
//! and a stage that predates the member refuses the whole request: its strict request type answers `400 bad-request` with the fixed phrase
//! "the body has a member this API does not know (line 1, column N)", which names the place of the member and not its name. Two things keep a user of
//! a newer client on such a stage working. A key set to the default sends no member, so that request is the one an older client sends and the
//! stage starts it. And for a key set to another value `ham` reads the place the stage named in the body it sent, finds the member there, and
//! names the key and the two ways out; the request is never sent again without the member.
//!
//! The older stage here is a plane that reads the body as the stage would: it parses the bytes with the strict request type of the protocol after the
//! member's name is replaced by a name of the same length that the type does not know, so the line and the column of its answer are the parser's own
//! numbers for that body.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError};
use ham::http::{ControlPlane, Response, TransportError};
use ham::start::{start_direct, start_direct_set_by};
use ham_core::command::{Command, parse};
use ham_core::config::parse_config;
use ham_core::request::{DirectInputs, resolve_run_settings};
use ham_core::settings::Key;
use harness_protocol::api::v1::StartDirectRunRequest;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";
const MEMBER: &str = "\"snapshotEveryS\"";
/// A name of the same length that the request type does not know: the stand-in for "a member this stage does not know yet".
const UNKNOWN: &str = "\"snapshotEveryZ\"";
const PHRASE: &str = "the body has a member this API does not know";

/// What the stage answers.
enum Answer {
    /// A stage of the release before the member: the request type without it.
    Older,
    /// A stage that answers the fixed phrase with this column, whatever the body.
    PhraseAt(usize),
    /// A fixed answer.
    Fixed(u16, &'static str),
}

struct Stage {
    answer: Answer,
    calls: Mutex<Vec<Vec<u8>>>,
}

impl Stage {
    fn new(answer: Answer) -> Arc<Stage> {
        Arc::new(Stage {
            answer,
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

    fn accepted() -> (u16, String) {
        (201, json!({"runId": RUN, "status": "active"}).to_string())
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

    fn older(body: &[u8]) -> (u16, String) {
        let text = String::from_utf8(body.to_vec()).unwrap();
        let as_the_older_type_reads_it = text.replace(MEMBER, UNKNOWN);
        match serde_json::from_str::<StartDirectRunRequest>(&as_the_older_type_reads_it) {
            Ok(_) => Stage::accepted(),
            Err(e) => {
                assert!(
                    e.to_string().starts_with("unknown field `snapshotEveryZ`"),
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
        let (status, answer) = match &self.answer {
            Answer::Older => Stage::older(&body),
            Answer::PhraseAt(column) => Stage::refused(1, *column),
            Answer::Fixed(status, text) => (*status, (*text).to_string()),
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

/// The request `ham` builds for a start with this config file, through the one function that resolves a run.
fn request(config: &str) -> StartDirectRunRequest {
    let Command::StartDirect(args) = parse(&["Add a function", "--s3-only", "--direct"])
        .unwrap()
        .command
    else {
        panic!("not a start");
    };
    let cfg = parse_config(config).unwrap();
    let run = resolve_run_settings(&cfg, &|_| None, &args, false).unwrap();
    run.request(DirectInputs::default()).unwrap()
}

/// The request of a start with this config file and the key of the file that set its copy interval, as the command line and the chat hand
/// them to the start.
fn resolved(config: &str) -> (StartDirectRunRequest, Option<Key>) {
    let Command::StartDirect(args) = parse(&["Add a function", "--s3-only", "--direct"])
        .unwrap()
        .command
    else {
        panic!("not a start");
    };
    let cfg = parse_config(config).unwrap();
    let run = resolve_run_settings(&cfg, &|_| None, &args, false).unwrap();
    let key = run.copy_interval_key();
    (run.request(DirectInputs::default()).unwrap(), key)
}

fn with_minutes(minutes: u32) -> StartDirectRunRequest {
    request(&format!(
        "configVersion = 1\n[run]\ncopy_every_minutes = {minutes}\n"
    ))
}

/// The request of a config that sets `run.copy_every_seconds`.
fn with_seconds(seconds: u32) -> StartDirectRunRequest {
    request(&format!(
        "configVersion = 1\n[run]\ncopy_every_seconds = {seconds}\n"
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b823_seconds_a_stage_whose_interval_starts_at_one_minute_is_named_with_the_seconds_key() {
    // A stage of the release before the 15 seconds knows the member and refuses a number below 60 with these words of its control-api.
    let stage = Stage::new(Answer::Fixed(
        400,
        r#"{"code":"bad-request","message":"snapshotEveryS is 30 seconds; it must be 60 to 3600 (leaving it out means 60). The copies of a run's work cannot be switched off"}"#,
    ));
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &with_seconds(30), "n1")
        .await
        .unwrap_err();
    let bodies = stage.bodies();
    assert_eq!(
        bodies[0]["snapshotEveryS"], 30,
        "TRIGGER PRESENT: the request carried 30 seconds: {}",
        bodies[0]
    );
    let shown = e.to_string();
    for words in [
        "this stage predates copy intervals under one minute",
        "run.copy_every_seconds = 30",
        "set it to 60 or more",
        "`ham config unset run.copy_every_seconds`",
        "or update the stage",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
    assert!(
        !shown.contains("snapshotEveryS"),
        "the member's name is no word for the user: {shown}"
    );
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-copy-interval"),
        "{e:?}"
    );
    assert_eq!(
        bodies.len(),
        1,
        "one request, and no second one with another interval"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b823_seconds_the_refusal_of_a_whole_minute_keeps_the_words_of_the_stage() {
    // The same answer for a number the older stage takes cannot come from that stage: the words stay the stage's.
    let stage = Stage::new(Answer::Fixed(
        400,
        r#"{"code":"bad-request","message":"snapshotEveryS is 30 seconds; it must be 60 to 3600 (leaving it out means 60). The copies of a run's work cannot be switched off"}"#,
    ));
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &with_minutes(10), "n1")
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "bad-request"),
        "{e:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b823_seconds_a_stage_that_predates_the_member_is_named_with_the_seconds_key() {
    let stage = Stage::new(Answer::Older);
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &with_seconds(45), "n1")
        .await
        .unwrap_err();
    let shown = e.to_string();
    for words in [
        "this stage predates the copy interval of a run",
        "run.copy_every_seconds = 45",
        "`ham config unset run.copy_every_seconds`",
        "or update the stage",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
    assert!(
        !shown.contains("run.copy_every_minutes"),
        "45 seconds are no whole minutes: only the seconds key can have set them: {shown}"
    );
    assert_eq!(stage.bodies().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b841_copy_a_stage_that_predates_the_member_is_named_with_the_key_and_the_two_ways_out() {
    let stage = Stage::new(Answer::Older);
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &with_minutes(10), "n1")
        .await
        .unwrap_err();
    let bodies = stage.bodies();
    assert_eq!(
        bodies[0]["snapshotEveryS"], 600,
        "TRIGGER PRESENT: the request carried the member, and the stage of the older release refused it: {}",
        bodies[0]
    );
    let shown = e.to_string();
    for words in [
        "this stage predates the copy interval of a run",
        "run.copy_every_minutes = 10",
        "`ham config unset run.copy_every_minutes`",
        "or update the stage",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
    assert!(
        !shown.contains(PHRASE),
        "the words of the stage, which name neither the member nor the key, are replaced: {shown}"
    );
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-copy-interval"),
        "{e:?}"
    );
    assert_eq!(
        bodies.len(),
        1,
        "one request, and no second one without the member: an interval the run did not get must not look as if it did"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b841_copy_a_key_set_to_the_default_starts_on_a_stage_that_predates_the_member() {
    let stage = Stage::new(Answer::Older);
    let api = Api::new(stage.clone());
    // The default is 15 seconds, the default of `run.copy_every_seconds`.
    let started = start_direct(&api, &with_seconds(15), "n1").await;
    let bodies = stage.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].get("snapshotEveryS").is_none(),
        "the default is what the stage does by itself, so the request is the one an older client sends: {}",
        bodies[0]
    );
    assert!(
        started.is_ok(),
        "the stage of the older release starts it: {:?}",
        started.err()
    );
    // An unset key is the same request.
    let unset = request("configVersion = 1\n");
    assert_eq!(
        serde_json::to_value(&unset).unwrap(),
        serde_json::to_value(with_seconds(15)).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b841_copy_the_serde_words_of_a_stage_before_the_fixed_phrases_are_mapped_too() {
    let stage = Stage::new(Answer::Fixed(
        400,
        r#"{"code":"bad-request","message":"StartDirectRunRequest: unknown field `snapshotEveryS`, expected one of `prompt`, `target` at line 1 column 90"}"#,
    ));
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &with_minutes(10), "n1")
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-copy-interval"),
        "{e:?}"
    );
    assert_eq!(stage.bodies().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b841_copy_the_same_phrase_for_another_member_keeps_the_words_of_the_stage() {
    // The place the stage names is the end of another member's name: the stage does not know that one, and the copy interval is not the cause.
    let sent = serde_json::to_string(&with_minutes(10)).unwrap();
    let prompt_ends = sent.find("\"prompt\"").unwrap() + "\"prompt\"".len();
    for column in [prompt_ends, 1, 2, sent.len(), sent.len() + 40, 0] {
        let stage = Stage::new(Answer::PhraseAt(column));
        let api = Api::new(stage.clone());
        let e = start_direct(&api, &with_minutes(10), "n1")
            .await
            .unwrap_err();
        let shown = e.to_string();
        assert!(shown.contains(PHRASE), "column {column}: {shown}");
        assert!(!shown.contains("predates"), "column {column}: {shown}");
        assert_eq!(stage.bodies().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b841_copy_another_refusal_and_a_request_without_the_member_keep_the_words_of_the_stage() {
    // A request that carries the member and is refused for something else.
    for (status, body) in [
        (
            400,
            r#"{"code":"bad-request","message":"snapshotEveryS is 30 seconds; it must be 60 to 3600 (leaving it out means 60). The copies of a run's work cannot be switched off"}"#,
        ),
        (
            400,
            r#"{"code":"bad-request","message":"StartDirectRunRequest: a required member is missing (line 1, column 40)"}"#,
        ),
        (
            422,
            r#"{"code":"model-not-smoked","message":"model m has no smoke record"}"#,
        ),
    ] {
        let stage = Stage::new(Answer::Fixed(status, body));
        let api = Api::new(stage.clone());
        let e = start_direct(&api, &with_minutes(10), "n1")
            .await
            .unwrap_err();
        assert!(!e.to_string().contains("predates"), "{body}: {e}");
        assert_eq!(e.status(), Some(status));
        assert_eq!(stage.bodies().len(), 1);
    }
    // A request without the member that gets the phrase: the copy interval is not in it, so it is not named.
    let sent = serde_json::to_string(&request("configVersion = 1\n")).unwrap();
    let stage = Stage::new(Answer::PhraseAt(sent.len() - 1));
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &request("configVersion = 1\n"), "n1")
        .await
        .unwrap_err();
    assert!(e.to_string().contains(PHRASE), "{e}");
    assert!(!e.to_string().contains("predates"), "{e}");
}

// ----: the sentence names the key the config sets --------------------------------------------------------------------------------

/// Whole minutes set through the seconds key: the start knows the key, and the sentence names it alone. Before, the sentence named
/// `run.copy_every_minutes` first, and the first command it printed changed nothing in a file that sets the seconds key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b823_seconds_whole_minutes_set_through_the_seconds_key_name_the_seconds_key_alone() {
    let stage = Stage::new(Answer::Older);
    let api = Api::new(stage.clone());
    let (req, key) = resolved("configVersion = 1\n[run]\ncopy_every_seconds = 120\n");
    assert_eq!(key, Some(Key::RunCopyEverySeconds));
    let e = start_direct_set_by(&api, &req, "n1", key)
        .await
        .unwrap_err();
    assert_eq!(
        stage.bodies()[0]["snapshotEveryS"],
        120,
        "TRIGGER PRESENT: the request carried two whole minutes"
    );
    let shown = e.to_string();
    for words in [
        "this stage predates the copy interval of a run",
        "run.copy_every_seconds = 120; run `ham config unset run.copy_every_seconds` or update the stage",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
    assert!(!shown.contains("run.copy_every_minutes"), "{shown}");
    assert!(
        matches!(&e, ApiError::Http { status: 400, error } if error.code == "stage-predates-copy-interval"),
        "{e:?}"
    );
    assert_eq!(stage.bodies().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b823_seconds_an_interval_set_through_the_minutes_key_names_the_minutes_key_alone() {
    let stage = Stage::new(Answer::Older);
    let api = Api::new(stage.clone());
    let (req, key) = resolved("configVersion = 1\n[run]\ncopy_every_minutes = 2\n");
    assert_eq!(key, Some(Key::RunCopyEveryMinutes));
    let e = start_direct_set_by(&api, &req, "n1", key)
        .await
        .unwrap_err();
    let shown = e.to_string();
    for words in [
        "this stage predates the copy interval of a run",
        "run.copy_every_minutes = 2; run `ham config unset run.copy_every_minutes` or update the stage",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
    assert!(!shown.contains("run.copy_every_seconds"), "{shown}");
    assert_eq!(stage.bodies().len(), 1);
}

/// A caller that does not know the key (a request that came from somewhere else than this config) keeps the words that name both keys
/// for whole minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b823_seconds_a_start_that_names_no_key_keeps_both_names_for_whole_minutes() {
    let stage = Stage::new(Answer::Older);
    let api = Api::new(stage.clone());
    let e = start_direct(&api, &with_seconds(120), "n1")
        .await
        .unwrap_err();
    let shown = e.to_string();
    for words in [
        "run.copy_every_minutes = 2 (or run.copy_every_seconds = 120, whichever your config sets)",
        "`ham config unset run.copy_every_minutes` (or `ham config unset run.copy_every_seconds`)",
    ] {
        assert!(shown.contains(words), "{words:?} is not in: {shown}");
    }
}

#[test]
fn b823_seconds_the_words_for_a_stage_that_predates_the_member_follow_the_key() {
    use ham::api::predates_copy_interval_words as words;
    let seconds = "this stage predates the copy interval of a run, so it cannot start a run with run.copy_every_seconds = 600; run `ham config unset run.copy_every_seconds` or update the stage";
    let minutes = "this stage predates the copy interval of a run, so it cannot start a run with run.copy_every_minutes = 10; run `ham config unset run.copy_every_minutes` or update the stage";
    assert_eq!(words(600, Some(Key::RunCopyEverySeconds)), seconds);
    assert_eq!(words(600, Some(Key::RunCopyEveryMinutes)), minutes);
    // Seconds that are no whole minutes can only come from the seconds key, whoever asks.
    assert!(words(45, None).contains("run.copy_every_seconds = 45; run"));
    assert!(!words(45, None).contains("run.copy_every_minutes"));
}
