//! The S3 side of the transcript writer: a write-once put (`If-None-Match: *`, signed with the session
//! credentials), a listing after a key, and a ranged read of an object's first bytes, against an in-process stand-in that answers
//! 412, 409 and 5xx the way S3 does. The stand-in refuses an unconditional put to a chunk key, as the bucket policy will.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::creds::{CredStore, RefreshContext, Refresher};
use agentd::direct::transcript::s3store::S3Store;
use agentd::direct::transcript::store::{ChunkStore, PutOutcome};
use agentd::time::SystemClock;
use agentd::{BoxFuture, time::format_rfc3339, time::now_unix};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use harness_protocol::api::internal::{CredsRequest, CredsResponse, ProcessCredentials};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BUCKET: &str = "hamh-direct-harness-111111111111";
const PREFIX: &str = "runs/R1/agents/direct/turns/1/transcript/";

#[derive(Default)]
struct Fake {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Every request: method, path, the headers a test looks at.
    seen: Mutex<Vec<Seen>>,
    /// The next this many requests answer this status with an S3 error document.
    fail: Mutex<Vec<(u16, &'static str)>>,
    /// Keys listed per page.
    page: Mutex<usize>,
}

#[derive(Clone, Debug)]
struct Seen {
    method: &'static str,
    path: String,
    if_none_match: Option<String>,
    authorization: Option<String>,
    token: Option<String>,
    content_type: Option<String>,
    range: Option<String>,
    query: BTreeMap<String, String>,
}

fn note(
    f: &Fake,
    method: &'static str,
    path: &str,
    h: &HeaderMap,
    query: BTreeMap<String, String>,
) {
    let g = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    f.seen.lock().unwrap().push(Seen {
        method,
        path: path.to_string(),
        if_none_match: g("if-none-match"),
        authorization: g("authorization"),
        token: g("x-amz-security-token"),
        content_type: g("content-type"),
        range: g("range"),
        query,
    });
}

fn failure(f: &Fake) -> Option<axum::response::Response> {
    let mut q = f.fail.lock().unwrap();
    if q.is_empty() {
        return None;
    }
    let (status, code) = q.remove(0);
    Some(
        (
            StatusCode::from_u16(status).unwrap(),
            format!("<Error><Code>{code}</Code><Message>stand-in</Message></Error>"),
        )
            .into_response(),
    )
}

async fn put_object(
    State(f): State<Arc<Fake>>,
    Path((bucket, key)): Path<(String, String)>,
    h: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    note(&f, "PUT", &format!("{bucket}/{key}"), &h, BTreeMap::new());
    if let Some(r) = failure(&f) {
        return r;
    }
    let mut o = f.objects.lock().unwrap();
    match h.get("if-none-match").and_then(|v| v.to_str().ok()) {
        Some("*") if o.contains_key(&key) => {
            (StatusCode::PRECONDITION_FAILED, "<Error><Code>PreconditionFailed</Code><Message>At least one of the pre-conditions you specified did not hold</Message></Error>").into_response()
        }
        Some("*") => {
            o.insert(key, body.to_vec());
            StatusCode::OK.into_response()
        }
        // the bucket policy of the design refuses a write to a chunk key without the condition
        _ => (StatusCode::FORBIDDEN, "<Error><Code>AccessDenied</Code><Message>no condition</Message></Error>").into_response(),
    }
}

async fn get_object(
    State(f): State<Arc<Fake>>,
    Path((bucket, key)): Path<(String, String)>,
    h: HeaderMap,
) -> axum::response::Response {
    note(&f, "GET", &format!("{bucket}/{key}"), &h, BTreeMap::new());
    if let Some(r) = failure(&f) {
        return r;
    }
    let o = f.objects.lock().unwrap();
    let Some(b) = o.get(&key) else {
        return (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchKey</Code><Message>x</Message></Error>",
        )
            .into_response();
    };
    match h
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes=0-"))
        .and_then(|n| n.parse::<usize>().ok())
    {
        Some(end) => (
            StatusCode::PARTIAL_CONTENT,
            b[..(end + 1).min(b.len())].to_vec(),
        )
            .into_response(),
        None => (StatusCode::OK, b.clone()).into_response(),
    }
}

/// A query string as a map, percent-decoded.
fn parse_query(raw: &str) -> BTreeMap<String, String> {
    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 3 <= b.len()
                && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

async fn list_objects(
    State(f): State<Arc<Fake>>,
    Path(bucket): Path<String>,
    RawQuery(raw): RawQuery,
    h: HeaderMap,
) -> axum::response::Response {
    let q = parse_query(raw.as_deref().unwrap_or_default());
    note(&f, "GET", &bucket, &h, q.clone());
    if let Some(r) = failure(&f) {
        return r;
    }
    assert_eq!(q.get("list-type").map(String::as_str), Some("2"));
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let after = q
        .get("continuation-token")
        .or_else(|| q.get("start-after"))
        .cloned()
        .unwrap_or_default();
    let page = (*f.page.lock().unwrap()).max(1);
    let o = f.objects.lock().unwrap();
    let all: Vec<&String> = o
        .keys()
        .filter(|k| k.starts_with(&prefix) && **k > after)
        .collect();
    let shown: Vec<&&String> = all.iter().take(page).collect();
    let more = all.len() > page;
    let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
    xml.push_str(&format!("<IsTruncated>{more}</IsTruncated>"));
    let escape = |k: &str| {
        k.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    for k in &shown {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key><Size>1</Size></Contents>",
            escape(k)
        ));
    }
    if more {
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            escape(shown.last().unwrap())
        ));
    }
    xml.push_str("</ListBucketResult>");
    (StatusCode::OK, xml).into_response()
}

struct NoRefresh;
impl Refresher for NoRefresh {
    fn refresh<'a>(
        &'a self,
        _c: &'a RefreshContext,
        _cur: &'a ProcessCredentials,
        _r: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        Box::pin(async { Err("no refresh in this test".to_string()) })
    }
}

async fn rig() -> (Arc<Fake>, S3Store) {
    let fake = Arc::new(Fake {
        page: Mutex::new(1000),
        ..Fake::default()
    });
    let app = Router::new()
        .route("/{bucket}/{*key}", put(put_object).get(get_object))
        .route("/{bucket}", get(list_objects))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let creds = CredStore::new(Arc::new(NoRefresh), Arc::new(SystemClock));
    let ctx = RefreshContext {
        run_id: "R1".into(),
        agent_id: "direct".into(),
        runtime_session_id: "s".into(),
        internal_api_url: "http://127.0.0.1:1".into(),
        region: "us-east-1".into(),
    };
    creds
        .install(
            ctx,
            common::creds(&format_rfc3339(now_unix() + 3600)),
            "nonce".into(),
        )
        .await;
    let store = S3Store::new(
        creds,
        "us-east-1".into(),
        Some(format!("http://{addr}")),
        BUCKET.into(),
    );
    (fake, store)
}

#[tokio::test]
async fn a_put_is_conditional_signed_and_refused_the_second_time() {
    let (fake, store) = rig().await;
    let key = format!("{PREFIX}000000000000.ndjson.gz");
    assert_eq!(
        store.put_new(&key, b"first".to_vec()).await,
        PutOutcome::Stored
    );
    assert_eq!(
        store.put_new(&key, b"second".to_vec()).await,
        PutOutcome::Exists,
        "412 is Exists"
    );
    assert_eq!(
        fake.objects.lock().unwrap()[&key],
        b"first".to_vec(),
        "never overwritten"
    );
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    for s in &seen {
        assert_eq!(s.method, "PUT");
        assert_eq!(s.path, format!("{BUCKET}/{key}"));
        assert_eq!(s.if_none_match.as_deref(), Some("*"));
        assert_eq!(s.content_type.as_deref(), Some("application/gzip"));
        let auth = s.authorization.as_deref().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential="), "{auth}");
        let signed = auth
            .split("SignedHeaders=")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap();
        assert!(
            signed.split(';').any(|h| h == "if-none-match"),
            "the condition is signed: {signed}"
        );
        assert!(
            signed.split(';').any(|h| h == "x-amz-security-token"),
            "{signed}"
        );
        assert_eq!(s.token.as_deref(), Some(common::CANARY_TOKEN));
    }
}

#[tokio::test]
async fn failures_are_failed_with_the_status_and_never_with_a_secret() {
    let (fake, store) = rig().await;
    let key = format!("{PREFIX}000000000001.ndjson.gz");
    for (status, code) in [
        (503, "SlowDown"),
        (500, "InternalError"),
        (409, "ConditionalRequestConflict"),
        (403, "AccessDenied"),
    ] {
        fake.fail.lock().unwrap().push((status, code));
        match store.put_new(&key, b"x".to_vec()).await {
            PutOutcome::Failed(why) => {
                assert!(why.contains(&status.to_string()), "{why}");
                assert!(
                    !why.contains(common::CANARY_SECRET) && !why.contains(common::CANARY_TOKEN),
                    "{why}"
                );
            }
            other => panic!("{status}: {other:?}"),
        }
    }
    assert!(fake.objects.lock().unwrap().is_empty());
    // a server that is not there
    let creds = CredStore::new(Arc::new(NoRefresh), Arc::new(SystemClock));
    creds
        .install(
            RefreshContext {
                run_id: "R1".into(),
                agent_id: "direct".into(),
                runtime_session_id: "s".into(),
                internal_api_url: String::new(),
                region: "us-east-1".into(),
            },
            common::creds(&format_rfc3339(now_unix() + 3600)),
            "n".into(),
        )
        .await;
    let dead = S3Store::new(
        creds,
        "us-east-1".into(),
        Some("http://127.0.0.1:1".into()),
        BUCKET.into(),
    );
    assert!(matches!(
        dead.put_new(&key, b"x".to_vec()).await,
        PutOutcome::Failed(_)
    ));
    assert!(dead.list(PREFIX, None).await.is_err());
    assert!(dead.head(&key, 10).await.is_err());
}

#[tokio::test]
async fn a_listing_follows_the_pages_and_starts_after_a_key() {
    let (fake, store) = rig().await;
    *fake.page.lock().unwrap() = 3;
    for i in 0..10u64 {
        fake.objects
            .lock()
            .unwrap()
            .insert(format!("{PREFIX}{i:012}.ndjson.gz"), vec![1]);
    }
    fake.objects
        .lock()
        .unwrap()
        .insert("runs/R1/agents/direct/turns/1/summary.json".into(), vec![1]);
    let all = store.list(PREFIX, None).await.unwrap();
    assert_eq!(all.len(), 10, "only the keys under the prefix");
    assert_eq!(all[0], format!("{PREFIX}000000000000.ndjson.gz"));
    assert_eq!(all[9], format!("{PREFIX}000000000009.ndjson.gz"));
    let after = store
        .list(PREFIX, Some(&format!("{PREFIX}000000000006.ndjson.gz")))
        .await
        .unwrap();
    assert_eq!(after.len(), 3);
    assert_eq!(after[0], format!("{PREFIX}000000000007.ndjson.gz"));
    let seen = fake.seen.lock().unwrap().clone();
    let first = &seen[0];
    assert_eq!(
        first.query.get("prefix").map(String::as_str),
        Some(PREFIX),
        "the prefix reaches S3 decoded, so it was encoded on the wire"
    );
    assert_eq!(first.query.get("list-type").map(String::as_str), Some("2"));
    assert!(
        seen.len() >= 4,
        "10 keys at 3 per page: {} calls",
        seen.len()
    );
    assert!(seen.iter().all(|s| s.authorization.is_some()));
}

#[tokio::test]
async fn a_listing_stops_at_the_cap_of_keys() {
    let (fake, store) = rig().await;
    *fake.page.lock().unwrap() = 1000;
    for i in 0..21_500u64 {
        fake.objects
            .lock()
            .unwrap()
            .insert(format!("{PREFIX}{i:012}.ndjson.gz"), vec![]);
    }
    let all = store.list(PREFIX, None).await.unwrap();
    assert_eq!(all.len(), 20_000, "the writer reads at most 20,000 keys");
}

#[tokio::test]
async fn a_head_reads_the_first_bytes_by_range_and_a_missing_key_is_an_error() {
    let (fake, store) = rig().await;
    let key = format!("{PREFIX}000000000002.ndjson.gz");
    fake.objects
        .lock()
        .unwrap()
        .insert(key.clone(), (0u8..=255).collect());
    let got = store.head(&key, 16).await.unwrap();
    assert_eq!(got, (0u8..16).collect::<Vec<u8>>());
    let seen = fake.seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(seen.range.as_deref(), Some("bytes=0-15"));
    assert!(
        seen.authorization.unwrap().contains("range"),
        "the range is signed"
    );
    assert!(
        store
            .head(&format!("{PREFIX}000000000099.ndjson.gz"), 16)
            .await
            .is_err()
    );
    // a short object is returned whole
    let small = format!("{PREFIX}000000000003.ndjson.gz");
    fake.objects
        .lock()
        .unwrap()
        .insert(small.clone(), vec![7, 8, 9]);
    assert_eq!(store.head(&small, 1000).await.unwrap(), vec![7, 8, 9]);
}

#[tokio::test]
async fn keys_with_odd_characters_are_listed_as_they_are() {
    let (fake, store) = rig().await;
    let odd = format!("{PREFIX}a&b<c>.ndjson.gz");
    fake.objects.lock().unwrap().insert(odd.clone(), vec![]);
    // S3 escapes the five XML entities in a key; the store gives the key back as it is
    let keys = store.list(PREFIX, None).await.unwrap();
    assert_eq!(keys, vec![odd]);
}
