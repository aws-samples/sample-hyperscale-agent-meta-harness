//! The client side of the chat-client server routes: the typed calls of
//! `ham::chat_routes` and the download of the work in progress with its checks. Two kinds of plane carry them: a scripted one (every
//! status and every wrong byte exact) and the stand-in API behind the real `control_api::handler` (the route and the client agree).
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError, Retry};
use ham::chat_routes::{SnapshotFetchError, fetch_snapshot_with};
use ham::http::{ControlPlane, Response, TransportError};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham_core::events_view::EventsQuery;
use ham_core::run_index::RunFilter;
use ham_core::snapshot::{SNAPSHOT_REF, SnapshotError};
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::EventType;
use harness_protocol::store::event_shard;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Fake, REGION, creds};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const OTHER_HEAD: &str = "3333333333333333333333333333333333333333";
const BASE: &str = "1111111111111111111111111111111111111111";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn bundle(head: &str, filler: usize) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n-{BASE} base subject\n{head} {SNAPSHOT_REF}\n\nPACK")
        .into_bytes();
    b.extend((0..filler).map(|i| (i % 251) as u8));
    b
}

fn fast() -> Retry {
    Retry {
        attempts: 3,
        base: Duration::from_millis(5),
    }
}

// ---- a scripted plane ---------------------------------------------------------------------------------------------------

/// Answers `call` from a queue per path prefix and `fetch` from one object with S3's range rules; records both.
#[derive(Default)]
struct Canned {
    calls: Mutex<Vec<(String, String)>>,
    replies: Mutex<BTreeMap<String, VecDeque<(u16, Value)>>>,
    object: Mutex<Vec<u8>>,
    /// The next fetches answer these statuses (403: the URL expired; 412: the object changed), then the object is served.
    fetch_faults: Mutex<VecDeque<u16>>,
    fetches: Mutex<Vec<Option<(u64, u64)>>>,
    /// The `If-Match` of each fetch.
    if_matches: Mutex<Vec<Option<String>>>,
}

impl Canned {
    fn new(object: Vec<u8>) -> Arc<Canned> {
        let c = Canned::default();
        *c.object.lock().unwrap() = object;
        Arc::new(c)
    }
    fn reply(&self, path_prefix: &str, status: u16, body: Value) {
        self.replies
            .lock()
            .unwrap()
            .entry(path_prefix.to_string())
            .or_default()
            .push_back((status, body));
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ControlPlane for Canned {
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
            .push((method.into(), path.into()));
        let mut g = self.replies.lock().unwrap();
        let key = g
            .iter()
            .filter(|(k, q)| path.starts_with(k.as_str()) && !q.is_empty())
            .max_by_key(|(k, _)| k.len())
            .map(|(k, _)| k.clone());
        let (status, body) = key
            .and_then(|k| g.get_mut(&k).and_then(VecDeque::pop_front))
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
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        self.fetches.lock().unwrap().push(range);
        self.if_matches
            .lock()
            .unwrap()
            .push(if_match.map(str::to_string));
        if let Some(status) = self.fetch_faults.lock().unwrap().pop_front() {
            return Ok(Response {
                status,
                headers: BTreeMap::new(),
                body: b"<Error/>".to_vec(),
            });
        }
        let object = self.object.lock().unwrap().clone();
        let size = object.len() as u64;
        let headers = BTreeMap::from([("etag".to_string(), "\"abc\"".to_string())]);
        match range {
            None => Ok(Response {
                status: 200,
                headers,
                body: object,
            }),
            Some((s, e)) => {
                let e = e.min(size - 1);
                let mut h = headers;
                h.insert("content-range".into(), format!("bytes {s}-{e}/{size}"));
                Ok(Response {
                    status: 206,
                    headers: h,
                    body: object[usize::try_from(s).unwrap()..=usize::try_from(e).unwrap()]
                        .to_vec(),
                })
            }
        }
    }
}

fn scripted(plane: &Arc<Canned>) -> Api<Canned> {
    Api::new(plane.clone()).with_retry(fast())
}

fn dl(size: u64, head: &str) -> Value {
    json!({
        "url": "https://bucket.s3.us-east-1.amazonaws.com/runs/x/snapshot.bundle?X-Amz-Signature=deadbeef&X-Amz-Security-Token=SECRETTOKEN",
        "expiresAt": "2099-10-15T13:25:00Z", "headSha": head, "baseSha": BASE, "takenAt": "2026-10-15T13:10:00Z",
        "count": 3, "sizeBytes": size, "etag": "\"abc\"", "runStatus": "active"
    })
}

fn tmp(label: &str) -> std::path::PathBuf {
    support::scratch(label)
}

// ---- events from the end ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn events_view_asks_for_the_tail_and_reads_the_superset_page() {
    let plane = Canned::new(vec![]);
    let progress = |n: u32| {
        json!({"schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-15T13:41:07Z",
               "type": "turn.progress", "authority": "agent", "payload": {"kind": "inline", "value": {"kind": "message", "text": "x"}}})
    };
    let future = json!({"schemaVersion": 1, "eventId": id(3), "runId": RUN, "seq": 3, "ts": "2026-10-15T13:41:07Z",
                        "type": "future.thing", "authority": "harness", "payload": {"kind": "inline", "value": {}}});
    plane.reply(
        "/v1/runs/",
        200,
        json!({"events": [progress(1), progress(2), future, progress(4)], "nextAfter": id(4), "nextBefore": id(1), "hasMore": true,
               "answers": [{"requestId": id(2), "action": "continue_with_prompt", "text": "go on", "createdAt": "2026-10-15T13:42:00Z"}]}),
    );
    let api = scripted(&plane);
    let shard = event_shard(harness_protocol::envelope::Authority::Agent, Some("direct"));
    let view = api
        .events_view(RUN, &EventsQuery::tail(shard, 50))
        .await
        .unwrap();
    assert_eq!(
        plane.calls()[0].1,
        format!("/v1/runs/{RUN}/events?shard={shard}&tail=50")
    );
    assert_eq!(
        view.events.len(),
        3,
        "the event of a type this client does not know is left out"
    );
    assert!(view.has_more);
    assert_eq!(view.next_before.as_deref(), Some(id(1).as_str()));
    assert_eq!(view.answers[0].text.as_deref(), Some("go on"));
    assert_eq!(view.answers[0].action, InstructAction::ContinueWithPrompt);
    let older = EventsQuery::older(shard, &id(1), 20);
    plane.reply("/v1/runs/", 200, json!({"events": []}));
    api.events_view(RUN, &older).await.unwrap();
    assert_eq!(
        plane.calls()[1].1,
        format!(
            "/v1/runs/{RUN}/events?shard={shard}&tail=20&before={}",
            id(1)
        )
    );
}

#[tokio::test]
async fn an_events_read_is_one_attempt_the_poll_is_its_own_retry() {
    let plane = Canned::new(vec![]);
    plane.reply(
        "/v1/runs/",
        503,
        json!({"code": "unavailable", "message": "retry"}),
    );
    let e = scripted(&plane)
        .events_view(RUN, &EventsQuery::tail(28, 5))
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(503));
    assert_eq!(plane.calls().len(), 1);
}

// ---- the run list ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn run_rows_sends_the_filter_encoded_and_the_token_last_and_follows_tokens() {
    let plane = Canned::new(vec![]);
    let row = |n: &str| {
        json!({"runId": n, "status": "complete", "phase": "green", "target": {"mode": "s3-only"}, "createdAt": "2026-10-15T13:00:00Z",
               "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b", "title": format!("run {n}")})
    };
    plane.reply(
        "/v1/runs",
        200,
        json!({"runs": [row("A")], "nextToken": "t1"}),
    );
    plane.reply(
        "/v1/runs",
        200,
        json!({"runs": [row("B")], "nextToken": "t2", "partial": true}),
    );
    plane.reply("/v1/runs", 200, json!({"runs": [row("C")]}));
    let api = scripted(&plane);
    let filter = RunFilter {
        q: Some("calc tests".into()),
        target: Some("cc:x".into()),
        status: Some(harness_protocol::goal::GoalStatus::Complete),
        since: Some("2026-10-01T00:00:00Z".into()),
    };
    let rows = api.run_rows_all(&filter).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.title.clone().unwrap())
            .collect::<Vec<_>>(),
        ["run A", "run B", "run C"]
    );
    let calls = plane.calls();
    assert_eq!(
        calls[0].1,
        "/v1/runs?q=calc%20tests&target=cc%3Ax&status=complete&since=2026-10-01T00%3A00%3A00Z"
    );
    assert_eq!(calls[1].1, format!("{}&nextToken=t1", calls[0].1));
    assert_eq!(calls[2].1, format!("{}&nextToken=t2", calls[0].1));
    // No filter, no query.
    plane.reply("/v1/runs", 200, json!({"runs": []}));
    api.run_rows(&RunFilter::default(), None).await.unwrap();
    assert_eq!(plane.calls()[3].1, "/v1/runs");
}

#[tokio::test]
async fn a_token_goes_on_the_wire_percent_encoded() {
    let plane = Canned::new(vec![]);
    plane.reply("/v1/runs", 200, json!({"runs": []}));
    scripted(&plane)
        .run_rows(&RunFilter::default(), Some("a+b/c="))
        .await
        .unwrap();
    assert_eq!(plane.calls()[0].1, "/v1/runs?nextToken=a%2Bb%2Fc%3D");
    plane.reply("/v1/runs/index", 200, json!({"indexed": 0, "alreadyIndexed": 0, "skipped": 0, "scanned": 0, "more": true, "nextToken": "x y"}));
    plane.reply(
        "/v1/runs/index",
        200,
        json!({"indexed": 0, "alreadyIndexed": 0, "skipped": 0, "scanned": 0, "more": false}),
    );
    scripted(&plane).backfill_run_index().await.unwrap();
    assert_eq!(plane.calls()[2].1, "/v1/runs/index?nextToken=x%20y");
}

#[tokio::test]
async fn a_walk_that_hits_its_page_cap_says_so_instead_of_returning_a_short_list() {
    let plane = Canned::new(vec![]);
    for i in 0..60 {
        plane.reply(
            "/v1/runs",
            200,
            json!({"runs": [], "nextToken": format!("t{i}")}),
        );
    }
    let api = scripted(&plane);
    let e = api.run_rows_all(&RunFilter::default()).await.unwrap_err();
    assert!(e.to_string().contains("50 pages"), "{e}");
    // The walk keeps what it read and the token to go on from.
    plane.replies.lock().unwrap().clear();
    for i in 0..60 {
        plane.reply(
            "/v1/runs",
            200,
            json!({"runs": [], "nextToken": format!("u{i}")}),
        );
    }
    let walk = api.run_rows_walk(&RunFilter::default(), 3).await.unwrap();
    assert_eq!(walk.next_token.as_deref(), Some("u2"));
}

#[tokio::test]
async fn a_token_that_repeats_ends_the_walk() {
    let plane = Canned::new(vec![]);
    for _ in 0..3 {
        plane.reply("/v1/runs", 200, json!({"runs": [], "nextToken": "same"}));
    }
    let e = scripted(&plane)
        .run_rows_all(&RunFilter::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("repeated"), "{e}");
    assert_eq!(
        plane.calls().len(),
        2,
        "the second page named the token the first one named"
    );
}

#[tokio::test]
async fn the_backfill_calls_again_until_the_server_says_it_is_done() {
    let plane = Canned::new(vec![]);
    let report = |indexed: u32, more: bool, tok: Option<&str>| {
        let mut v = json!({"indexed": indexed, "alreadyIndexed": 2, "skipped": 1, "scanned": 43, "more": more});
        if let Some(t) = tok {
            v["nextToken"] = json!(t);
        }
        v
    };
    plane.reply("/v1/runs/index", 200, report(40, true, Some("p2")));
    plane.reply("/v1/runs/index", 200, report(40, true, None));
    plane.reply("/v1/runs/index", 200, report(7, false, None));
    let total = scripted(&plane).backfill_run_index().await.unwrap();
    // Only the items made add up; the read counts are the last call's (a call that stops inside a page is followed by one that reads it again).
    assert_eq!((total.indexed, total.skipped, total.more), (87, 1, false));
    let calls = plane.calls();
    assert_eq!(calls[0], ("POST".into(), "/v1/runs/index".into()));
    assert_eq!(calls[1].1, "/v1/runs/index?nextToken=p2");
    assert_eq!(
        calls[2].1, "/v1/runs/index",
        "no token: the same page again"
    );
}

// ---- the snapshot ----------------------------------------------------------------------------------------------------

fn route() -> String {
    format!("/v1/runs/{RUN}/snapshot/download-url")
}

#[tokio::test]
async fn the_snapshot_is_downloaded_in_ranges_checked_and_named_for_its_run() {
    let object = bundle(HEAD, 5_000);
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    let dir = tmp("snap-ok");
    let got = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap();
    assert_eq!(got.path, dir.join(format!("{RUN}-snapshot-3.bundle")));
    assert_eq!(std::fs::read(&got.path).unwrap(), object);
    assert_eq!(got.head_sha, HEAD);
    assert_eq!(got.size_bytes, object.len() as u64);
    assert_eq!(got.count, Some(3));
    assert!(plane.fetches.lock().unwrap().len() >= 5, "ranges of 1 KiB");
    // Every range is asked for only while the object is still the one whose header the route checked.
    let tags = plane.if_matches.lock().unwrap().clone();
    assert!(
        tags.iter().all(|t| t.as_deref() == Some("\"abc\"")),
        "{tags:?}"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        leftovers,
        vec![format!("{RUN}-snapshot-3.bundle")],
        "no .part is left"
    );
    assert_eq!(plane.calls()[0], ("POST".into(), route()));
}

#[tokio::test]
async fn a_bundle_that_is_not_the_commit_the_route_named_is_refused_and_leaves_nothing() {
    let object = bundle(OTHER_HEAD, 3_000);
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    let dir = tmp("snap-head");
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert!(
        matches!(e, SnapshotFetchError::Verify(SnapshotError::Head { .. })),
        "{e}"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    let text = e.to_string();
    assert!(
        !text.contains("X-Amz") && !text.contains("SECRETTOKEN") && !text.contains("amazonaws"),
        "{text}"
    );
}

#[tokio::test]
async fn a_size_that_is_not_the_one_the_route_named_is_refused() {
    let object = bundle(HEAD, 3_000);
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64 + 10, HEAD));
    let dir = tmp("snap-size");
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert!(
        matches!(e, SnapshotFetchError::Verify(SnapshotError::Size { .. })),
        "{e}"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn an_object_that_changes_during_the_download_is_a_clear_failure() {
    let object = bundle(HEAD, 3_000);
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    // The store answers 412 to a ranged GET whose If-Match no longer holds: the object was replaced.
    plane.fetch_faults.lock().unwrap().push_back(412);
    let dir = tmp("snap-412");
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("changed during the download"), "{e}");
    assert!(!e.to_string().contains("X-Amz"));
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "a failed download leaves no .part"
    );
}

#[tokio::test]
async fn a_route_that_says_a_snapshot_is_being_saved_is_asked_again() {
    let object = bundle(HEAD, 100);
    let plane = Canned::new(object.clone());
    let updating = json!({"code": "snapshot-updating", "message": "a new snapshot is being saved; ask again in a few seconds"});
    plane.reply(&route(), 409, updating.clone());
    plane.reply(&route(), 409, updating);
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    let dir = tmp("snap-409");
    let got = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap();
    assert_eq!(got.head_sha, HEAD);
    assert_eq!(plane.calls().len(), 3);
}

#[tokio::test]
async fn a_route_that_keeps_saying_so_gives_up_with_its_message() {
    let plane = Canned::new(vec![]);
    for _ in 0..6 {
        plane.reply(
            &route(),
            409,
            json!({"code": "snapshot-updating", "message": "a new snapshot is being saved"}),
        );
    }
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &tmp("snap-409b"), 1024)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &e,
            SnapshotFetchError::Api(ApiError::Http { status: 409, .. })
        ),
        "{e}"
    );
    assert!(plane.calls().len() <= 5);
}

#[tokio::test]
async fn no_snapshot_yet_is_the_servers_plain_sentence() {
    let plane = Canned::new(vec![]);
    plane.reply(
        &route(),
        404,
        json!({"code": "no-snapshot", "message": "no snapshot has been saved for this run yet; agentd saves one about every 10 minutes while the files change"}),
    );
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &tmp("snap-404"), 1024)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no snapshot has been saved"), "{e}");
}

#[tokio::test]
async fn an_expired_url_is_replaced_once() {
    let object = bundle(HEAD, 2_000);
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    plane.fetch_faults.lock().unwrap().push_back(403);
    let got = fetch_snapshot_with(&scripted(&plane), RUN, &tmp("snap-403"), 1024)
        .await
        .unwrap();
    assert_eq!(got.head_sha, HEAD);
    assert_eq!(plane.calls().len(), 2);
    // A fresh URL that is refused too is an error that names no URL.
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    plane.fetch_faults.lock().unwrap().extend([403, 403]);
    let dir = tmp("snap-403b");
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("403"), "{e}");
    assert!(!e.to_string().contains("X-Amz"), "{e}");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "a failed download leaves no .part"
    );
}

#[tokio::test]
async fn a_renewed_url_must_be_for_the_same_object() {
    let object = bundle(HEAD, 2_000);
    let plane = Canned::new(object.clone());
    plane.reply(&route(), 200, dl(object.len() as u64, HEAD));
    // The renewal names another bundle (another commit, another tag): bytes already written would be joined to a different object.
    let mut other = dl(object.len() as u64, OTHER_HEAD);
    other["etag"] = json!("\"zzz\"");
    plane.reply(&route(), 200, other);
    plane.fetch_faults.lock().unwrap().push_back(403);
    let dir = tmp("snap-renew");
    let e = fetch_snapshot_with(&scripted(&plane), RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("changed"), "{e}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn a_download_url_for_another_host_is_never_fetched() {
    let object = bundle(HEAD, 100);
    for url in [
        "https://other.example.com/runs/x/snapshot.bundle?X-Amz-Signature=deadbeef",
        "ftp://bucket.s3.us-east-1.amazonaws.com/x",
        "https://amazonaws.com.other.example/x",
    ] {
        let plane = Canned::new(object.clone());
        let mut d = dl(object.len() as u64, HEAD);
        d["url"] = json!(url);
        plane.reply(&route(), 200, d);
        let e = fetch_snapshot_with(&scripted(&plane), RUN, &tmp("snap-host"), 1024)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("url"), "{url}: {e}");
        assert!(!e.to_string().contains("deadbeef"), "{e}");
        assert!(
            plane.fetches.lock().unwrap().is_empty(),
            "{url} was fetched"
        );
    }
}

// ---- through the real handler -------------------------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(fast())
}

async fn start(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    // An Idempotency-Key is 1 to 128 visible ASCII characters: the prompt's own letters only.
    let key: String = prompt.chars().filter(char::is_ascii_alphanumeric).collect();
    api(fake)
        .start_direct_run(&req, &format!("k-{key}"))
        .await
        .unwrap()
        .run_id
}

#[tokio::test]
async fn started_runs_are_listed_with_their_title_and_model_and_found_by_q() {
    let fake = Fake::start().await;
    let a = start(&fake, "Add a percent_change function\nwith tests").await;
    let _b = start(&fake, "Write a markdown parser").await;
    let rows = api(&fake)
        .run_rows_all(&RunFilter::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let ra = rows.iter().find(|r| r.summary.run_id == a).unwrap();
    assert_eq!(ra.title.as_deref(), Some("Add a percent_change function"));
    assert_eq!(ra.model.as_deref(), Some(support::MODEL));
    let found = api(&fake)
        .run_rows_all(&RunFilter {
            q: Some("PERCENT".into()),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].summary.run_id, a);
}

#[tokio::test]
async fn a_run_from_before_the_index_is_found_after_the_backfill() {
    let fake = Fake::start().await;
    let a = start(&fake, "Add a percent_change function").await;
    fake.mem.inner.lock().unwrap().index.clear();
    let q = RunFilter {
        q: Some("percent".into()),
        ..RunFilter::default()
    };
    assert!(api(&fake).run_rows_all(&q).await.unwrap().is_empty());
    let report = api(&fake).backfill_run_index().await.unwrap();
    assert_eq!(report.indexed, 1);
    let found = api(&fake).run_rows_all(&q).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].summary.run_id, a);
}

#[tokio::test]
async fn the_tail_of_a_run_and_the_users_answer_arrive_through_the_real_route() {
    let fake = Fake::start().await;
    let run = start(&fake, "Add a percent_change function").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    for i in 0..60 {
        fake.mem.agent(
            &run,
            "direct",
            EventType::TurnProgress,
            json!({"turn": 1, "kind": "message", "text": format!("m{i}")}),
        );
    }
    let request = fake.mem.needs_input(&run, "engine_stop");
    let shard = event_shard(harness_protocol::envelope::Authority::Agent, Some("direct"));
    let a = api(&fake);
    let answer: harness_protocol::api::v1::InputAnswer = serde_json::from_value(
        json!({"requestId": request, "action": "continue_with_prompt", "text": "Poll the log instead."}),
    )
    .unwrap();
    a.answer_input(&run, &answer).await.unwrap();
    let view = a
        .events_view(&run, &EventsQuery::tail(shard, 10))
        .await
        .unwrap();
    assert_eq!(view.events.len(), 10);
    assert!(view.has_more);
    assert_eq!(view.answers.len(), 1, "{:?}", view.answers);
    assert_eq!(
        view.answers[0].text.as_deref(),
        Some("Poll the log instead.")
    );
    let older = a
        .events_view(
            &run,
            &EventsQuery::older(shard, view.next_before.as_deref().unwrap(), 10),
        )
        .await
        .unwrap();
    assert_eq!(older.events.len(), 10);
    assert!(older.events.last().unwrap().event_id < view.events[0].event_id);
}

#[tokio::test]
async fn the_snapshot_of_a_live_run_downloads_and_verifies_through_the_real_route() {
    let fake = Fake::start().await;
    let run = start(&fake, "Add a percent_change function").await;
    let object = bundle(HEAD, 40_000);
    {
        let mut g = fake.mem.inner.lock().unwrap();
        let prefix = harness_protocol::direct::s3::turn_prefix(&run);
        g.objects.insert(
            format!("{prefix}snapshot.bundle"),
            (object.clone(), "\"etag-b\"".into()),
        );
        g.objects.insert(
            format!("{prefix}snapshot.json"),
            (
                json!({"schemaVersion": 1, "headSha": HEAD, "baseSha": BASE, "at": "2026-10-15T13:10:00Z", "count": 3})
                    .to_string()
                    .into_bytes(),
                "\"etag-j\"".into(),
            ),
        );
    }
    let dir = tmp("snap-real");
    let got = fetch_snapshot_with(&api(&fake), &run, &dir, 16 * 1024)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&got.path).unwrap(), object);
    assert_eq!(got.head_sha, HEAD);
    assert_eq!(got.count, Some(3));
    // The route presigned the bundle for 15 minutes and nothing else.
    let signed = fake.mem.inner.lock().unwrap().presigned.clone();
    assert_eq!(
        signed,
        vec![(
            format!(
                "{}snapshot.bundle",
                harness_protocol::direct::s3::turn_prefix(&run)
            ),
            control_api::handler::DOWNLOAD_URL_TTL
        )]
    );
    // No snapshot on a run that has none: the server's sentence.
    let other = start(&fake, "Another run").await;
    let e = fetch_snapshot_with(&api(&fake), &other, &tmp("snap-none"), 1024)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no copy of this run's work"), "{e}");
}
