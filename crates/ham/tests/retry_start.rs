//! The call the second Enter of a retry makes: `POST /v1/runs/{runId}/retry` for the blocked run, an empty JSON object as the body, and the
//! key `retry-<blocked run id>` (so a second press, or a send that got no answer, returns the run the first one started). The client sends no
//! prompt, rules or settings: the service holds the request the run was started with, and `ham` keeps no copy of it (invariant 5). A scripted
//! plane answers, so the path, the headers and the body are the ones the client sent.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham_core::retry::start_key;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";

/// One call the client made: method, path, headers, the body as text.
type Call = (String, String, Vec<(String, String)>, String);

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
    reply: Mutex<Option<(u16, Value)>>,
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
            .reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or((201, json!({"runId": NEW, "status": "active"})));
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
        Err(TransportError::Refused("not used".into()))
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn api(plane: &Arc<Scripted>) -> Api<Scripted> {
    Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    })
}

async fn run(plane: &Arc<Scripted>) -> Result<String, String> {
    let env = |_: &str| None;
    match perform_with_env(&api(plane), Job::RetryRun { run_id: RUN.into() }, &env).await {
        Done::RetryStarted { run_id, result } => {
            assert_eq!(run_id, RUN, "the answer names the blocked run");
            result
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_enter_posts_the_retry_of_the_blocked_run_with_its_key() {
    let plane = Arc::new(Scripted::default());
    assert_eq!(run(&plane).await, Ok(NEW.to_string()));
    let calls = plane.calls.lock().unwrap().clone();
    // The row of the blocked run is read first: its model says whether the account's data retention mode has to be looked at before the
    // stored request is started again. This plane's answer names no model, so nothing else is asked.
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("GET", "/v1/runs?q=01M4ZHFBV5SDX7J92E6YRH4QTW")
    );
    let (method, path, headers, body) = &calls[1];
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/v1/runs/01M4ZHFBV5SDX7J92E6YRH4QTW/retry")
    );
    assert_eq!(
        header(headers, "Idempotency-Key"),
        Some(start_key(RUN).as_str())
    );
    assert_eq!(
        header(headers, "Idempotency-Key"),
        Some("retry-01M4ZHFBV5SDX7J92E6YRH4QTW")
    );
    let sent: Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        sent,
        json!({}),
        "no prompt, rules or settings leave the client: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_press_sends_the_same_key() {
    let plane = Arc::new(Scripted::default());
    run(&plane).await.unwrap();
    run(&plane).await.unwrap();
    let keys: Vec<String> = plane
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 == "POST")
        .map(|c| header(&c.2, "Idempotency-Key").unwrap().to_string())
        .collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1], "so the service returns the first run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_of_the_service_comes_back_in_plain_words() {
    for (status, body, holds) in [
        (
            409,
            json!({"code": "run-not-blocked", "message": "run R is complete; only a blocked run can be retried"}),
            "only a blocked run",
        ),
        (
            422,
            json!({"code": "unknown-model", "message": "model m is not offered"}),
            "start a new run",
        ),
        (
            404,
            json!({"code": "not-found", "message": "no such route"}),
            "does not offer retry",
        ),
    ] {
        let plane = Arc::new(Scripted::default());
        *plane.reply.lock().unwrap() = Some((status, body));
        let err = run(&plane).await.unwrap_err();
        assert!(err.contains(holds), "{status}: {err}");
        assert!(!err.contains('\n'), "{err:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_the_client_cannot_read_is_an_error_not_a_run() {
    let plane = Arc::new(Scripted::default());
    *plane.reply.lock().unwrap() = Some((201, json!({"status": "active"})));
    let err = run(&plane).await.unwrap_err();
    assert!(!err.is_empty());
}
