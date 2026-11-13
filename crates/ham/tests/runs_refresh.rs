//! Reading the Runs list again. The timer reads the newest page only and lays it over the list already held; `r` reads
//! every page. A stand-in transport records the paths asked for, so the tests count requests and never time them.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::runfilter::Refresh;
use harness_protocol::api::v1::RunSummary;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Scripted {
    answers: Mutex<VecDeque<Result<Response, TransportError>>>,
    paths: Mutex<Vec<String>>,
}

impl Scripted {
    fn with(bodies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                bodies
                    .into_iter()
                    .map(|b| {
                        Ok(Response {
                            status: 200,
                            headers: Default::default(),
                            body: b.into_bytes(),
                        })
                    })
                    .collect(),
            ),
            paths: Mutex::default(),
        })
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.paths.lock().unwrap().push(path.to_string());
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(TransportError::Refused("script ended".into())))
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("no objects here".into()))
    }
}

fn api(plane: &Arc<Scripted>) -> Api<Scripted> {
    Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    })
}

fn run_json(id: &str, minute: u32, status: &str) -> String {
    format!(
        r#"{{"runId": "{id}", "status": "{status}", "phase": "planning", "target": {{"mode": "s3-only"}},
        "createdAt": "2026-10-16T08:{minute:02}:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"}}"#
    )
}

fn page(runs: &[String], next: Option<&str>) -> String {
    match next {
        Some(t) => format!(r#"{{"runs": [{}], "nextToken": "{t}"}}"#, runs.join(",")),
        None => format!(r#"{{"runs": [{}]}}"#, runs.join(",")),
    }
}

fn held() -> Vec<RunSummary> {
    ["R3", "R2", "R1"]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            serde_json::from_str(&run_json(id, 3 - u32::try_from(i).unwrap(), "active")).unwrap()
        })
        .collect()
}

fn ids(runs: &[RunSummary]) -> Vec<&str> {
    runs.iter().map(|r| r.run_id.as_str()).collect()
}

#[tokio::test]
async fn the_timer_asks_for_page_one_only_and_its_page_is_laid_over_the_held_list_when_applied() {
    let p = Scripted::with(vec![page(
        &[run_json("R4", 4, "active"), run_json("R3", 3, "complete")],
        Some("t1"),
    )]);
    let read = api(&p).read_runs(Refresh::Timer).await.unwrap();
    assert_eq!(p.paths(), ["/v1/runs"], "one request, no token followed");
    let got = read.apply(&held());
    assert_eq!(ids(&got), ["R4", "R3", "R2", "R1"]);
    assert_eq!(
        serde_json::to_value(&got[1]).unwrap()["status"],
        "complete",
        "the refreshed row replaced the held one"
    );
}

#[tokio::test]
async fn r_asks_for_every_page_and_ignores_what_is_held() {
    let p = Scripted::with(vec![
        page(
            &[run_json("R4", 4, "active"), run_json("R3", 3, "active")],
            Some("t1"),
        ),
        page(&[run_json("R2", 2, "active")], None),
    ]);
    let read = api(&p).read_runs(Refresh::Manual).await.unwrap();
    assert_eq!(p.paths(), ["/v1/runs", "/v1/runs?nextToken=t1"]);
    let got = read.apply(&held());
    assert_eq!(
        ids(&got),
        ["R4", "R3", "R2"],
        "R1 is gone from the server, so it is gone"
    );
}

#[tokio::test]
async fn a_first_page_with_no_next_token_is_the_whole_list() {
    let p = Scripted::with(vec![page(
        &[run_json("R4", 4, "active"), run_json("R3", 3, "active")],
        None,
    )]);
    let read = api(&p).read_runs(Refresh::Timer).await.unwrap();
    assert_eq!(p.paths(), ["/v1/runs"]);
    assert_eq!(ids(&read.apply(&held())), ["R4", "R3"]);
}

#[tokio::test]
async fn a_failed_read_is_an_error_and_the_held_list_is_the_callers_to_keep() {
    let p = Scripted::with(vec![]);
    let held = held();
    let e = api(&p).read_runs(Refresh::Timer).await;
    assert!(e.is_err());
    assert_eq!(held.len(), 3);
}

#[tokio::test]
async fn the_page_token_is_percent_encoded_in_the_query() {
    let p = Scripted::with(vec![
        page(&[run_json("R2", 2, "active")], Some("a b+c/d=e&f")),
        page(&[run_json("R1", 1, "active")], Some("abc-DEF_123.~")),
        page(&[run_json("R0", 0, "active")], None),
    ]);
    let got = api(&p).list_runs().await.unwrap();
    assert_eq!(ids(&got), ["R2", "R1", "R0"]);
    assert_eq!(
        p.paths(),
        [
            "/v1/runs",
            "/v1/runs?nextToken=a%20b%2Bc%2Fd%3De%26f",
            "/v1/runs?nextToken=abc-DEF_123.~"
        ],
        "reserved characters are escaped, unreserved ones are not"
    );
}

#[tokio::test]
async fn list_runs_still_reads_every_page() {
    let p = Scripted::with(vec![
        page(&[run_json("R2", 2, "active")], Some("t1")),
        page(&[run_json("R1", 1, "active")], None),
    ]);
    let got = api(&p).list_runs().await.unwrap();
    assert_eq!(ids(&got), ["R2", "R1"]);
    assert_eq!(p.paths().len(), 2);
}
