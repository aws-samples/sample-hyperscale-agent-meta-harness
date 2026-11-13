//! Shared harness of the direct driver's end-to-end tests: a fake AWS (path-style S3 and the private internal API in one
//! in-process server), an in-process agentd serving `/invocations`, the fake ACP engine behind a wrapper script, and the
//! artifacts a real run has (`repo.bundle`, `request.json`). No AWS call is made.
#![allow(dead_code, clippy::unwrap_used)]

use crate::common::{
    CANARY_NONCE, CANARY_SECRET, CANARY_TOKEN, creds, fake_engine_dir, git, python_calc, repo_root,
    scratch, unique_suffix,
};
use agentd::config::{Config, DirectConfig};
use agentd::logging::MemoryLog;
use agentd::server::{App, serve};
use agentd::time::{format_rfc3339, now_unix};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use harness_protocol::api::internal::{
    CredsResponse, EventsBatchRequest, EventsBatchResponse, HeartbeatRequest, PendingControl,
    ProcessCredentials, TurnCompleteRequest,
};
use harness_protocol::api::v1::{ContinueSettings, RuleFile};
use harness_protocol::common::Protocol;
use harness_protocol::direct::{
    DirectInvocationRequest, DirectKind, DirectModelSpec, DirectSummary, InputRef, InstructAction,
    InstructOrigin, s3 as keys,
};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::runtime::{
    InvocationAccepted, InvocationResponse, PingResponse, PingStatus, RepoSource, turn_result_path,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BUCKET: &str = "hamh-direct-harness-111111111111";
pub const RUN: &str = "01M4XTESTRUNDIRECT00000001";
pub const MODEL: &str = "us.anthropic.claude-opus-5-5";
pub const PROMPT: &str =
    "Add a percent_change(a, b) function with tests\n  (typed as is, \u{e9} \u{1F600})\n";

pub fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub fn rule(name: &str, content: &str) -> RuleFile {
    RuleFile {
        path: name.into(),
        sha256: sha(content.as_bytes()),
        content: content.into(),
    }
}

// ---- the fake AWS ------------------------------------------------------------------------------------------------------------

/// How a fake S3 PUT that is made to fail ends (`FakeAws::fail_puts_after_store`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutFailure {
    /// A 500 answer with an S3 error document.
    Status500,
    /// The connection breaks while the answer is being sent (the client sees a transport error, not a status).
    ResetBody,
    /// The answer never comes (the client's own time limit ends the request).
    Hang,
}

/// One scheduled failure of the fake S3: the next `left` PUTs whose bucket/key contains `substring` (and, for `only_unconditional`, carry no
/// `If-None-Match`) end as `how`. With `after_store` the body is stored first, so the object is committed and only the answer is lost (the
/// case a real S3 produces when a connection drops after the commit); `overwrite_after_store` then replaces that object with other bytes,
/// as a foreign writer would between the lost answer and the retry.
struct PutFault {
    substring: String,
    left: u32,
    how: PutFailure,
    after_store: bool,
    only_unconditional: bool,
    overwrite_after_store: Option<Vec<u8>>,
}

/// A body whose first frame is an error: hyper breaks the connection instead of finishing the answer.
struct ResetBody;

impl hyper::body::Body for ResetBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
        std::task::Poll::Ready(Some(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "the fake S3 reset the connection",
        ))))
    }
}

/// Standard base64 with padding, written here and not taken from the product so that a mistake in the product's encoder shows.
pub fn b64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| {
            acc | (u32::from(*b) << (16 - 8 * u32::try_from(i).unwrap()))
        });
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The SHA-256 checksum header value S3 expects: base64 of the 32-byte digest.
pub fn sha256_b64(bytes: &[u8]) -> String {
    b64(&Sha256::digest(bytes))
}

/// Every version of each key (`bucket/key`), oldest first, as (version id, body).
type Versions = BTreeMap<String, Vec<(String, Vec<u8>)>>;

#[derive(Default)]
pub struct AwsState {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Every version a key has had, oldest first, as (version id, body). The object in `objects` is the newest of them, as in a versioned
    /// bucket.
    versions: Mutex<Versions>,
    /// An unversioned bucket: the answers to PUT and HEAD carry no `x-amz-version-id`.
    no_version_ids: Mutex<bool>,
    /// The `x-amz-checksum-sha256` each object was stored with (S3 keeps the value it validated). An object put through
    /// `FakeAws::put` has none, like an object a foreign writer stored without a checksum.
    checksums: Mutex<BTreeMap<String, String>>,
    put_attempts: Mutex<BTreeMap<String, u32>>,
    put_failures: Mutex<Vec<(String, u32)>>,
    put_faults: Mutex<Vec<PutFault>>,
    head_attempts: Mutex<BTreeMap<String, u32>>,
    /// When set, every HEAD answers this status with no body (a 403 for a key the role may not list, a 500).
    head_status: Mutex<Option<u16>>,
    events: Mutex<Vec<Envelope>>,
    batches: Mutex<Vec<(u32, Option<bool>)>>,
    heartbeats: Mutex<Vec<HeartbeatRequest>>,
    complete: Mutex<Vec<TurnCompleteRequest>>,
    cancel_on: Mutex<Option<EventType>>,
    cancel_sent: Mutex<bool>,
    /// `(substring of bucket/key, pause)`: a GET whose key contains the substring is answered after the pause.
    get_delays: Mutex<Vec<(String, Duration)>>,
    /// The next this-many `/internal/turn-complete` posts answer 500.
    complete_failures: Mutex<u32>,
    complete_attempts: Mutex<u32>,
    /// Controls the fake plane hands out once enough events of one progress kind have been posted (user answers and the like).
    control_rules: Mutex<Vec<ControlRule>>,
    /// The next this-many events responses that would carry a control are garbage instead (the lost response).
    swallow: Mutex<u32>,
    /// How many control-bearing responses were swallowed so far.
    swallowed: Mutex<u32>,
    /// Every control the plane put in a response that agentd could read, in order.
    sent_controls: Mutex<Vec<PendingControl>>,
    /// Raw JSON items for the pending list that agentd cannot parse (a control of a future kind): `(progress kind, item, sent)`. Each goes out
    /// once, in front of the readable controls of the response that carries the first control after an event of that kind.
    raw_controls: Mutex<Vec<(String, serde_json::Value, bool)>>,
    /// What `/internal/creds` answers (`FakeAws::refresh_answers`); `None` is today's answer: the canary credentials for another hour and
    /// the nonce `next`.
    refresh: Mutex<Option<CredsResponse>>,
    /// How many `/internal/creds` posts arrived (a refresh of the session credentials).
    refreshes: Mutex<u32>,
    /// The completion key each `/internal/events` and `/internal/turn-complete` post carried: `(path, key)`.
    completion_keys: Mutex<Vec<(String, Option<String>)>>,
}

/// Picks the event a control rule waits for.
pub type EventMatcher = Arc<dyn Fn(&Envelope) -> bool + Send + Sync>;

/// One control rule of the fake plane. An `Instruct` whose `request_id` is empty gets the id of the event that triggered the rule,
/// because the id of a `needs-input` event is the request id the answer must name.
struct ControlRule {
    /// The `kind` member of the inline payload of a `turn.progress` event (`needs-input`, `new-session`, ...).
    kind: String,
    /// When set, the rule fires on the first stored event this picks and `kind` and `nth` are not used (`instruct_after_event`).
    matcher: Option<EventMatcher>,
    /// The rule fires on the nth event of that kind (1 = the first).
    nth: usize,
    /// The progress kind whose first event's id fills an empty request id (the question the answer is for).
    request_from: String,
    controls: Vec<PendingControl>,
    /// How many readable responses carry the controls (2 = the same answer again, as a client does when it saw no acknowledgement).
    times: u32,
    sent: u32,
}

/// What a test answers a question with (`FakeAws::instruct_on`).
#[derive(Clone, Debug)]
pub struct Answer {
    pub action: InstructAction,
    pub text: Option<String>,
    pub model: Option<String>,
}

impl Answer {
    pub fn new(action: InstructAction) -> Self {
        Self {
            action,
            text: None,
            model: None,
        }
    }

    pub fn text(mut self, t: &str) -> Self {
        self.text = Some(t.to_string());
        self
    }

    pub fn model(mut self, m: &str) -> Self {
        self.model = Some(m.to_string());
        self
    }

    /// The control the plane delivers; the empty request id is filled with the id of the event that triggered the rule.
    pub fn control(&self) -> PendingControl {
        PendingControl::Instruct {
            request_id: String::new(),
            action: self.action,
            text: self.text.clone(),
            model: self.model.clone(),
            origin: InstructOrigin::User,
        }
    }
}

/// The `kind` of a `turn.progress` event's inline payload.
fn progress_kind(e: &Envelope) -> Option<String> {
    match (&e.event_type, &e.payload) {
        (EventType::TurnProgress, Payload::Inline { value }) => {
            value.get("kind").and_then(|k| k.as_str()).map(String::from)
        }
        _ => None,
    }
}

fn with_request_id(c: &PendingControl, id: &str) -> PendingControl {
    match c {
        PendingControl::Instruct {
            request_id,
            action,
            text,
            model,
            origin,
        } if request_id.is_empty() => PendingControl::Instruct {
            request_id: id.to_string(),
            action: *action,
            text: text.clone(),
            model: model.clone(),
            origin: *origin,
        },
        other => other.clone(),
    }
}

pub struct FakeAws {
    pub base: String,
    pub state: Arc<AwsState>,
}

type Shared = Arc<AwsState>;

/// Stores `body` as the newest version of `full` (`bucket/key`) and returns its version id.
fn record_version(s: &AwsState, full: &str, body: &[u8]) -> String {
    // The id names the key's last segment and the version's place in that key's history, so it is the same on every run of the same flow
    // (the golden streams carry it) whatever order the other keys are written in.
    let mut versions = s.versions.lock().unwrap();
    let history = versions.entry(full.to_string()).or_default();
    let name = full.rsplit('/').next().unwrap_or_default();
    let id = format!("fake-{}-{name}", history.len() + 1);
    history.push((id.clone(), body.to_vec()));
    id
}

async fn s3_put(
    State(s): State<Shared>,
    AxPath((bucket, key)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let full = format!("{bucket}/{key}");
    *s.put_attempts
        .lock()
        .unwrap()
        .entry(full.clone())
        .or_insert(0) += 1;
    {
        let mut fails = s.put_failures.lock().unwrap();
        if let Some(f) = fails
            .iter_mut()
            .find(|(sub, left)| full.contains(sub.as_str()) && *left > 0)
        {
            if f.1 != u32::MAX {
                f.1 -= 1;
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "<Error><Code>InternalError</Code><Message>fake S3 failure</Message></Error>",
            )
                .into_response();
        }
    }
    // S3 refuses a request that carries an x-amz-* header its signature does not cover; the fake does the same, so a header the
    // product forgot to sign fails here as it would against the real service.
    if let Some(refusal) = unsigned_amz_header(&headers) {
        return refusal;
    }
    let conditional = headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("*");
    // S3 validates a supplied checksum against the bytes it received and refuses a mismatch.
    let checksum = headers
        .get("x-amz-checksum-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(sent) = &checksum
        && *sent != sha256_b64(&body)
    {
        return (
            StatusCode::BAD_REQUEST,
            "<Error><Code>BadDigest</Code><Message>The SHA256 you specified did not match the calculated checksum.</Message></Error>",
        )
            .into_response();
    }
    // A scheduled failure (`FakeAws::fail_puts_after_store`, `fail_overwrites`) of this PUT: the first one that still has a count left.
    let fault = s.put_faults.lock().unwrap().iter().position(|f| {
        f.left > 0 && full.contains(f.substring.as_str()) && (!f.only_unconditional || !conditional)
    });
    let (how, after_store, overwrite) = match fault {
        Some(i) => {
            let faults = s.put_faults.lock().unwrap();
            (
                Some(faults[i].how),
                faults[i].after_store,
                faults[i].overwrite_after_store.clone(),
            )
        }
        None => (None, false, None),
    };
    if let (Some(i), Some(how), false) = (fault, how, after_store) {
        s.put_faults.lock().unwrap()[i].left -= 1;
        return failure_response(how).await;
    }
    let stored_version: String;
    {
        let mut objects = s.objects.lock().unwrap();
        // S3's conditional write: `If-None-Match: *` fails with 412 when the key holds an object.
        if conditional && objects.contains_key(&full) {
            return (
                StatusCode::PRECONDITION_FAILED,
                "<Error><Code>PreconditionFailed</Code><Message>At least one of the pre-conditions you specified did not hold</Message></Error>",
            )
                .into_response();
        }
        objects.insert(full.clone(), body.to_vec());
        stored_version = record_version(&s, &full, &body);
        let mut sums = s.checksums.lock().unwrap();
        match checksum {
            Some(c) => sums.insert(full.clone(), c),
            None => sums.remove(&full),
        };
    }
    if let (Some(i), Some(how), true) = (fault, how, after_store) {
        // The object is committed; only the answer is lost. A foreign writer may get in before the retry.
        s.put_faults.lock().unwrap()[i].left -= 1;
        if let Some(other) = overwrite {
            record_version(&s, &full, &other);
            s.objects.lock().unwrap().insert(full.clone(), other);
            s.checksums.lock().unwrap().remove(&full);
        }
        return failure_response(how).await;
    }
    let mut resp = StatusCode::OK.into_response();
    if !*s.no_version_ids.lock().unwrap() {
        resp.headers_mut()
            .insert("x-amz-version-id", stored_version.parse().unwrap());
    }
    resp
}

/// The error answer of a scheduled failure.
async fn failure_response(how: PutFailure) -> axum::response::Response {
    match how {
        PutFailure::Status500 => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "<Error><Code>InternalError</Code><Message>fake S3 failure</Message></Error>",
        )
            .into_response(),
        PutFailure::ResetBody => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::body::Body::new(ResetBody),
        )
            .into_response(),
        PutFailure::Hang => {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `Some(refusal)` when the request names an `x-amz-*` header that its `Authorization` header does not sign. A request without an
/// `Authorization` header is not checked (a test talking to the fake directly).
fn unsigned_amz_header(headers: &axum::http::HeaderMap) -> Option<axum::response::Response> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let signed: Vec<&str> = auth
        .split("SignedHeaders=")
        .nth(1)?
        .split(',')
        .next()?
        .split(';')
        .collect();
    let unsigned = headers
        .keys()
        .map(|k| k.as_str())
        .find(|k| k.starts_with("x-amz-") && !signed.contains(k))?;
    Some(
        (
            StatusCode::FORBIDDEN,
            format!(
                "<Error><Code>AccessDenied</Code><Message>There were headers present in the request which were not signed: {unsigned}</Message></Error>"
            ),
        )
            .into_response(),
    )
}

/// HEAD (`HeadObject`): 404 for a missing key, the forced status when `FakeAws::heads_answer` set one, else 200 with
/// `x-amz-checksum-sha256` when the request asked for checksum mode and the object was stored with a SHA-256 checksum.
async fn s3_head(
    State(s): State<Shared>,
    AxPath((bucket, key)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let full = format!("{bucket}/{key}");
    *s.head_attempts
        .lock()
        .unwrap()
        .entry(full.clone())
        .or_insert(0) += 1;
    if let Some(refusal) = unsigned_amz_header(&headers) {
        return refusal;
    }
    let forced = *s.head_status.lock().unwrap();
    if let Some(code) = forced {
        return StatusCode::from_u16(code).unwrap().into_response();
    }
    if !s.objects.lock().unwrap().contains_key(&full) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut resp = StatusCode::OK.into_response();
    let wants_checksum = headers
        .get("x-amz-checksum-mode")
        .and_then(|v| v.to_str().ok())
        == Some("ENABLED");
    if wants_checksum && let Some(sum) = s.checksums.lock().unwrap().get(&full) {
        resp.headers_mut()
            .insert("x-amz-checksum-sha256", sum.parse().unwrap());
    }
    if !*s.no_version_ids.lock().unwrap()
        && let Some((id, _)) = s
            .versions
            .lock()
            .unwrap()
            .get(&full)
            .and_then(|v| v.last().cloned())
    {
        resp.headers_mut()
            .insert("x-amz-version-id", id.parse().unwrap());
    }
    resp
}

/// `GET /{bucket}?list-type=2&prefix=...&start-after=...` (ListObjectsV2, one page).
async fn s3_list(
    State(s): State<Shared>,
    AxPath(bucket): AxPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> axum::response::Response {
    let q: std::collections::BTreeMap<String, String> = raw
        .unwrap_or_default()
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_string(), v.replace("%2F", "/")))
        .collect();
    let prefix = format!("{bucket}/{}", q.get("prefix").cloned().unwrap_or_default());
    let after = q
        .get("start-after")
        .map(|a| format!("{bucket}/{a}"))
        .unwrap_or_default();
    let objects = s.objects.lock().unwrap();
    let mut xml =
        String::from("<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>");
    for k in objects
        .keys()
        .filter(|k| k.starts_with(&prefix) && **k > after)
    {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key></Contents>",
            &k[bucket.len() + 1..]
        ));
    }
    xml.push_str("</ListBucketResult>");
    (StatusCode::OK, xml).into_response()
}

async fn s3_get(
    State(s): State<Shared>,
    AxPath((bucket, key)): AxPath<(String, String)>,
) -> axum::response::Response {
    let full = format!("{bucket}/{key}");
    let pause = s
        .get_delays
        .lock()
        .unwrap()
        .iter()
        .find(|(sub, _)| full.contains(sub.as_str()))
        .map(|(_, d)| *d);
    if let Some(d) = pause {
        tokio::time::sleep(d).await;
    }
    match s.objects.lock().unwrap().get(&full) {
        Some(b) => (StatusCode::OK, b.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchKey</Code></Error>",
        )
            .into_response(),
    }
}

async fn api_events(
    State(s): State<Shared>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    note_completion_key(&s, "/internal/events", &headers);
    let batch: EventsBatchRequest = serde_json::from_slice(&body).unwrap();
    s.batches.lock().unwrap().push((
        u32::try_from(batch.events.len()).unwrap(),
        batch.take_controls,
    ));
    {
        // The real plane stores an event once per id (a resent batch is a no-op).
        let mut stored = s.events.lock().unwrap();
        for e in &batch.events {
            if !stored.iter().any(|x| x.event_id == e.event_id) {
                stored.push(e.clone());
            }
        }
    }
    let mut pending = Vec::new();
    let want = *s.cancel_on.lock().unwrap();
    if let Some(t) = want
        && batch.take_controls != Some(false)
        && s.events.lock().unwrap().iter().any(|e| e.event_type == t)
    {
        let mut sent = s.cancel_sent.lock().unwrap();
        if !*sent {
            *sent = true;
            pending.push(PendingControl::Cancel);
        }
    }
    // The rules of `instruct_on`: an answer is handed out on the response after the matching event was posted.
    let mut armed: Vec<(usize, Vec<PendingControl>)> = Vec::new();
    if batch.take_controls != Some(false) {
        let stored = s.events.lock().unwrap().clone();
        let mut rules = s.control_rules.lock().unwrap();
        for (i, rule) in rules.iter().enumerate() {
            if rule.sent >= rule.times {
                continue;
            }
            let hit = match &rule.matcher {
                Some(m) => stored.iter().find(|e| m(e)),
                None => stored
                    .iter()
                    .filter(|e| progress_kind(e).as_deref() == Some(rule.kind.as_str()))
                    .nth(rule.nth - 1),
            };
            // The question the answer names: the event that triggered the rule when the trigger is the question's own kind (the nth
            // question of the run), else the first event of the question's kind.
            let asked = if rule.request_from == rule.kind {
                hit
            } else {
                stored
                    .iter()
                    .find(|e| progress_kind(e).as_deref() == Some(rule.request_from.as_str()))
            };
            if let (Some(_), Some(question)) = (hit, asked) {
                armed.push((
                    i,
                    rule.controls
                        .iter()
                        .map(|c| with_request_id(c, &question.event_id))
                        .collect(),
                ));
            }
        }
        if !armed.is_empty() {
            let mut swallow = s.swallow.lock().unwrap();
            if *swallow > 0 {
                // The lost response: the plane considers the control undelivered (as control-api does after twelve seconds) and
                // sends it again on a later response; the body agentd gets now is not JSON.
                *swallow -= 1;
                *s.swallowed.lock().unwrap() += 1;
                return (StatusCode::OK, "<<garbage: the response was lost>>").into_response();
            }
            for (i, controls) in &armed {
                rules[*i].sent += 1;
                s.sent_controls.lock().unwrap().extend(controls.clone());
                pending.extend(controls.clone());
            }
        }
    }
    let mut body = serde_json::to_value(EventsBatchResponse {
        accepted: u32::try_from(batch.events.len()).unwrap(),
        pending,
    })
    .unwrap();
    if !armed.is_empty() {
        let stored = s.events.lock().unwrap().clone();
        let mut raws = s.raw_controls.lock().unwrap();
        let mut front = Vec::new();
        for (kind, item, sent) in raws.iter_mut().filter(|r| !r.2) {
            if stored
                .iter()
                .any(|e| progress_kind(e).as_deref() == Some(kind.as_str()))
            {
                *sent = true;
                front.push(item.clone());
            }
        }
        if !front.is_empty() {
            let list = body["pending"].as_array_mut().unwrap();
            front.append(list);
            *list = front;
        }
    }
    axum::Json(body).into_response()
}

async fn api_heartbeat(State(s): State<Shared>, body: Bytes) -> axum::response::Response {
    s.heartbeats
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap());
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// Records the completion key header of a guarded route (the real plane refuses a request without the run's key).
fn note_completion_key(s: &Shared, path: &str, headers: &axum::http::HeaderMap) {
    let key = headers
        .get(harness_protocol::store::COMPLETION_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    s.completion_keys
        .lock()
        .unwrap()
        .push((path.to_string(), key));
}

async fn api_complete(
    State(s): State<Shared>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    note_completion_key(&s, "/internal/turn-complete", &headers);
    *s.complete_attempts.lock().unwrap() += 1;
    {
        let mut left = s.complete_failures.lock().unwrap();
        if *left > 0 {
            if *left != u32::MAX {
                *left -= 1;
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "fake internal API failure",
            )
                .into_response();
        }
    }
    s.complete
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap());
    axum::Json(serde_json::json!({})).into_response()
}

async fn api_creds(State(s): State<Shared>) -> axum::response::Response {
    *s.refreshes.lock().unwrap() += 1;
    let answer = s.refresh.lock().unwrap().clone();
    axum::Json(answer.unwrap_or_else(|| CredsResponse {
        credentials: creds(&format_rfc3339(now_unix() + 3600)),
        next_nonce: "next".into(),
    }))
    .into_response()
}

impl FakeAws {
    pub async fn start() -> Self {
        let state: Shared = Arc::default();
        let app = Router::new()
            .route("/internal/events", post(api_events))
            .route("/internal/heartbeat", post(api_heartbeat))
            .route("/internal/turn-complete", post(api_complete))
            .route("/internal/creds", post(api_creds))
            .route("/{bucket}/{*key}", get(s3_get).put(s3_put).head(s3_head))
            .route("/{bucket}", get(s3_list))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            state,
        }
    }

    /// Puts an object as a foreign writer would: it carries no SHA-256 checksum.
    pub fn put(&self, key: &str, bytes: Vec<u8>) {
        let full = format!("{BUCKET}/{key}");
        self.state.checksums.lock().unwrap().remove(&full);
        record_version(&self.state, &full, &bytes);
        self.state.objects.lock().unwrap().insert(full, bytes);
    }

    /// Every version `key` has had, oldest first, as (version id, body).
    pub fn versions_of(&self, key: &str) -> Vec<(String, Vec<u8>)> {
        self.state
            .versions
            .lock()
            .unwrap()
            .get(&format!("{BUCKET}/{key}"))
            .cloned()
            .unwrap_or_default()
    }

    /// From now on the answers to PUT and HEAD carry no version id, as those of a bucket that keeps no versions.
    pub fn without_version_ids(&self) {
        *self.state.no_version_ids.lock().unwrap() = true;
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.state
            .objects
            .lock()
            .unwrap()
            .get(&format!("{BUCKET}/{key}"))
            .cloned()
    }

    pub fn keys(&self) -> Vec<String> {
        self.state.objects.lock().unwrap().keys().cloned().collect()
    }

    /// The next `times` PUTs whose bucket/key contains `substring` answer 500 (`u32::MAX`: every one).
    pub fn fail_puts(&self, substring: &str, times: u32) {
        self.state
            .put_failures
            .lock()
            .unwrap()
            .push((substring.to_string(), times));
    }

    /// The next `times` PUTs whose bucket/key contains `substring` STORE their body and then end as `how`: the object is committed and
    /// only the answer is lost (a dropped connection, a 5xx after the commit, a timeout).
    pub fn fail_puts_after_store(&self, substring: &str, times: u32, how: PutFailure) {
        self.state.put_faults.lock().unwrap().push(PutFault {
            substring: substring.to_string(),
            left: times,
            how,
            after_store: true,
            only_unconditional: false,
            overwrite_after_store: None,
        });
    }

    /// Like [`fail_puts_after_store`](Self::fail_puts_after_store) for one PUT, and a foreign writer replaces the committed object with
    /// `other` (stored without a checksum) before the client retries.
    pub fn fail_put_after_store_then_overwrite(
        &self,
        substring: &str,
        how: PutFailure,
        other: &[u8],
    ) {
        self.state.put_faults.lock().unwrap().push(PutFault {
            substring: substring.to_string(),
            left: 1,
            how,
            after_store: true,
            only_unconditional: false,
            overwrite_after_store: Some(other.to_vec()),
        });
    }

    /// The next `times` PUTs without `If-None-Match` (a replacement of an object, not a create-only write) whose bucket/key contains
    /// `substring` answer 500 and store nothing.
    pub fn fail_overwrites(&self, substring: &str, times: u32) {
        self.state.put_faults.lock().unwrap().push(PutFault {
            substring: substring.to_string(),
            left: times,
            how: PutFailure::Status500,
            after_store: false,
            only_unconditional: true,
            overwrite_after_store: None,
        });
    }

    /// Every HEAD answers `status` with no body from now on (403: the role may not list the bucket, so S3 hides a missing key).
    pub fn heads_answer(&self, status: u16) {
        *self.state.head_status.lock().unwrap() = Some(status);
    }

    /// How many HEADs arrived for the key.
    pub fn head_attempts(&self, key: &str) -> u32 {
        *self
            .state
            .head_attempts
            .lock()
            .unwrap()
            .get(&format!("{BUCKET}/{key}"))
            .unwrap_or(&0)
    }

    /// Puts an object that carries this checksum value, as a foreign writer's multipart upload would (a composite value `<digest>-<parts>`).
    pub fn put_with_checksum(&self, key: &str, bytes: Vec<u8>, checksum: &str) {
        let full = format!("{BUCKET}/{key}");
        self.state
            .objects
            .lock()
            .unwrap()
            .insert(full.clone(), bytes);
        self.state
            .checksums
            .lock()
            .unwrap()
            .insert(full, checksum.to_string());
    }

    /// The SHA-256 checksum the object was stored with, if the writer sent one.
    pub fn checksum_of(&self, key: &str) -> Option<String> {
        self.state
            .checksums
            .lock()
            .unwrap()
            .get(&format!("{BUCKET}/{key}"))
            .cloned()
    }

    pub fn put_attempts(&self, key: &str) -> u32 {
        *self
            .state
            .put_attempts
            .lock()
            .unwrap()
            .get(&format!("{BUCKET}/{key}"))
            .unwrap_or(&0)
    }

    /// A GET whose bucket/key contains `substring` is answered after `pause` (a slow restore of the repository).
    pub fn delay_gets(&self, substring: &str, pause: Duration) {
        self.state
            .get_delays
            .lock()
            .unwrap()
            .push((substring.to_string(), pause));
    }

    /// The next `times` turn-complete posts answer 500 (`u32::MAX`: every one).
    pub fn fail_complete(&self, times: u32) {
        *self.state.complete_failures.lock().unwrap() = times;
    }

    /// The completion key header of every `/internal/events` and `/internal/turn-complete` post, in order: `(path, key)`.
    pub fn completion_keys(&self) -> Vec<(String, Option<String>)> {
        self.state.completion_keys.lock().unwrap().clone()
    }

    /// How many turn-complete posts arrived, accepted or not.
    pub fn complete_attempts(&self) -> u32 {
        *self.state.complete_attempts.lock().unwrap()
    }

    /// What every `/internal/creds` post is answered with from now on: the credentials a refresh mints and the nonce the next refresh
    /// must carry (the real route rotates both). Without it the fake answers the canary credentials for another hour and the nonce `next`.
    pub fn refresh_answers(&self, credentials: ProcessCredentials, next_nonce: &str) {
        *self.state.refresh.lock().unwrap() = Some(CredsResponse {
            credentials,
            next_nonce: next_nonce.to_string(),
        });
    }

    /// How many `/internal/creds` posts arrived: the refreshes of the session credentials agentd made.
    pub fn refreshes(&self) -> u32 {
        *self.state.refreshes.lock().unwrap()
    }

    /// Once an event of this type has been posted, the next events response carries a Cancel control.
    pub fn cancel_on(&self, t: EventType) {
        *self.state.cancel_on.lock().unwrap() = Some(t);
    }

    /// Once the first event with this progress kind (`needs-input`, ...) has been posted, the next events response carries `answer`
    /// as a user's `Instruct` for that event's request id.
    pub fn instruct_on(&self, kind: &str, answer: Answer) {
        self.instruct_rule(kind, 1, kind, vec![answer.control()], 1);
    }

    /// As [`FakeAws::instruct_on`], but the control is handed out after the first event of kind `trigger` (an `input-ignored` that
    /// the first answer earned, say) and names the question of kind `question` (`needs-input`).
    pub fn instruct_after(&self, trigger: &str, question: &str, answer: Answer) {
        self.instruct_rule(trigger, 1, question, vec![answer.control()], 1);
    }

    /// The general form: `controls` go out in one response once the nth event of kind `trigger` has been posted, their empty request
    /// ids naming the first event of kind `question`, in `times` readable responses (2 is a client that sent its answer again).
    pub fn instruct_rule(
        &self,
        trigger: &str,
        nth: usize,
        question: &str,
        controls: Vec<PendingControl>,
        times: u32,
    ) {
        assert!(nth >= 1, "nth counts from 1");
        self.state.control_rules.lock().unwrap().push(ControlRule {
            kind: trigger.to_string(),
            matcher: None,
            nth,
            request_from: question.to_string(),
            controls,
            times,
            sent: 0,
        });
    }

    /// As [`FakeAws::instruct_on`], but the answer to the `question` (a progress kind, `needs-input`) is handed out once the plane has stored an
    /// event `trigger` picks, whatever that event is. For an order a test must prove without a pause: the trigger is an event agentd posts
    /// only after it has read a line of the engine's output, and the pipe is read in order.
    pub fn instruct_after_event(
        &self,
        trigger: impl Fn(&Envelope) -> bool + Send + Sync + 'static,
        question: &str,
        answer: Answer,
    ) {
        self.state.control_rules.lock().unwrap().push(ControlRule {
            kind: String::new(),
            matcher: Some(Arc::new(trigger)),
            nth: 1,
            request_from: question.to_string(),
            controls: vec![answer.control()],
            times: 1,
            sent: 0,
        });
    }

    /// One batch with several controls on the first event of the kind (`instruct_batch("needs-input", vec![Cancel, answer])`).
    pub fn instruct_batch(&self, kind: &str, controls: Vec<PendingControl>) {
        self.instruct_rule(kind, 1, kind, controls, 1);
    }

    /// Puts a raw item that agentd cannot parse (`{"kind": "future-thing"}`) in front of the controls of the response that follows an
    /// event of this progress kind. It goes out once.
    pub fn add_unreadable_control_on(&self, kind: &str, item: serde_json::Value) {
        self.state
            .raw_controls
            .lock()
            .unwrap()
            .push((kind.to_string(), item, false));
    }

    /// The next events response that would carry a control is garbage instead, and the control is sent again on a later response.
    pub fn swallow_response(&self) {
        *self.state.swallow.lock().unwrap() += 1;
    }

    /// How many control-bearing responses `swallow_response` ate.
    pub fn swallowed(&self) -> u32 {
        *self.state.swallowed.lock().unwrap()
    }

    /// The controls of the rules that agentd could read, in the order they were handed out (the request ids filled in).
    pub fn sent_controls(&self) -> Vec<PendingControl> {
        self.state.sent_controls.lock().unwrap().clone()
    }

    /// The `turn.progress` events of one kind, in order, with their event ids: `(eventId, payload)`.
    pub fn progress(&self, kind: &str) -> Vec<(String, serde_json::Value)> {
        self.state
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| progress_kind(e).as_deref() == Some(kind))
            .map(|e| match &e.payload {
                Payload::Inline { value } => (e.event_id.clone(), value.clone()),
                other => panic!("not inline: {other:?}"),
            })
            .collect()
    }

    /// The kinds of every `turn.progress` event, in order (the order a test asserts a question's life by).
    pub fn progress_kinds(&self) -> Vec<String> {
        self.state
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(progress_kind)
            .collect()
    }

    pub fn events(&self) -> Vec<(EventType, serde_json::Value)> {
        self.state
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match &e.payload {
                Payload::Inline { value } => (e.event_type, value.clone()),
                other => panic!("not inline: {other:?}"),
            })
            .collect()
    }

    pub fn envelopes(&self) -> Vec<Envelope> {
        self.state.events.lock().unwrap().clone()
    }

    pub fn heartbeats(&self) -> Vec<HeartbeatRequest> {
        self.state.heartbeats.lock().unwrap().clone()
    }

    pub fn complete(&self) -> Vec<TurnCompleteRequest> {
        self.state.complete.lock().unwrap().clone()
    }

    pub fn batches(&self) -> Vec<(u32, Option<bool>)> {
        self.state.batches.lock().unwrap().clone()
    }
}

// ---- the run -------------------------------------------------------------------------------------------------------------------

/// A change to the agentd config a test makes before the server starts.
pub type CfgEdit = Box<dyn FnOnce(&mut Config)>;

pub struct Opts {
    /// The engine profile name in the request: `cline` (a test profile that runs the fake engine under the real profile's
    /// environment and hidden paths) or `fake-acp`.
    pub engine: &'static str,
    pub scenario: &'static str,
    /// Environment the engine wrapper exports before it runs the fake engine.
    pub engine_env: Vec<(&'static str, String)>,
    pub rules: Vec<RuleFile>,
    pub prompt: String,
    pub timeout_s: u32,
    pub provider_silence_s: Option<u32>,
    pub snapshot_every_s: Option<u32>,
    pub nudge: Option<ContinueSettings>,
    /// `request.json` in the bucket is not the file the invocation hashed.
    pub tamper_request: bool,
    /// `repo.bundle` is missing from the bucket.
    pub no_bundle: bool,
    pub edit_cfg: Option<CfgEdit>,
    pub packages_token: bool,
    /// The failover routes of the request (`DirectInvocationRequest.failover`); empty means none.
    pub failover: Vec<DirectModelSpec>,
    /// `DirectInvocationRequest.input_wait_s` (protocol 1.37.0): how long a stopped run waits for the user's answer; `None`: no wait.
    pub input_wait_s: Option<u32>,
    /// `StartDirectRunRequest.report_tests` (protocol 1.38.0): `Some(false)` switches the clean-checkout test report off. `None` leaves
    /// the member out of `request.json`, so the request of every other test is byte for byte what it was.
    pub report_tests: Option<bool>,
    /// `StartDirectRunRequest.report_step_s` (protocol 1.41.0): the seconds one command of the report may run. `None` leaves the member out of
    /// `request.json`, so the request of every other test is byte for byte what it was.
    pub report_step_s: Option<u32>,
    /// The test `cline` profile keeps the `[mcp]` table of the real profile (protocol 1.40.0, the tool layer) and its fake engine reads the
    /// registration file (`HAMH_FAKE_MCP_SETTINGS`). `false` strips the table, so every other test runs a profile that has none (an engine the
    /// layer has not been wired to) and the tool hook, which is in the product's list, has nothing to do: the 13 golden streams and the
    /// whole-run tests are byte for byte what they were.
    pub mcp: bool,
    /// Environment of the real `agentd serve` process (`Direct::start_process`), added to the harness's own: `AGENTD_TOOLS_MODE` and the like.
    pub agentd_env: Vec<(&'static str, String)>,
    /// `StartDirectRunRequest.review` as the JSON of the request member. `None` leaves the member out of
    /// `request.json`, so the request of every other test is byte for byte what it was.
    pub review: Option<serde_json::Value>,
    /// `StartDirectRunRequest.build` as the JSON of the request member. `None` leaves the member out of `request.json`.
    pub build: Option<serde_json::Value>,
    /// `StartDirectRunRequest.goal` as the JSON of the request member. `None` leaves the member out of `request.json`.
    pub goal: Option<serde_json::Value>,
    /// Files committed to the repository before it is bundled (path, text); empty leaves the repository as `python_calc` made it.
    pub repo_files: Vec<(&'static str, &'static str)>,
    /// Files moved in the repository before it is bundled (`git mv`, from and to): `tests/test_calc.py` to a name that is no test file gives a tree
    /// with no test layout the report knows, now that the report runs a plain pytest layout.
    pub repo_moved: Vec<(&'static str, &'static str)>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            engine: "cline",
            scenario: "writes_files",
            engine_env: Vec::new(),
            rules: vec![rule(
                "00-my-rules.md",
                "Always write the failing test first.\n",
            )],
            prompt: PROMPT.to_string(),
            timeout_s: 120,
            provider_silence_s: None,
            snapshot_every_s: None,
            nudge: None,
            tamper_request: false,
            no_bundle: false,
            edit_cfg: None,
            packages_token: false,
            failover: Vec::new(),
            input_wait_s: None,
            report_tests: None,
            report_step_s: None,
            mcp: false,
            agentd_env: Vec::new(),
            review: None,
            build: None,
            goal: None,
            repo_files: Vec::new(),
            repo_moved: Vec::new(),
        }
    }
}

/// The real `agentd serve` binary, killed when the test ends.
pub struct Process {
    child: std::process::Child,
    pub log_file: PathBuf,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Direct {
    pub process: Option<Process>,
    pub aws: FakeAws,
    pub base: String,
    pub log: Arc<MemoryLog>,
    pub dir: PathBuf,
    pub app: Arc<App>,
    pub req: DirectInvocationRequest,
    pub request_json: Vec<u8>,
}

fn short_runtime_dir() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("dd{}-{n}-{}", std::process::id(), unique_suffix()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The real `cline` profile with its launch swapped for the fake engine: same environment, hidden paths, rows and model pin. The `[mcp]` table
/// of the real profile is dropped unless `mcp` is set (see [`Opts::mcp`]); with it the fake engine is told where its registration file is.
pub fn test_cline_profile(mcp: bool) -> String {
    let real = std::fs::read_to_string(repo_root().join("engine/profiles/cline.toml")).unwrap();
    let transcripts = repo_root().join("fixtures/engines/fake-acp/transcripts");
    let mut out = String::new();
    let mut in_mcp = false;
    for line in real.lines() {
        if line.starts_with('[') {
            in_mcp = line == "[mcp]";
        }
        if in_mcp && !mcp {
            continue;
        }
        if line.starts_with("argv = [\"/opt/harness/bin/cline\"") {
            out.push_str(&format!(
                "argv = [\"fake-acp-engine\", \"--transcripts\", \"{}\", \"--workspace\", \"{{workspace}}\"]\n",
                transcripts.display()
            ));
        } else if line.starts_with("path = \"/opt/harness/bin/cline\"") {
            out.push_str("path = \"fake-acp-engine\"\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
        if line == "[env]" {
            out.push_str("FAKE_ACP_LOG = \"{dataDir}/fake-acp.log\"\n");
            if mcp {
                out.push_str(
                    "HAMH_FAKE_MCP_SETTINGS = \"{dataDir}/settings/cline_mcp_settings.json\"\n",
                );
            }
        }
    }
    out
}

/// What the engine wrapper does before the fake engine runs, switched on by an `engine_env` variable of the test. Each step leaves a mark
/// in HOME so a test can assert that its trigger was present.
const PRELUDES: &str = concat!(
    // WRAP_FILTER_SETTING: a clean filter in .git/config and the attribute that selects it (the engine-written repository
    // configuration of the workspace.rs GitState comment); the filter script leaves `filter-used` if git ever runs it.
    "if [ -n \"$WRAP_FILTER_SETTING\" ]; then { echo '#!/bin/sh'; echo \"echo used > $HOME/filter-used\"; echo cat; } > \"$HOME/clean-filter.sh\"; \
     chmod 755 \"$HOME/clean-filter.sh\"; git config filter.custom.clean \"$HOME/clean-filter.sh\"; \
     mkdir -p .git/info; echo '*.py filter=custom' >> .git/info/attributes; \
     git config --get filter.custom.clean > \"$HOME/filter-setting-seen\"; fi\n",
    // WRAP_GIT_REDIRECT: the repository's directory moves and a file named `.git` names it, so git reads the configuration of the moved
    // directory, where the clean filter is defined (b630-secrets-2). The filter script leaves `filter-used` if git ever runs it.
    "if [ -n \"$WRAP_GIT_REDIRECT\" ]; then { echo '#!/bin/sh'; echo \"echo used > $HOME/filter-used\"; echo cat; } > \"$HOME/clean-filter.sh\"; \
     chmod 755 \"$HOME/clean-filter.sh\"; mv .git .gitreal; printf 'gitdir: %s\\n' \"$PWD/.gitreal\" > .git; \
     git config filter.custom.clean \"$HOME/clean-filter.sh\"; echo '*.py filter=custom' > .gitattributes; \
     git config --get filter.custom.clean > \"$HOME/filter-setting-seen\"; fi\n",
    // WRAP_BUILD_OUTPUTS: build outputs that are never the engine's work.
    "if [ -n \"$WRAP_BUILD_OUTPUTS\" ]; then mkdir -p src/__pycache__; echo x > src/__pycache__/calc.cpython-312.pyc; \
     echo x > src/stray.pyc; echo placed > \"$HOME/build-outputs-seen\"; fi\n",
    // WRAP_RECORD_NOFILE: the soft and the hard open-files limit the engine's process starts with, one per line, as the shell reports
    // them.
    "if [ -n \"$WRAP_RECORD_NOFILE\" ]; then { ulimit -S -n; ulimit -H -n; } > \"$HOME/nofile-seen\"; fi\n",
    // WRAP_COMMIT_MESSAGE: the engine commits one new file with this message before it works, as Cline commits its own work.
    "if [ -n \"$WRAP_COMMIT_MESSAGE\" ]; then echo note > NOTES.md; git add NOTES.md; \
     git -c user.name=t -c user.email=t@example.com commit -q -m \"$WRAP_COMMIT_MESSAGE\"; fi\n",
    // WRAP_REDIRECT_TURNS: a link where agentd's end sequence once wrote the bundle (the worktree root belongs to the engine).
    "if [ -n \"$WRAP_REDIRECT_TURNS\" ]; then mkdir -p \"$HOME/redirect\"; ln -s \"$HOME/redirect\" ../turns; fi\n",
    // WRAP_CONFIG_WRITES: the engine user tries to change its configuration directory and writes each exit code. The one-line writer is a
    // script file, so that no process has a redirect in its arguments.
    "if [ -n \"$WRAP_CONFIG_WRITES\" ]; then { try() { \"$@\" > /dev/null 2>&1; echo \"rc=$? $*\"; }; \
     { echo '#!/bin/sh'; echo 'echo x > \"$1\"'; } > \"$HOME/write-line.sh\"; chmod 755 \"$HOME/write-line.sh\"; \
     try mv \"$cfg/rules\" \"$cfg/rules.old\"; try rmdir \"$cfg/locked\"; try mkdir \"$cfg/rules/new-dir\"; \
     try \"$HOME/write-line.sh\" \"$cfg/rules/00-my-rules.md\"; try mkdir \"$cfg/anything\"; \
     try \"$HOME/write-line.sh\" \"$HOME/own-file\"; } > \"$HOME/config-writes.txt\"; fi\n",
);

fn wrapper(dir: &Path, opts: &Opts) -> PathBuf {
    let real = fake_engine_dir();
    let bin = dir.join("engine-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut exports = format!("export FAKE_ACP_SCENARIO='{}'\n", opts.scenario);
    for (k, v) in &opts.engine_env {
        exports.push_str(&format!("export {k}='{v}'\n"));
    }
    let body = format!(
        "#!/bin/sh\n{exports}case \"$1\" in config) exec {real}/fake-acp-engine \"$@\" ;; esac\n\
         if [ -n \"$WRAP_STDERR_FILE\" ]; then cat \"$WRAP_STDERR_FILE\" >&2; exit 2; fi\n\
         if [ -n \"$WRAP_STDERR_HUGE\" ]; then i=0; while [ $i -lt 30000 ]; do echo \"noise line $i padding padding padding padding\" >&2; i=$((i+1)); done; \
         echo 'FINAL-WORDS: boom' >&2; exit 1; fi\n\
         if [ -n \"$WRAP_EXIT\" ]; then echo \"engine says: it cannot start\" >&2; exit \"$WRAP_EXIT\"; fi\n\
         cfg=$(dirname \"$(dirname \"$AWS_CONFIG_FILE\")\")\n\
         {{ pwd; echo '--- argv'; printf '%s\n' \"$@\"; echo '--- env'; env | sort; echo '--- rules'; ls -ln \"$cfg/rules\" 2>&1; echo '--- rules dir'; ls -ldn \"$cfg/rules\" 2>&1; \
         echo '--- settings'; cat \"$CLINE_GLOBAL_SETTINGS_PATH\" 2>&1; echo '--- cline dir'; ls -a .cline 2>&1; }} > \"$HOME/engine-probe.txt\"\n\
         {preludes}\
         if [ -n \"$CLINE_MODEL\" ] && [ -z \"$FAKE_ACP_MODEL\" ]; then export FAKE_ACP_MODEL=\"$CLINE_MODEL\"; fi\n\
         exec {real}/fake-acp-engine \"$@\"\n",
        real = real.display(),
        preludes = PRELUDES
    );
    let p = bin.join("fake-acp-engine");
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The environment `agentd serve` reads, from a test config (`agentd::config`).
fn process_env(cfg: &Config) -> Vec<(String, String)> {
    let path = |p: &Path| p.display().to_string();
    let mut env = vec![
        ("AGENTD_PROFILES_DIR".to_string(), path(&cfg.profiles_dir)),
        ("AGENTD_FIXTURES_ROOT".to_string(), path(&cfg.fixtures_root)),
        (
            "AGENTD_TEMPLATES_ROOT".to_string(),
            path(&cfg.templates_root),
        ),
        (
            "AGENTD_WORKSPACE_ROOT".to_string(),
            path(&cfg.workspace_root),
        ),
        ("AGENTD_DATA_ROOT".to_string(), path(&cfg.data_root)),
        ("AGENTD_RUNTIME_DIR".to_string(), path(&cfg.runtime_dir)),
        (
            "AGENTD_ENGINE_PATH".to_string(),
            cfg.engine_path
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(":"),
        ),
        ("AGENTD_EXE".to_string(), path(&cfg.agentd_exe)),
        ("AGENTD_ENGINE_USER".to_string(), String::new()),
        ("AGENTD_ENGINE_SANDBOX".to_string(), "uid".to_string()),
        (
            "AGENTD_S3_ENDPOINT".to_string(),
            cfg.s3_endpoint.clone().unwrap_or_default(),
        ),
        (
            "AGENTD_INTERNAL_API_ENDPOINT".to_string(),
            cfg.internal_api_endpoint.clone().unwrap_or_default(),
        ),
        (
            "AGENTD_DIRECT_EVENTS_EVERY_MS".to_string(),
            cfg.direct.events_every.as_millis().to_string(),
        ),
        (
            "AGENTD_DIRECT_HEARTBEAT_EVERY_MS".to_string(),
            cfg.direct.heartbeat_every.as_millis().to_string(),
        ),
        (
            "AGENTD_DIRECT_UPLOAD_PAUSE_MS".to_string(),
            cfg.direct.upload_retry_pause.as_millis().to_string(),
        ),
    ];
    if !cfg.direct.disabled_tools.is_empty() {
        env.push((
            "AGENTD_DISABLED_TOOLS".to_string(),
            cfg.direct.disabled_tools.join(","),
        ));
    }
    // The stage's switch of the review round travels to a process as its variable.
    if !cfg.direct.review_round {
        env.push(("AGENTD_DIRECT_REVIEW_ROUND".to_string(), "off".to_string()));
    }
    env
}

impl Direct {
    pub async fn start(opts: Opts) -> Self {
        Self::start_mode(opts, false).await
    }

    /// As [`Direct::start`], but the server is the real `agentd serve` process (the entry point a microVM runs).
    pub async fn start_process(opts: Opts) -> Self {
        Self::start_mode(opts, true).await
    }

    async fn start_mode(opts: Opts, as_process: bool) -> Self {
        let dir = scratch("direct");
        let aws = FakeAws::start().await;
        let bin = wrapper(&dir, &opts);
        let profiles = dir.join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join("cline.toml"), test_cline_profile(opts.mcp)).unwrap();
        std::fs::copy(
            repo_root().join("engine/profiles/fake-acp.toml"),
            profiles.join("fake-acp.toml"),
        )
        .unwrap();

        // The artifacts control-api and repo-gateway leave in the bucket.
        let repo = python_calc(&dir);
        // The fake engine keeps its state in the worktree; a real engine keeps it elsewhere, and the nudge's tree test
        // must not count it.
        std::fs::write(repo.join(".gitignore"), ".fake-acp/\n").unwrap();
        git(&repo, &["add", ".gitignore"]);
        git(
            &repo,
            &["commit", "-q", "-m", "ignore the fake engine state"],
        );
        if !opts.repo_files.is_empty() {
            for (rel, text) in &opts.repo_files {
                let path = repo.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            git(&repo, &["add", "-A"]);
            git(
                &repo,
                &["commit", "-q", "-m", "files the test put in the repository"],
            );
        }
        if !opts.repo_moved.is_empty() {
            for (from, to) in &opts.repo_moved {
                git(&repo, &["mv", from, to]);
            }
            git(
                &repo,
                &[
                    "commit",
                    "-q",
                    "-m",
                    "files the test moved in the repository",
                ],
            );
        }
        let bundle = dir.join("repo.bundle");
        git(
            &repo,
            &["bundle", "create", "-q", bundle.to_str().unwrap(), "--all"],
        );
        if !opts.no_bundle {
            aws.put(&keys::repo_bundle_key(RUN), std::fs::read(&bundle).unwrap());
        }
        let mut request_value = serde_json::json!({
            "prompt": opts.prompt,
            "target": {"mode": "s3-only"},
            "model": MODEL,
            "rules": opts.rules,
            "nudge": opts.nudge,
        });
        if let Some(on) = opts.report_tests {
            request_value["reportTests"] = serde_json::json!(on);
        }
        if let Some(s) = opts.report_step_s {
            request_value["reportStepS"] = serde_json::json!(s);
        }
        if let Some(r) = &opts.review {
            request_value["review"] = r.clone();
        }
        if let Some(b) = &opts.build {
            request_value["build"] = b.clone();
        }
        if let Some(g) = &opts.goal {
            request_value["goal"] = g.clone();
        }
        let request_json = serde_json::to_vec(&request_value).unwrap();
        let hashed = request_json.clone();
        let stored = if opts.tamper_request {
            b"{\"prompt\":\"tampered\"}".to_vec()
        } else {
            request_json.clone()
        };
        aws.put(&keys::request_key(RUN), stored);

        let direct = DirectConfig {
            events_every: Duration::from_millis(100),
            heartbeat_every: Duration::from_millis(300),
            upload_retry_pause: Duration::from_millis(10),
            // The rows of the stop table are one prompt each; the retry tests turn it on through `edit_cfg`.
            provider_retries: 0,
            provider_retry_pause: Duration::from_millis(10),
            // The rows of the stop table are one prompt each, so the review round is off here; `direct_review_run.rs` turns it on
            // through `edit_cfg`.
            review_round: false,
            ..DirectConfig::default()
        };
        let mut cfg = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            profiles_dir: profiles,
            fixtures_root: repo_root(),
            templates_root: repo_root(),
            workspace_root: dir.join("mnt-workspace"),
            data_root: dir.join("var-lib-agentd"),
            runtime_dir: short_runtime_dir(),
            engine_path: vec![bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
            agentd_exe: PathBuf::from(env!("CARGO_BIN_EXE_agentd")),
            engine_user: None,
            engine_sandbox: agentd::sandbox::Setting::Mode(agentd::sandbox::Mode::Uid),
            engine_identity: agentd::sandbox::IdentitySetting::default(),
            identity_helper: PathBuf::from("/opt/harness/bin/ham-nobody-exec"),
            s3_endpoint: Some(aws.base.clone()),
            packages: None,
            internal_api_endpoint: Some(aws.base.clone()),
            goproxy: None,
            direct,
        };
        if let Some(edit) = opts.edit_cfg {
            edit(&mut cfg);
        }
        let log = MemoryLog::new();
        let mut env = process_env(&cfg);
        env.extend(
            opts.agentd_env
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone())),
        );
        let app = App::new(cfg, log.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut process = None;
        if as_process {
            // The process takes the port the listener held; the in-process server is not started.
            drop(listener);
            let log_file = dir.join("agentd.log");
            let child = std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["serve", "--listen", &addr.to_string()])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(env)
                .stderr(std::fs::File::create(&log_file).unwrap())
                .spawn()
                .unwrap();
            process = Some(Process { child, log_file });
            let deadline = Instant::now() + Duration::from_secs(10);
            while http("GET", &format!("http://{addr}/ping"), None).await.0 != 200 {
                assert!(Instant::now() < deadline, "agentd serve did not come up");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } else {
            let served = app.clone();
            tokio::spawn(async move { serve(served, listener).await.unwrap() });
        }

        let uri = |key: String| keys::uri(BUCKET, &key);
        let req = DirectInvocationRequest {
            kind: DirectKind::Direct,
            run_id: RUN.into(),
            agent_id: "direct".into(),
            turn: 1,
            runtime_session_id: format!("{RUN}-direct-g1"),
            credentials: creds(&format_rfc3339(now_unix() + 3600)),
            nonce: CANARY_NONCE.into(),
            internal_api_url: "https://127.0.0.1:1/direct".into(),
            region: "us-east-1".into(),
            engine: opts.engine.into(),
            model: DirectModelSpec {
                id: MODEL.into(),
                region: "us-east-1".into(),
                protocol: Protocol::Converse,
                context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                default_reasoning_effort: None,
            },
            failover: opts.failover.clone(),
            request: InputRef {
                uri: uri(keys::request_key(RUN)),
                sha256: sha(&hashed),
            },
            repo: RepoSource::S3Bundle {
                uri: uri(keys::repo_bundle_key(RUN)),
            },
            timeout_s: opts.timeout_s,
            provider_silence_s: opts.provider_silence_s,
            snapshot_every_s: opts.snapshot_every_s,
            packages_token: opts.packages_token.then(|| {
                harness_protocol::runtime::PackagesToken::new(CANARY_TOKEN.replace('-', "."))
            }),
            packages_token_expires_at: opts
                .packages_token
                .then(|| u64::try_from(now_unix() + 3600).unwrap()),
            input_wait_s: opts.input_wait_s,
            slice: None,
        };
        Self {
            process,
            aws,
            base: format!("http://{addr}"),
            log,
            dir,
            app,
            req,
            request_json,
        }
    }

    pub async fn post_raw(&self, body: String) -> (u16, String) {
        http("POST", &format!("{}/invocations", self.base), Some(body)).await
    }

    pub async fn post(&self) -> (u16, String) {
        self.post_raw(serde_json::to_string(&self.req).unwrap())
            .await
    }

    pub async fn ping(&self) -> PingStatus {
        let (_, body) = http("GET", &format!("{}/ping", self.base), None).await;
        serde_json::from_str::<PingResponse>(&body).unwrap().status
    }

    pub fn agent_data(&self) -> PathBuf {
        self.dir.join("var-lib-agentd").join(RUN).join("direct")
    }

    pub fn result_path(&self) -> PathBuf {
        turn_result_path(
            &self.dir.join("var-lib-agentd"),
            RUN,
            "direct",
            self.req.turn,
        )
    }

    /// Waits for result.json, which agentd writes last.
    pub async fn wait_result(&self) -> InvocationResponse {
        let deadline = Instant::now() + Duration::from_secs(u64::from(self.req.timeout_s) + 90);
        loop {
            if let Ok(text) = std::fs::read_to_string(self.result_path())
                && let Ok(r) = serde_json::from_str(&text)
            {
                no_secrets(&text);
                return r;
            }
            assert!(
                Instant::now() < deadline,
                "no result at {}",
                self.result_path().display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn wait_healthy(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.ping().await != PingStatus::Healthy {
            assert!(
                Instant::now() < deadline,
                "/ping stayed busy after the result"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// POSTs the invocation (answered before the turn ends), then waits for the result.
    pub async fn invoke(&self) -> (InvocationAccepted, InvocationResponse) {
        let t0 = Instant::now();
        let (code, text) = self.post().await;
        assert_eq!(code, 200, "{text}");
        no_secrets(&text);
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the invocation waited for the turn: {:?}",
            t0.elapsed()
        );
        let ack: InvocationAccepted = serde_json::from_str(&text).unwrap();
        (ack, self.wait_result().await)
    }

    pub fn summary(&self) -> DirectSummary {
        let bytes = self
            .aws
            .get(&keys::summary_key(RUN))
            .expect("summary.json in the bucket");
        no_secrets(&String::from_utf8_lossy(&bytes));
        let s: DirectSummary = serde_json::from_slice(&bytes).unwrap();
        s.validate().unwrap();
        s
    }

    /// The uploaded bundle, written to a file and verified.
    pub fn bundle_file(&self) -> Option<PathBuf> {
        let bytes = self.aws.get(&keys::bundle_key(RUN))?;
        let p = self.dir.join("uploaded.bundle");
        std::fs::write(&p, bytes).unwrap();
        Some(p)
    }

    pub fn worktree(&self) -> PathBuf {
        self.dir.join("mnt-workspace").join(RUN).join("direct/repo")
    }

    pub fn fake_engine_log(&self) -> String {
        std::fs::read_to_string(self.agent_data().join("data/fake-acp.log")).unwrap_or_default()
    }

    /// The lines of the fake engine's sessions log (`new-session <id> generation=<g>`, `prompt <id> history=<k>`), in order. The engine's
    /// state directory is removed from the work tree when a run ends, so the lines are read from its log in the data directory.
    pub fn engine_sessions(&self) -> Vec<String> {
        self.fake_engine_log()
            .lines()
            .filter_map(|l| l.strip_prefix("sessions: "))
            .map(String::from)
            .collect()
    }

    /// The model ids the engine was switched to with `session/set_config_option`, in order.
    pub fn model_switches(&self) -> Vec<String> {
        self.fake_engine_log()
            .lines()
            .filter_map(|l| l.strip_prefix("in "))
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["method"] == "session/set_config_option")
            .filter_map(|v| v["params"]["value"].as_str().map(String::from))
            .collect()
    }

    /// A short trace of what the fake engine saw and said (requests with their session, stops and errors, silences, exits) and of what
    /// agentd logged about the answer window, for the message of a failed assertion.
    pub fn engine_trace(&self) -> String {
        let mut out = Vec::new();
        for l in self.fake_engine_log().lines() {
            if let Some(j) = l.strip_prefix("in ") {
                let v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
                if let Some(m) = v["method"].as_str() {
                    out.push(format!(
                        "in {m} {}",
                        v["params"]["sessionId"].as_str().unwrap_or("")
                    ));
                }
            } else if let Some(j) = l.strip_prefix("out ") {
                let v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
                if let Some(r) = v["result"]["stopReason"].as_str() {
                    out.push(format!("out stop {r}"));
                } else if v["error"].is_object() {
                    out.push(format!("out error {}", v["error"]["code"]));
                }
            } else if l.starts_with("silent")
                || l.starts_with("exiting")
                || l.starts_with("sessions:")
                || l.starts_with("notification")
                || l.starts_with("outage")
            {
                out.push(l.to_string());
            }
        }
        // what agentd itself said about the answer window and the questions
        out.extend(
            self.log
                .lines()
                .into_iter()
                .filter(|l| {
                    l.contains("answer window") || l.contains("needs-input") || l.contains("input")
                })
                .map(|l| format!("agentd log: {l}")),
        );
        out.join("\n")
    }

    pub fn engine_probe(&self) -> String {
        std::fs::read_to_string(self.agent_data().join("home/engine-probe.txt")).unwrap_or_default()
    }

    /// The `session/prompt` requests the fake engine received, as the user text of each.
    pub fn prompts_received(&self) -> Vec<String> {
        self.fake_engine_log()
            .lines()
            .filter_map(|l| l.strip_prefix("in "))
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["method"] == "session/prompt")
            .map(|v| {
                v["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// Agent events of one type, in order.
    pub fn events_of(&self, t: EventType) -> Vec<serde_json::Value> {
        self.aws
            .events()
            .into_iter()
            .filter(|(e, _)| *e == t)
            .map(|(_, v)| v)
            .collect()
    }
}

pub async fn http(method: &str, url: &str, body: Option<String>) -> (u16, String) {
    let out = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "-",
            "-w",
            "\n%{http_code}",
            "-X",
            method,
            "-H",
            "content-type: application/json",
        ])
        .args(
            body.map(|b| vec!["--data-binary".to_string(), b])
                .unwrap_or_default(),
        )
        .arg(url)
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap();
    (code.parse().unwrap(), body.to_string())
}

pub fn no_secrets(s: &str) {
    for c in [CANARY_SECRET, CANARY_TOKEN, CANARY_NONCE] {
        assert!(!s.contains(c), "secret leaked: {c}");
    }
}
