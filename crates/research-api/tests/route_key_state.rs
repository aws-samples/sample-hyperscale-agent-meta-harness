//! The operator's key-state question: `POST /internal/research/key-state`, reached by a direct invoke of the function (API Gateway has no such
//! method). It answers which provider would serve a search now and the Secrets Manager version id of the key the function holds, and nothing else: no run,
//! no policy row, no counter, no audit object, no key and nothing derived from one. A request that carries a worker session is refused as if the route did
//! not exist. The five routes keep needing a worker session. No test reaches the network or AWS.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::research::{ResearchErrorCode, ResearchProvider, route};
use research_api::fakes::{FixedClock, MemBlobs, MemTable, RecordingInvoker, ScriptedProvider};
use research_api::handler::{Config, Deps, KEY_STATE_PATH, Request, handle};
use research_api::lambda::to_request;
use research_api::ports::{Fail, KeyState, Stage, StageKind};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use support::{ACCOUNT, NOW_MS, ROLE, RUN, caller_arn};

/// A stage whose key state is whatever the test sets, and which counts how often it was asked.
struct FakeStage {
    provider: ScriptedProvider,
    state: Mutex<Result<KeyState, Fail>>,
    asked: AtomicU32,
}

impl FakeStage {
    fn new(state: Result<KeyState, Fail>) -> Self {
        Self {
            provider: ScriptedProvider::new(ResearchProvider::Fixture),
            state: Mutex::new(state),
            asked: AtomicU32::new(0),
        }
    }
    fn asked(&self) -> u32 {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Stage for FakeStage {
    type Provider = ScriptedProvider;

    async fn serve(&self) -> Result<&ScriptedProvider, Fail> {
        Ok(&self.provider)
    }

    fn audit_word(&self) -> ResearchProvider {
        ResearchProvider::Fixture
    }

    async fn key_state(&self) -> Result<KeyState, Fail> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.state.lock().unwrap().clone()
    }
}

type D = Deps<MemTable, MemBlobs, FakeStage, RecordingInvoker, FixedClock>;

fn deps(stage: FakeStage) -> D {
    // No policy row for any run: the question needs none.
    Deps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        provider: stage,
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(NOW_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: 80,
            stage_kind: StageKind::Official,
        },
    }
}

fn holding(provider: ResearchProvider, version: Option<&str>) -> FakeStage {
    FakeStage::new(Ok(KeyState {
        provider,
        version_id: version.map(str::to_string),
    }))
}

fn operator(body: &str) -> Request {
    Request {
        method: "POST".into(),
        path: KEY_STATE_PATH.into(),
        caller_arn: None,
        account_id: Some("000000000000".into()),
        access_key: None,
        request_id: "operator-key-state".into(),
        body: body.into(),
    }
}

#[test]
fn the_path_is_not_one_of_the_five_routes_api_gateway_serves() {
    assert_eq!(KEY_STATE_PATH, "/internal/research/key-state");
    assert_eq!(route::ALL.len(), 5);
    assert!(!route::ALL.contains(&KEY_STATE_PATH));
}

#[tokio::test]
async fn the_answer_names_the_provider_and_the_version_and_nothing_else() {
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-1")));
    let resp = handle(&d, &operator("{}")).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v, json!({"provider": "tavily", "versionId": "ver-1"}));
    assert_eq!(resp.log.op, "key_state");
    assert_eq!(resp.log.run, None);
    assert_eq!(resp.log.status, 200);
    assert_eq!(d.provider.asked(), 1);
}

#[tokio::test]
async fn with_no_key_the_version_is_left_out() {
    let d = deps(holding(ResearchProvider::Agentcore, None));
    let resp = handle(&d, &operator("{}")).await;
    assert_eq!(resp.status, 200);
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v, json!({"provider": "agentcore"}));
}

#[tokio::test]
async fn it_touches_no_run_no_counter_and_writes_no_audit_object() {
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-1")));
    let resp = handle(&d, &operator("{}")).await;
    assert_eq!(resp.status, 200);
    assert!(
        d.blobs.keys().is_empty(),
        "no audit object, no search record"
    );
    assert!(d.invoker.events().is_empty(), "no job was started");
}

#[tokio::test]
async fn a_worker_session_is_refused_as_if_the_route_did_not_exist_and_nothing_is_read() {
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-1")));
    let mut req = operator("{}");
    req.caller_arn = Some(caller_arn(RUN));
    req.account_id = Some(ACCOUNT.into());
    let resp = handle(&d, &req).await;
    assert_eq!(resp.status, 404);
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v["code"], "not_found");
    assert_eq!(d.provider.asked(), 0, "the key cache was not asked");
}

#[tokio::test]
async fn a_worker_session_gets_not_found_whatever_the_method() {
    // The denial comes before the method check, so a worker cannot tell from a 405 that the path is handled.
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-1")));
    for method in ["POST", "GET", "PUT", "DELETE", "HEAD"] {
        let mut req = operator("{}");
        req.method = method.into();
        req.caller_arn = Some(caller_arn(RUN));
        req.account_id = Some(ACCOUNT.into());
        let resp = handle(&d, &req).await;
        assert_eq!(resp.status, 404, "{method}");
        let v: Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["code"], "not_found", "{method}");
    }
    assert_eq!(d.provider.asked(), 0);
}

#[tokio::test]
async fn only_post_with_an_empty_object_is_served() {
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-1")));
    let mut get = operator("{}");
    get.method = "GET".into();
    let resp = handle(&d, &get).await;
    assert_eq!(resp.status, 405);
    for body in [
        r#"{"x":1}"#,
        "[]",
        "junk",
        r#"{"provider":"tavily"}"#,
        "null",
    ] {
        let resp = handle(&d, &operator(body)).await;
        assert_eq!(resp.status, 400, "{body}");
        let v: Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["code"], "usage", "{body}");
    }
    for ok in ["{}", " { } ", "{\n}", ""] {
        let resp = handle(&d, &operator(ok)).await;
        assert_eq!(resp.status, 200, "{ok:?}: {}", resp.body);
    }
    assert_eq!(d.provider.asked(), 4, "only the served requests asked");
}

#[tokio::test]
async fn a_key_store_that_cannot_be_read_fails_the_question_with_its_word() {
    let d = deps(FakeStage::new(Err(Fail::upstream(
        "the key store did not answer",
    ))));
    let resp = handle(&d, &operator("{}")).await;
    assert_eq!(resp.status, ResearchErrorCode::UpstreamError.http_status());
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v["code"], "upstream_error");
    assert_eq!(resp.log.code.as_deref(), Some("upstream_error"));
}

#[tokio::test]
async fn the_five_routes_still_need_a_worker_session() {
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-1")));
    for path in route::ALL {
        let mut req = operator("{}");
        req.path = path.into();
        let resp = handle(&d, &req).await;
        assert_eq!(resp.status, 503, "{path}: {}", resp.body);
        let v: Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["code"], "disabled", "{path}");
    }
    assert_eq!(d.provider.asked(), 0);
}

#[tokio::test]
async fn the_event_the_operator_script_sends_reaches_the_handler_as_this_request() {
    // fixtures/research/key-state-event.json is the event scripts/research-key-put.py sends with `aws lambda invoke`; its test pins the same file.
    let event = include_str!("../../../fixtures/research/key-state-event.json");
    let req = to_request(&lambda_http::request::from_str(event).unwrap());
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", KEY_STATE_PATH)
    );
    assert_eq!(req.caller_arn, None, "a direct invoke carries no caller");
    assert_eq!(req.body, "{}");
    let d = deps(holding(ResearchProvider::Tavily, Some("ver-9")));
    let resp = handle(&d, &req).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let v: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(v["versionId"], "ver-9");
}
