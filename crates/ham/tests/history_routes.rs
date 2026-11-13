//! The two read calls of the full-history chat view: one page of the stored conversation
//! (`GET /v1/runs/{id}/transcript`) and the search across runs (`GET /v1/search`). The routes and their wire types are on main
//! (`control-api`, `ham_core::transcript`, `ham_core::runsearch`); here `Api` asks them with the right query, parses the answer, and
//! leaves the server's own words in the error. A scripted plane answers, so every status is exact.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::runsearch::{HitField, SearchQuery};
use ham_core::transcript::{Before, TranscriptQuery, TranscriptState};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<(String, String)>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl Scripted {
    fn new(replies: Vec<(u16, Value)>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        })
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), path.to_string()));
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((500, json!({"code": "unscripted", "message": path})));
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

fn api(plane: &Arc<Scripted>) -> Api<Scripted> {
    Api::new(plane.clone()).with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(5),
    })
}

fn page() -> Value {
    json!({
        "runId": RUN, "state": "live",
        "records": [{"seq": 4, "n": 9, "incarnation": "01M5A2R7K3C9XJ4D8E6F000001", "origin": "engine", "t": "2026-10-16T08:15:09Z",
                     "dir": "in", "kind": "agent_message_chunk",
                     "data": {"messageId": "m", "content": {"type": "text", "text": "hello"}}}],
        "gaps": [{"kind": "missing", "code": "missing-chunk", "fromSeq": 2, "toSeq": 2}],
        "completeness": [], "publishedThrough": 4, "listedThrough": 4, "next": "cursor-next", "prev": 4, "more": false, "skippedKeys": 0
    })
}

#[tokio::test]
async fn a_page_is_asked_from_the_end_with_the_limit_and_parsed() {
    let plane = Scripted::new(vec![(200, page())]);
    let q = TranscriptQuery {
        before: Some(Before::End),
        limit: Some(32),
        ..TranscriptQuery::default()
    };
    let p = api(&plane).transcript_page(RUN, &q).await.unwrap();
    assert_eq!(
        plane.calls(),
        vec![(
            "GET".to_string(),
            format!("/v1/runs/{RUN}/transcript?before=end&limit=32")
        )]
    );
    assert_eq!(p.state, TranscriptState::Live);
    assert_eq!(p.records.len(), 1);
    assert_eq!(p.records[0].kind, "agent_message_chunk");
    assert_eq!(p.gaps.len(), 1);
    assert_eq!(p.next.as_deref(), Some("cursor-next"));
    assert_eq!(p.prev, Some(4));
}

#[tokio::test]
async fn later_pages_carry_their_cursors_encoded() {
    let plane = Scripted::new(vec![(200, page()), (200, page())]);
    let a = api(&plane);
    a.transcript_page(
        RUN,
        &TranscriptQuery {
            after: Some("a/b+c==".into()),
            limit: Some(8),
            ..TranscriptQuery::default()
        },
    )
    .await
    .unwrap();
    a.transcript_page(
        RUN,
        &TranscriptQuery {
            before: Some(Before::Seq(7)),
            ..TranscriptQuery::default()
        },
    )
    .await
    .unwrap();
    let paths: Vec<String> = plane.calls().into_iter().map(|(_, p)| p).collect();
    assert_eq!(
        paths,
        vec![
            format!("/v1/runs/{RUN}/transcript?after=a%2Fb%2Bc%3D%3D&limit=8"),
            format!("/v1/runs/{RUN}/transcript?before=7"),
        ]
    );
}

#[tokio::test]
async fn another_principals_run_is_the_servers_404_in_its_own_words() {
    let plane = Scripted::new(vec![(
        404,
        json!({"code": "not-found", "message": "no such run"}),
    )]);
    let e = api(&plane)
        .transcript_page(RUN, &TranscriptQuery::default())
        .await
        .unwrap_err();
    match e {
        ApiError::Http { status, error } => {
            assert_eq!(status, 404);
            assert_eq!(error.code, "not-found");
        }
        other => panic!("{other:?}"),
    }
    // A 404 is an answer: asked once.
    assert_eq!(plane.calls().len(), 1);
}

#[tokio::test]
async fn a_gateway_error_is_asked_again_and_then_answered() {
    let plane = Scripted::new(vec![
        (503, json!({"code": "unavailable", "message": "later"})),
        (200, page()),
    ]);
    let p = api(&plane)
        .transcript_page(RUN, &TranscriptQuery::default())
        .await
        .unwrap();
    assert_eq!(p.records.len(), 1);
    assert_eq!(plane.calls().len(), 2);
}

#[tokio::test]
async fn the_search_asks_with_the_words_and_parses_the_hits() {
    let plane = Scripted::new(vec![(
        200,
        json!({
            "hits": [{"runId": RUN, "createdAt": "2026-10-16T08:00:00Z", "field": "chat", "snippet": "…the percent change helper…",
                      "seq": 3, "offset": 5, "open": "cursor-open", "coverage": "full"}],
            "searched": 12, "partial": false, "more": true, "next": "older", "partialCoverage": 0
        }),
    )]);
    let q = SearchQuery {
        q: "percent change".into(),
        limit: Some(20),
        ..SearchQuery::default()
    };
    let r = api(&plane).search_runs(&q).await.unwrap();
    assert_eq!(
        plane.calls(),
        vec![(
            "GET".to_string(),
            "/v1/search?q=percent%20change&limit=20".to_string()
        )]
    );
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].run_id, RUN);
    assert_eq!(r.hits[0].field, HitField::Chat);
    assert_eq!(r.hits[0].open.as_deref(), Some("cursor-open"));
    assert!(r.more);
    assert_eq!(r.next.as_deref(), Some("older"));
}
