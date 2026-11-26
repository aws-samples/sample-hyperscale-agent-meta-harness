//! The whole read of the run list hands its newest page over as soon as it has it (`Api::read_runs_whole`): the page of the first call
//! goes to the caller before the second page is asked for, when the first page carries a next token. A first page without a next token
//! hands over nothing: the read is one call and one answer. A later page that fails fails the read; the pages read so far are never
//! given as if they were the whole list.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::runfilter::{IndexedRead, Refresh, RunsRead};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";

/// A plane that answers each call with the next scripted reply and keeps the paths it was asked.
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
        let (status, body) = self.replies.lock().unwrap().pop_front().unwrap_or((
            500,
            json!({"code": "unscripted", "message": "no reply scripted"}),
        ));
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
        unreachable!("these calls fetch nothing")
    }
}

fn row(id: &str, title: Option<&str>) -> Value {
    let mut r = json!({"runId": id, "status": "complete", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN});
    if let Some(t) = title {
        r["title"] = json!(t);
    }
    r
}

fn api(replies: Vec<(u16, Value)>) -> (Api<Scripted>, Arc<Scripted>) {
    let plane = Arc::new(Scripted::default());
    *plane.replies.lock().unwrap() = replies.into_iter().collect();
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    (api, plane)
}

/// What the read handed over: for each hand-over, the calls the plane had had by then and the page.
type Handed = Arc<Mutex<Vec<(usize, IndexedRead)>>>;

/// The read, with a closure that keeps every hand-over.
async fn whole(
    api: &Api<Scripted>,
    plane: &Arc<Scripted>,
) -> (Result<IndexedRead, String>, Vec<(usize, IndexedRead)>) {
    let handed: Handed = Arc::default();
    let (keep, seen) = (Arc::clone(&handed), Arc::clone(plane));
    let mut newest = move |page: IndexedRead| {
        let calls = seen.calls.lock().unwrap().len();
        keep.lock().unwrap().push((calls, page));
    };
    let got = api
        .read_runs_whole(&mut newest)
        .await
        .map_err(|e| e.to_string());
    let handed = handed.lock().unwrap().clone();
    (got, handed)
}

fn ids(read: &RunsRead) -> Vec<String> {
    let runs = match read {
        RunsRead::All(runs) | RunsRead::First { runs, .. } => runs,
    };
    runs.iter().map(|r| r.run_id.clone()).collect()
}

fn entry_ids(read: &IndexedRead) -> Vec<String> {
    read.index.iter().map(|e| e.run_id.clone()).collect()
}

#[tokio::test]
async fn b961_the_newest_page_is_handed_over_before_the_second_page_is_asked_for() {
    let (api, plane) = api(vec![
        (
            200,
            json!({"runs": [row("R1", Some("one")), row("R2", None)], "nextToken": "t2"}),
        ),
        (
            200,
            json!({"runs": [row("R3", Some("three"))], "nextToken": "t3"}),
        ),
        (200, json!({"runs": [row("R4", None)]})),
    ]);
    let (got, handed) = whole(&api, &plane).await;
    assert_eq!(handed.len(), 1, "the newest page is handed over once");
    let (calls_then, page) = &handed[0];
    assert_eq!(
        *calls_then, 1,
        "the page is handed over before the second page is asked for"
    );
    assert!(
        matches!(page.read, RunsRead::First { has_more: true, .. }),
        "{:?}",
        page.read
    );
    assert_eq!(ids(&page.read), ["R1", "R2"]);
    assert_eq!(entry_ids(page), ["R1"]);
    let got = got.unwrap();
    assert!(matches!(got.read, RunsRead::All(_)), "{:?}", got.read);
    assert_eq!(ids(&got.read), ["R1", "R2", "R3", "R4"]);
    assert_eq!(entry_ids(&got), ["R1", "R3"]);
    assert_eq!(
        plane.calls.lock().unwrap().clone(),
        ["/v1/runs", "/v1/runs?nextToken=t2", "/v1/runs?nextToken=t3"]
    );
}

#[tokio::test]
async fn b961_a_first_page_without_a_next_token_hands_over_nothing() {
    let page = json!({"runs": [row("R1", Some("one")), row("R2", None)]});
    let (api, plane) = api(vec![(200, page.clone())]);
    let (got, handed) = whole(&api, &plane).await;
    assert!(handed.is_empty(), "nothing is handed over: {handed:?}");
    assert_eq!(plane.calls.lock().unwrap().clone(), ["/v1/runs"]);
    let got = got.unwrap();
    // The same as the whole read of before.
    let (api, _plane) = self::api(vec![(200, page)]);
    let today = api.read_runs_indexed(Refresh::Manual).await.unwrap();
    assert!(matches!(got.read, RunsRead::All(_)), "{:?}", got.read);
    assert_eq!(ids(&got.read), ids(&today.read));
    assert_eq!(got.index, today.index);
}

#[tokio::test]
async fn b961_a_full_first_page_with_an_empty_second_page_is_handed_over_once() {
    // A first page of exactly 100 runs carries a token although nothing follows: the store gives a key whenever its limit is reached.
    let hundred: Vec<Value> = (0..100)
        .map(|n| row(&format!("R{n:03}"), Some("a title")))
        .collect();
    let (api, plane) = api(vec![
        (200, json!({"runs": hundred, "nextToken": "t2"})),
        (200, json!({"runs": []})),
    ]);
    let (got, handed) = whole(&api, &plane).await;
    assert_eq!(handed.len(), 1, "the full page is handed over once");
    assert_eq!(handed[0].0, 1);
    assert_eq!(ids(&handed[0].1.read).len(), 100);
    assert_eq!(plane.calls.lock().unwrap().len(), 2);
    let got = got.unwrap();
    let all = ids(&got.read);
    assert_eq!(all.len(), 100, "the same 100 rows");
    assert_eq!(
        all,
        ids(&handed[0].1.read),
        "each once, in the page's order"
    );
    assert_eq!(got.index.len(), 100);
}

#[tokio::test]
async fn b961_a_second_page_that_fails_fails_the_read_after_the_hand_over() {
    let (api, plane) = api(vec![
        (
            200,
            json!({"runs": [row("R1", None)], "nextToken": "tokenOfTheCaller"}),
        ),
        (500, json!({"code": "internal", "message": "boom"})),
    ]);
    let (got, handed) = whole(&api, &plane).await;
    assert_eq!(
        handed.len(),
        1,
        "the newest page was handed over before the failure"
    );
    let error = got.expect_err("a later page that fails fails the read");
    // The token holds the caller's identity: the error names the route without its query.
    assert!(!error.contains("tokenOfTheCaller"), "{error}");
    assert!(!error.contains('?'), "{error}");
    assert!(!error.contains("nextToken"), "{error}");
    assert_eq!(plane.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn b961_a_token_the_server_repeats_ends_the_read() {
    let (api, plane) = api(vec![
        (200, json!({"runs": [row("R1", None)], "nextToken": "t2"})),
        (200, json!({"runs": [row("R2", None)], "nextToken": "t2"})),
    ]);
    let (got, _handed) = whole(&api, &plane).await;
    let got = got.expect("a token that comes back ends the read with what was read");
    assert_eq!(ids(&got.read), ["R1", "R2"]);
    assert_eq!(plane.calls.lock().unwrap().len(), 2);
}
