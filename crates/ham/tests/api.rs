//! `api::Api` over a scripted transport: page tokens, API Gateway's own error bodies, bodies that
//! do not decode, and the retry classification.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Answers calls from a script, recording each path.
#[derive(Default)]
struct Scripted {
    answers: Mutex<VecDeque<Result<Response, TransportError>>>,
    paths: Mutex<Vec<String>>,
}

impl Scripted {
    fn with(answers: Vec<Result<Response, TransportError>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            paths: Mutex::default(),
        })
    }
}

fn ok(status: u16, body: &str) -> Result<Response, TransportError> {
    Ok(Response {
        status,
        headers: Default::default(),
        body: body.as_bytes().to_vec(),
    })
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
        attempts: 3,
        base: Duration::from_millis(1),
    })
}

fn run(id: &str) -> String {
    format!(
        r#"{{"runId": "{id}", "status": "active", "phase": "planning", "target": {{"mode": "s3-only"}},
        "createdAt": "2026-09-25T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"}}"#
    )
}

#[tokio::test]
async fn every_page_of_runs_is_read_and_a_repeated_token_ends_the_list() {
    let p = Scripted::with(vec![
        ok(
            200,
            &format!(r#"{{"runs": [{}], "nextToken": "t1"}}"#, run("R2")),
        ),
        ok(
            200,
            &format!(r#"{{"runs": [{}], "nextToken": "t1"}}"#, run("R1")),
        ),
    ]);
    let runs = api(&p).list_runs().await.unwrap();
    let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["R2", "R1"]);
    assert_eq!(
        *p.paths.lock().unwrap(),
        ["/v1/runs", "/v1/runs?nextToken=t1"]
    );
}

#[tokio::test]
async fn api_gateway_errors_keep_their_message_and_classify_for_retries() {
    let p = Scripted::with(vec![ok(
        403,
        r#"{"message": "User: arn:aws:sts::1:assumed-role/A/b is not authorized"}"#,
    )]);
    let e = api(&p).get_run("R1").await.unwrap_err();
    assert_eq!(e.status(), Some(403));
    assert!(!e.is_retryable());
    assert!(e.to_string().contains("is not authorized"), "{e}");

    let p = Scripted::with(vec![ok(502, ""), ok(502, ""), ok(502, "")]);
    let e = api(&p).get_run("R1").await.unwrap_err();
    assert!(e.is_retryable());
    assert!(
        e.to_string().contains("HTTP 502 without an error body"),
        "{e}"
    );
    assert_eq!(p.paths.lock().unwrap().len(), 3, "three attempts");

    let p = Scripted::with(vec![ok(200, "{not json")]);
    let e = api(&p).get_run("R1").await.unwrap_err();
    assert!(matches!(e, ApiError::Decode { .. }), "{e:?}");
    assert!(!e.is_retryable());
    assert_eq!(e.status(), None);

    let p = Scripted::with(vec![Err(TransportError::Refused("bad".into()))]);
    let e = api(&p).get_run("R1").await.unwrap_err();
    assert!(!e.is_retryable());
    assert_eq!(p.paths.lock().unwrap().len(), 1);
}

#[test]
fn debug_output_names_no_credential() {
    let creds = aws_credential_types::Credentials::new(
        "AKIDTESTONLY",
        "test-secret-not-a-key",
        Some("test-session-token-value".into()),
        None,
        "t",
    );
    let plane = HttpsPlane::new(
        "https://abc.execute-api.us-west-2.amazonaws.com/live",
        Signer::new("us-west-2", creds),
    )
    .unwrap();
    let a = Api::new(Arc::new(plane));
    let text = format!("{a:?} {:?}", a.plane());
    assert!(
        text.contains("us-west-2") && text.contains("execute-api"),
        "{text}"
    );
    for secret in ["test-secret-not-a-key", "test-session-token-value"] {
        assert!(!text.contains(secret), "{text}");
    }
}

// ---- a client that meets an event type it does not know keeps reading (second review of the merged side) ----
//
// `EventType` has no catch-all and `EventsPage` decodes strictly, so one `context.changed` or `rung.skipped`
// on a page of an older client gave `ApiError::Decode`, which is not retried: that shard's cursor never moved
// and the shard's `run.completed` stayed hidden. The server already skips items it cannot parse so they cannot
// stall a cursor; the client now does the same for a type it has no name for and moves the cursor past it.
// Only a type nobody knows is skipped: a malformed event of a known type is still an error.

use ham_core::state::RunState;

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";

fn event_json(id: u32, ty: &str, authority: &str) -> String {
    format!(
        r#"{{"schemaVersion": 1, "eventId": "01J9ZZZZZZ00000000000000{id:02}", "runId": "{RUN}", "seq": {id},
        "ts": "2026-10-01T12:00:00Z", "type": "{ty}", "authority": "{authority}",
        "payload": {{"kind": "inline", "value": {{}}}}}}"#
    )
}

fn page(events: &[String], next_after: Option<&str>) -> String {
    let next = next_after.map_or(String::new(), |n| format!(r#", "nextAfter": "{n}""#));
    format!(r#"{{"events": [{}]{next}}}"#, events.join(","))
}

#[tokio::test]
async fn an_unknown_event_type_is_skipped_and_the_cursor_moves_past_it() {
    let events = [
        event_json(1, "run.created", "harness"),
        event_json(2, "future.thing", "harness"),
        event_json(3, "run.completed", "harness"),
    ];
    let p = Scripted::with(vec![ok(
        200,
        &page(&events, Some("01J9ZZZZZZ0000000000000003")),
    )]);
    let got = api(&p).events(RUN, 0, None).await.unwrap();
    let types: Vec<&str> = got.events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, ["run.created", "run.completed"]);
    assert_eq!(
        got.next_after.as_deref(),
        Some("01J9ZZZZZZ0000000000000003")
    );

    // Only unknown events on the page, and the server's cursor past them: nothing to show, cursor moved.
    let p = Scripted::with(vec![ok(
        200,
        &page(
            &[
                event_json(4, "future.thing", "harness"),
                event_json(5, "another.one", "agent"),
            ],
            Some("01J9ZZZZZZ0000000000000005"),
        ),
    )]);
    let got = api(&p)
        .events(RUN, 0, Some("01J9ZZZZZZ0000000000000003"))
        .await
        .unwrap();
    assert!(got.events.is_empty());
    assert_eq!(
        got.next_after.as_deref(),
        Some("01J9ZZZZZZ0000000000000005")
    );

    // The server leaves the cursor out: it is the largest id of what was read, skipped events included.
    let p = Scripted::with(vec![ok(
        200,
        &page(&[event_json(6, "future.thing", "harness")], None),
    )]);
    let got = api(&p).events(RUN, 0, None).await.unwrap();
    assert_eq!(
        got.next_after.as_deref(),
        Some("01J9ZZZZZZ0000000000000006")
    );
}

#[tokio::test]
async fn a_malformed_event_of_a_known_type_is_still_an_error() {
    // An unknown authority on a known type is corrupt data, not a newer harness.
    let bad = event_json(2, "run.completed", "somebody-else");
    let p = Scripted::with(vec![ok(200, &page(&[bad], None))]);
    assert!(matches!(
        api(&p).events(RUN, 0, None).await,
        Err(ApiError::Decode { .. })
    ));
    // So is an item that is not an object with a type at all.
    let p = Scripted::with(vec![ok(200, r#"{"events": [42]}"#)]);
    assert!(matches!(
        api(&p).events(RUN, 0, None).await,
        Err(ApiError::Decode { .. })
    ));
}

/// Answers `GET .../events?shard=N` from a table of pages by shard; every other shard is empty.
struct ByShard {
    pages: std::collections::BTreeMap<u8, String>,
}

impl ControlPlane for ByShard {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let shard: u8 = path
            .split("shard=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = self
            .pages
            .get(&shard)
            .cloned()
            .unwrap_or_else(|| r#"{"events": []}"#.to_string());
        Ok(Response {
            status: 200,
            headers: Default::default(),
            body: body.into_bytes(),
        })
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

#[tokio::test]
async fn a_poll_over_a_shard_with_an_unknown_event_reads_the_events_behind_it() {
    // Shard 0 holds harness events: run.created, an event of a type this client does not know, and
    // run.completed. An older client stalled on the second and never saw the third.
    let events = [
        event_json(1, "run.created", "harness"),
        event_json(2, "rung.skipped.v2", "harness"),
        event_json(3, "run.completed", "harness"),
    ];
    let plane = Arc::new(ByShard {
        pages: [(0u8, page(&events, Some("01J9ZZZZZZ0000000000000003")))].into(),
    });
    let a = Api::new(plane).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let mut state = RunState::new(RUN);
    a.poll(&mut state).await.unwrap();
    assert_eq!(state.cursor(0), Some("01J9ZZZZZZ0000000000000003"));
    let v = state.view();
    assert!(v.completed, "run.completed was read: {v:?}");
    assert_eq!(v.events, 2, "the unknown event is not counted as accepted");
}
