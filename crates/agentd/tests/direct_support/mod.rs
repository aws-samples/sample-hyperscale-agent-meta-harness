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
use axum::extract::{ConnectInfo, Path as AxPath, State};
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

/// How long a test waits, in real time, for real work it knows to be under way. The start of a real `agentd serve` is such work
/// ([`Direct::start`]): a crowded machine starts a debug build slowly. It is a wait and no bound of a rule: no test measures a start
/// against it, and the cost test reads its own figure ([`Process::start_took`]).
pub const REAL_WORK_WITHIN: Duration = Duration::from_secs(60);

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

/// How a fake S3 GET that is made to fail ends (`FakeAws::fail_gets_as`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GetFailure {
    /// A 500 answer with S3's `InternalError` document.
    Status500,
    /// A 503 answer with S3's `SlowDown` document: the service throttles.
    SlowDown,
    /// The head of the answer says 200 and the connection breaks while the body is being sent (the client sees a transport error).
    ResetBody,
    /// A 403 answer with S3's `AccessDenied` document, for a key that is there.
    Denied,
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
    /// Substrings of `bucket/key`: a GET of an absent key that contains one answers 403 `AccessDenied`, as S3 does for a role that may
    /// not list the bucket (it hides whether the key exists).
    get_absent_403: Mutex<Vec<String>>,
    events: Mutex<Vec<Envelope>>,
    batches: Mutex<Vec<(u32, Option<bool>)>>,
    heartbeats: Mutex<Vec<HeartbeatRequest>>,
    complete: Mutex<Vec<TurnCompleteRequest>>,
    cancel_on: Mutex<Option<EventType>>,
    cancel_sent: Mutex<bool>,
    /// The Cancel is told on every events answer from then on, as worker-api tells it while the user's cancel is recorded.
    cancel_always: Mutex<bool>,
    /// The events answers that carried a Cancel.
    cancels_told: Mutex<u32>,
    /// `(substring of bucket/key, pause)`: a GET whose key contains the substring is answered after the pause.
    get_delays: Mutex<Vec<(String, Duration)>>,
    /// Every GET of an object that arrived, in order: `bucket/key` and the `range` header it carried, if any.
    gets: Mutex<Vec<(String, Option<String>)>>,
    /// When each of those GETs arrived, in the same order: `bucket/key` and the moment.
    get_arrivals: Mutex<Vec<(String, Instant)>>,
    /// `(substring of bucket/key, left, how)`: the next `left` GETs whose key contains the substring fail as `how` says.
    get_failures: Mutex<Vec<(String, u32, GetFailure)>>,
    /// `(substring of bucket/key, pause)`: a PUT whose key contains the substring is answered after the pause.
    put_delays: Mutex<Vec<(String, Duration)>>,
    /// `(substring of bucket/key, pause, left)`: the next `left` PUTs whose key contains the substring are answered after the pause.
    put_delays_next: Mutex<Vec<(String, Duration, u32)>>,
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
    /// Every heartbeat with `ending: true` that arrived, answered or not: the request, the completion key it carried, and how many
    /// events posts had arrived before it.
    ending_notices: Mutex<Vec<(HeartbeatRequest, Option<String>, usize)>>,
    /// The next this-many heartbeats with `ending: true` answer 503 (`u32::MAX`: every one).
    ending_failures: Mutex<u32>,
    /// `(substring of bucket/key, bytes)`: the body of the next PUT whose key contains the substring is read up to that many
    /// bytes and then held (`FakeAws::hold_put_body`).
    put_body_hold: Mutex<Option<(String, usize)>>,
    /// A PUT is held in its body now.
    put_body_held: std::sync::atomic::AtomicBool,
    /// The PUT that is held may go on (`FakeAws::release_put_body`).
    put_body_released: std::sync::atomic::AtomicBool,
    /// Every PUT the store answered 200 on a connection that it keeps open, in order: its `bucket/key`, and whether the
    /// client has since closed that connection (`FakeAws::put_answers`).
    put_answers: Mutex<Vec<(String, Arc<std::sync::atomic::AtomicBool>)>>,
    /// Every request as its bytes came in on a connection of a fake that holds its acknowledgements back
    /// (`FakeAws::start_holding_acks`), in the order in which each was whole (`FakeAws::arrivals`).
    arrivals: Mutex<Vec<Arrival>>,
    /// How many heads of requests such a fake has read, and the notice of each one (`FakeAws::head_seen`).
    heads: std::sync::atomic::AtomicUsize,
    head_notice: tokio::sync::Notify,
    /// How often such a fake could not ask the kernel to hold an acknowledgement back, and how many of its connections
    /// carried a request it could not cut out of the bytes (`FakeAws::stand_in_faults`).
    ack_faults: std::sync::atomic::AtomicUsize,
    uncut: std::sync::atomic::AtomicUsize,
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

/// One connection of a client to the fake, from the moment the fake accepts it to the end of the task that serves it. The
/// fake keeps a connection of HTTP/1.1 open after an answer of 200 unless the request asked to close it, so that task ends when
/// the client has closed the connection.
#[derive(Clone)]
struct Connection(Arc<ConnectionOpen>);

/// What a [`Connection`] leaves behind: `closed` is set when its last holder is gone, and that is the task that served it.
struct ConnectionOpen {
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for ConnectionOpen {
    fn drop(&mut self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A connection the fake has accepted, as `axum::serve` hands it to the service of that connection.
type Accepted<'a> = axum::serve::IncomingStream<'a, tokio::net::TcpListener>;

impl axum::extract::connect_info::Connected<Accepted<'_>> for Connection {
    fn connect_info(_: Accepted<'_>) -> Self {
        Self(Arc::new(ConnectionOpen {
            closed: Arc::default(),
        }))
    }
}

// ---- a fake that holds its acknowledgements back ----------------------------------------------------------------------------

/// One request as its bytes came in on a connection of a fake that holds its acknowledgements back
/// ([`FakeAws::start_holding_acks`]).
#[derive(Debug, Clone)]
pub struct Arrival {
    pub method: String,
    /// The path of the request line.
    pub path: String,
    /// The request's place on its connection, from 0. The kernel acknowledges the first segments of a new connection at once, so only
    /// a request on a kept connection (1 or more) can show a wait for an acknowledgement.
    pub nth: usize,
    /// The bytes of the body the head names (`content-length`).
    pub body: usize,
    /// How many bytes of the body came in the read that ended the head. 0: the head came alone.
    pub with_head: usize,
    /// From the read that ended the head to the read that brought the first byte of the body.
    pub body_began_after: Duration,
    /// From the read that ended the head to the read that brought the last byte of the body.
    pub body_whole_after: Duration,
}

/// The bytes of one connection, cut into requests as they come in. It reads HTTP/1.1 requests whose body has a `content-length`, which
/// is what the worker's clients and the tests send; a connection with any other request is counted as not cut
/// ([`FakeAws::stand_in_faults`]) and no request of it is noted after that.
struct Cut {
    state: Shared,
    /// The bytes of a head that is not whole yet.
    head: Vec<u8>,
    nth: usize,
    /// The request whose body is coming in: what is known of it, the bytes of its body so far, and when its head was whole.
    open: Option<(Arrival, usize, Instant)>,
    lost: bool,
}

impl Cut {
    fn new(state: Shared) -> Self {
        Self {
            state,
            head: Vec::new(),
            nth: 0,
            open: None,
            lost: false,
        }
    }

    fn note(&self, arrival: Arrival) {
        self.state.arrivals.lock().unwrap().push(arrival);
    }

    /// The bytes of one read, read at `now`.
    fn read(&mut self, mut bytes: &[u8], now: Instant) {
        use std::sync::atomic::Ordering::SeqCst;
        while !bytes.is_empty() && !self.lost {
            if let Some((mut arrival, got, head_at)) = self.open.take() {
                let take = (arrival.body - got).min(bytes.len());
                if got == 0 {
                    arrival.body_began_after = now.saturating_duration_since(head_at);
                }
                bytes = &bytes[take..];
                if got + take == arrival.body {
                    arrival.body_whole_after = now.saturating_duration_since(head_at);
                    self.note(arrival);
                } else {
                    self.open = Some((arrival, got + take, head_at));
                }
                continue;
            }
            let from = self.head.len().saturating_sub(3);
            self.head.extend_from_slice(bytes);
            let Some(at) = self.head[from..].windows(4).position(|w| w == b"\r\n\r\n") else {
                return;
            };
            let end = from + at + 4;
            let after = self.head.len() - end;
            bytes = &bytes[bytes.len() - after..];
            let head = String::from_utf8_lossy(&self.head[..end]).into_owned();
            self.head.clear();
            let mut line = head.lines().next().unwrap_or_default().split(' ');
            let method = line.next().unwrap_or_default().to_string();
            let path = line.next().unwrap_or_default().to_string();
            let field = |name: &str| {
                head.lines().skip(1).find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                })
            };
            let body = match (field("content-length"), field("transfer-encoding")) {
                (Some(n), None) => n.parse::<usize>().ok(),
                (None, None) => Some(0),
                _ => None,
            };
            let Some(body) = body else {
                self.lost = true;
                self.state.uncut.fetch_add(1, SeqCst);
                return;
            };
            let arrival = Arrival {
                method,
                path,
                nth: self.nth,
                body,
                with_head: after.min(body),
                body_began_after: Duration::ZERO,
                body_whole_after: Duration::ZERO,
            };
            self.nth += 1;
            self.state.heads.fetch_add(1, SeqCst);
            self.state.head_notice.notify_waiters();
            if body == 0 {
                self.note(arrival);
            } else {
                self.open = Some((arrival, 0, now));
            }
        }
    }
}

/// A connection of a fake that holds its acknowledgements back. It asks the kernel to do so (`TCP_QUICKACK` off) when the
/// connection is accepted, after every read and before every write, as a peer does that has just answered a request: the kernel takes
/// the setting back by itself, and without it the kernel's own state decides from request to request whether the acknowledgement of a
/// head waits. Every byte it reads goes through [`Cut`].
struct AckHeld {
    stream: tokio::net::TcpStream,
    cut: Cut,
}

impl AckHeld {
    fn hold(&self) {
        if rustix::net::sockopt::set_tcp_quickack(&self.stream, false).is_err() {
            self.cut
                .state
                .ack_faults
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl tokio::io::AsyncRead for AckHeld {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let this = &mut *self;
        let read = std::pin::Pin::new(&mut this.stream).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &read {
            this.cut.read(&buf.filled()[before..], Instant::now());
            this.hold();
        }
        read
    }
}

impl tokio::io::AsyncWrite for AckHeld {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.hold();
        std::pin::Pin::new(&mut self.stream).poll_write(cx, data)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.hold();
        std::pin::Pin::new(&mut self.stream).poll_write_vectored(cx, data)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// The listener of a fake that holds its acknowledgements back.
struct AckHolding {
    listener: tokio::net::TcpListener,
    state: Shared,
}

impl axum::serve::Listener for AckHolding {
    type Io = AckHeld;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => {
                    let io = AckHeld {
                        stream,
                        cut: Cut::new(self.state.clone()),
                    };
                    io.hold();
                    return (io, addr);
                }
                // As `axum::serve` does for its own listener: an accept that failed is tried again after a moment.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, AckHolding>>
    for Connection
{
    fn connect_info(_: axum::serve::IncomingStream<'_, AckHolding>) -> Self {
        Self(Arc::new(ConnectionOpen {
            closed: Arc::default(),
        }))
    }
}

/// Lets the calling thread run on one processor only, the one it runs on now, and gives back the processors it had. A process
/// takes the processors of the thread that starts it, so a program that is started between this call and the call that sets the
/// thread's processors back runs on that one processor, with everything it starts ([`Opts::one_processor`]).
fn on_one_processor() -> rustix::thread::CpuSet {
    use rustix::thread::{CpuSet, sched_getaffinity, sched_getcpu, sched_setaffinity};
    let all = sched_getaffinity(None).unwrap();
    let here = sched_getcpu();
    let one_of = if all.is_set(here) {
        here
    } else {
        (0..CpuSet::MAX_CPU)
            .find(|cpu| all.is_set(*cpu))
            .expect("a processor this thread may run on")
    };
    let mut one = CpuSet::new();
    one.set(one_of);
    sched_setaffinity(None, &one).unwrap();
    all
}

/// `t<k>-` for a key in the folder of session `k` of a run (`.../turns/<k>/...`) when `k` is 2 or more, else nothing.
fn later_session_of(full: &str) -> String {
    let parts: Vec<&str> = full.split('/').collect();
    parts
        .windows(2)
        .find_map(|w| {
            (w[0] == "turns")
                .then(|| w[1].parse::<u32>().ok())
                .flatten()
        })
        .filter(|k| *k >= 2)
        .map(|k| format!("t{k}-"))
        .unwrap_or_default()
}

/// Stores `body` as the newest version of `full` (`bucket/key`) and returns its version id.
fn record_version(s: &AwsState, full: &str, body: &[u8]) -> String {
    // The id names the key's last segment and the version's place in that key's history, so it is the same on every run of the same flow
    // (the golden streams carry it) whatever order the other keys are written in. The id of a key of a later session of a run also names
    // that session: `turns/1/snapshot.bundle` and `turns/2/snapshot.bundle` each hold a first version, and a real bucket gives no
    // two versions one id, so a test tells the copies of two sessions apart by the id alone. A counter of the whole bucket would do that
    // too, but its ids would follow the order in which other keys are written. The ids of a first session's keys stay as they were.
    let mut versions = s.versions.lock().unwrap();
    let history = versions.entry(full.to_string()).or_default();
    let name = full.rsplit('/').next().unwrap_or_default();
    let id = format!(
        "fake-{}-{}{name}",
        history.len() + 1,
        later_session_of(full)
    );
    history.push((id.clone(), body.to_vec()));
    id
}

/// The largest body a PUT of the fake store carries, unless a hold was set for its key ([`put_body`]).
const MAX_PUT_BYTES: usize = 2 * 1024 * 1024;

/// The body of a PUT of `full` (`bucket/key`), at most [`MAX_PUT_BYTES`] of it. When a hold is set for the key
/// (`FakeAws::hold_put_body`), the body is read up to the bytes the hold names, then nothing more of it is read until the hold is
/// released (`FakeAws::release_put_body`), and then the rest is read whatever its size: the request stays in flight with a part of
/// its body on the way, as an upload to a bucket that stopped reading does.
async fn put_body(
    s: &AwsState,
    full: &str,
    body: axum::body::Body,
) -> Result<Bytes, Box<axum::response::Response>> {
    use http_body_util::BodyExt;
    use std::sync::atomic::Ordering::SeqCst;
    let hold = {
        let mut hold = s.put_body_hold.lock().unwrap();
        match hold.as_ref() {
            Some((sub, _)) if full.contains(sub.as_str()) => hold.take(),
            _ => None,
        }
    };
    let Some((_, after)) = hold else {
        return axum::body::to_bytes(body, MAX_PUT_BYTES)
            .await
            .map_err(|_| Box::new(StatusCode::PAYLOAD_TOO_LARGE.into_response()));
    };
    let mut body = body;
    let mut got: Vec<u8> = Vec::new();
    let mut held = false;
    loop {
        if !held && got.len() >= after {
            held = true;
            s.put_body_held.store(true, SeqCst);
            while !s.put_body_released.load(SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            s.put_body_held.store(false, SeqCst);
        }
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    got.extend_from_slice(&data);
                }
            }
            Some(Err(_)) => return Err(Box::new(StatusCode::BAD_REQUEST.into_response())),
            None => return Ok(Bytes::from(got)),
        }
    }
}

async fn s3_put(
    State(s): State<Shared>,
    AxPath((bucket, key)): AxPath<(String, String)>,
    ConnectInfo(connection): ConnectInfo<Connection>,
    version: axum::http::Version,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let full = format!("{bucket}/{key}");
    let body = match put_body(&s, &full, body).await {
        Ok(body) => body,
        Err(refusal) => return *refusal,
    };
    let next = s
        .put_delays_next
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(sub, _, left)| *left > 0 && full.contains(sub.as_str()))
        .map(|(_, d, left)| {
            *left -= 1;
            *d
        });
    let pause = next.or_else(|| {
        s.put_delays
            .lock()
            .unwrap()
            .iter()
            .find(|(sub, _)| full.contains(sub.as_str()))
            .map(|(_, d)| *d)
    });
    if let Some(d) = pause {
        tokio::time::sleep(d).await;
    }
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
    // The object is stored and this PUT is answered 200 on this connection. Nothing was awaited since the object was stored,
    // so whoever sees the object in the store finds this entry too. An answer on a connection that the fake ends itself after the
    // answer (the request asked for that, or it is not HTTP/1.1) is not entered: the end of that connection would not tell that
    // the client has read the answer.
    let asks_to_close = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("close"));
    if version == axum::http::Version::HTTP_11 && !asks_to_close {
        s.put_answers
            .lock()
            .unwrap()
            .push((full.clone(), connection.0.closed.clone()));
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
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let full = format!("{bucket}/{key}");
    // A ranged GET names the bytes it wants (`bytes=0-<n>`); every GET is recorded with it.
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    s.get_arrivals
        .lock()
        .unwrap()
        .push((full.clone(), Instant::now()));
    s.gets.lock().unwrap().push((full.clone(), range.clone()));
    let failing = s
        .get_failures
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(sub, left, _)| *left > 0 && full.contains(sub.as_str()))
        .map(|(_, left, how)| {
            *left -= 1;
            *how
        });
    if let Some(how) = failing {
        return match how {
            GetFailure::Status500 => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "<Error><Code>InternalError</Code><Message>We encountered an internal error.</Message></Error>",
            )
                .into_response(),
            GetFailure::SlowDown => (
                StatusCode::SERVICE_UNAVAILABLE,
                "<Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message></Error>",
            )
                .into_response(),
            GetFailure::ResetBody => {
                (StatusCode::OK, axum::body::Body::new(ResetBody)).into_response()
            }
            GetFailure::Denied => (
                StatusCode::FORBIDDEN,
                "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
            )
                .into_response(),
        };
    }
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
    let hidden = s
        .get_absent_403
        .lock()
        .unwrap()
        .iter()
        .any(|sub| full.contains(sub.as_str()));
    // A bucket that keeps versions answers a GET with the version it read (`x-amz-version-id`), as it answers a PUT and a
    // HEAD. Here that is the newest version of the key; a bucket without versions answers none.
    let version = if *s.no_version_ids.lock().unwrap() {
        None
    } else {
        s.versions
            .lock()
            .unwrap()
            .get(&full)
            .and_then(|v| v.last().map(|(id, _)| id.clone()))
    };
    let versioned = |mut resp: axum::response::Response| {
        if let Some(id) = &version {
            resp.headers_mut()
                .insert("x-amz-version-id", id.parse().unwrap());
        }
        resp
    };
    match s.objects.lock().unwrap().get(&full) {
        // The first bytes of the object, as S3 answers `bytes=0-<n>`: 206 with the range, clipped to the object.
        Some(b) => match range
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes=0-"))
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(last) if !b.is_empty() => {
                let end = last.min(b.len() - 1);
                versioned(
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [("content-range", format!("bytes 0-{end}/{}", b.len()))],
                        b[..=end].to_vec(),
                    )
                        .into_response(),
                )
            }
            _ => versioned((StatusCode::OK, b.clone()).into_response()),
        },
        None if hidden => (
            StatusCode::FORBIDDEN,
            "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
        )
            .into_response(),
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
        if !*sent || *s.cancel_always.lock().unwrap() {
            *sent = true;
            *s.cancels_told.lock().unwrap() += 1;
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

async fn api_heartbeat(
    State(s): State<Shared>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let beat: HeartbeatRequest = serde_json::from_slice(&body).unwrap();
    if beat.ending == Some(true) {
        // The notice of an end. The real plane checks the run's key on it and may fail its callback (503).
        let key = headers
            .get(harness_protocol::store::COMPLETION_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let batches = s.batches.lock().unwrap().len();
        s.ending_notices.lock().unwrap().push((beat, key, batches));
        let mut left = s.ending_failures.lock().unwrap();
        if *left > 0 {
            if *left != u32::MAX {
                *left -= 1;
            }
            return (StatusCode::SERVICE_UNAVAILABLE, "fake callback failure").into_response();
        }
        return axum::Json(serde_json::json!({"ok": true})).into_response();
    }
    s.heartbeats.lock().unwrap().push(beat);
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
    /// The routes of the fake, for either listener.
    fn routes(state: &Shared) -> Router {
        Router::new()
            .route("/internal/events", post(api_events))
            .route("/internal/heartbeat", post(api_heartbeat))
            .route("/internal/turn-complete", post(api_complete))
            .route("/internal/creds", post(api_creds))
            .route("/{bucket}/{*key}", get(s3_get).put(s3_put).head(s3_head))
            .route("/{bucket}", get(s3_list))
            .with_state(state.clone())
    }

    pub async fn start() -> Self {
        let state: Shared = Arc::default();
        let app = Self::routes(&state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Every request carries the connection it came on, so the store can tell when a client has closed one.
        let app = app.into_make_service_with_connect_info::<Connection>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            state,
        }
    }

    /// The same fake behind a listener that holds its acknowledgements back ([`AckHeld`]) and notes how the bytes of every
    /// request came in ([`FakeAws::arrivals`]). A client that leaves Nagle's algorithm on and sends a request as two writes on a
    /// kept connection waits here for the acknowledgement of the first; a client with no-delay does not. A test that judges by it
    /// shows both with clients of its own in the same run.
    pub async fn start_holding_acks() -> Self {
        let state: Shared = Arc::default();
        let app = Self::routes(&state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app.into_make_service_with_connect_info::<Connection>();
        let listener = AckHolding {
            listener,
            state: state.clone(),
        };
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            state,
        }
    }

    /// Every request a fake that holds its acknowledgements back has read whole, in that order.
    pub fn arrivals(&self) -> Vec<Arrival> {
        self.state.arrivals.lock().unwrap().clone()
    }

    /// How many heads of requests such a fake has read.
    pub fn heads_seen(&self) -> usize {
        self.state.heads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Waits until such a fake has read more than `before` heads of requests.
    pub async fn head_seen(&self, before: usize) {
        loop {
            let notice = self.state.head_notice.notified();
            if self.heads_seen() > before {
                return;
            }
            notice.await;
        }
    }

    /// What such a fake could not do: the calls that asked the kernel to hold an acknowledgement back and failed, and the
    /// connections with a request it could not cut out of the bytes. A test that judges by [`FakeAws::arrivals`] holds both at 0.
    pub fn stand_in_faults(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::SeqCst;
        (
            self.state.ack_faults.load(SeqCst),
            self.state.uncut.load(SeqCst),
        )
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

    /// A GET of an absent key whose `bucket/key` contains `sub` answers 403 from now on (the role may not list the bucket, so S3 hides
    /// a missing key).
    pub fn absent_gets_answer_403(&self, sub: &str) {
        self.state
            .get_absent_403
            .lock()
            .unwrap()
            .push(sub.to_string());
    }

    /// The GETs of `key` that arrived, in order: the `range` header of each (`None`: the whole object was asked for).
    pub fn gets_of(&self, key: &str) -> Vec<Option<String>> {
        let full = format!("{BUCKET}/{key}");
        self.state
            .gets
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == full)
            .map(|(_, range)| range.clone())
            .collect()
    }

    /// When the GETs of `key` arrived, in order ([`Self::gets_of`] has one entry for each).
    pub fn get_arrivals_of(&self, key: &str) -> Vec<Instant> {
        let full = format!("{BUCKET}/{key}");
        self.state
            .get_arrivals
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == full)
            .map(|(_, at)| *at)
            .collect()
    }

    /// Takes the object at `key` out of the bucket: a GET of it answers 404 from now on. Its earlier versions stay recorded.
    pub fn remove(&self, key: &str) {
        self.state
            .objects
            .lock()
            .unwrap()
            .remove(&format!("{BUCKET}/{key}"));
    }

    /// The next `times` GETs whose `bucket/key` contains `substring` answer 500.
    pub fn fail_gets(&self, substring: &str, times: u32) {
        self.fail_gets_as(substring, times, GetFailure::Status500);
    }

    /// The next `times` GETs whose `bucket/key` contains `substring` fail as `how` says.
    pub fn fail_gets_as(&self, substring: &str, times: u32, how: GetFailure) {
        self.state
            .get_failures
            .lock()
            .unwrap()
            .push((substring.to_string(), times, how));
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

    /// The next `times` PUTs whose bucket/key contains `substring` are answered after `pause`; the ones after them at once (one request
    /// that hangs, then a store that answers again).
    pub fn delay_next_puts(&self, substring: &str, pause: Duration, times: u32) {
        self.state
            .put_delays_next
            .lock()
            .unwrap()
            .push((substring.to_string(), pause, times));
    }

    /// A PUT whose bucket/key contains `substring` is answered after `pause` (a store that is slow to take an object).
    /// The body of the next PUT whose key contains `substring` is read up to `after` bytes and then held: the fake store reads
    /// nothing more of it, and the request stays in flight, until [`Self::release_put_body`]. Such a PUT may be larger than the fake
    /// store otherwise takes.
    pub fn hold_put_body(&self, substring: &str, after: usize) {
        self.state
            .put_body_released
            .store(false, std::sync::atomic::Ordering::SeqCst);
        *self.state.put_body_hold.lock().unwrap() = Some((substring.to_string(), after));
    }

    /// Whether a PUT is held in its body now ([`Self::hold_put_body`]).
    pub fn put_body_held(&self) -> bool {
        self.state
            .put_body_held
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The PUT that is held goes on ([`Self::hold_put_body`]).
    pub fn release_put_body(&self) {
        self.state
            .put_body_released
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// `(given, read)` for the answers of 200 this store gave to PUTs whose bucket/key contains `substring`: how many it
    /// gave, and how many of them the client has read, as far as a store can tell: the client has closed the connection the
    /// answer went out on. The worker makes a client for every store of a copy and drops it when that store has returned
    /// (`Copies::store`), and a closed connection delivers nothing more: an answer that is read is not on its way to the worker
    /// any more, and the call that waited for it has returned. A client that gave its request up has closed the connection as
    /// well; what came of a request is read elsewhere.
    pub fn put_answers(&self, substring: &str) -> (usize, usize) {
        let answers = self.state.put_answers.lock().unwrap();
        let of_key = answers.iter().filter(|(key, _)| key.contains(substring));
        of_key.fold((0, 0), |(given, read), (_, closed)| {
            let closed = closed.load(std::sync::atomic::Ordering::SeqCst);
            (given + 1, read + usize::from(closed))
        })
    }

    pub fn delay_puts(&self, substring: &str, pause: Duration) {
        self.state
            .put_delays
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

    /// As [`FakeAws::cancel_on`], and every later events response that hands out controls carries the Cancel again: what
    /// worker-api answers while the user's cancel is recorded on the Run item.
    pub fn cancel_on_every_answer(&self, t: EventType) {
        *self.state.cancel_always.lock().unwrap() = true;
        self.cancel_on(t);
    }

    /// How many events responses carried a Cancel.
    pub fn cancels_told(&self) -> u32 {
        *self.state.cancels_told.lock().unwrap()
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

    /// Every heartbeat with `ending: true` that arrived: the request, the completion key it carried, and how many events posts
    /// had arrived before it.
    pub fn ending_notices(&self) -> Vec<(HeartbeatRequest, Option<String>, usize)> {
        self.state.ending_notices.lock().unwrap().clone()
    }

    /// The next `times` heartbeats with `ending: true` answer 503 (`u32::MAX`: every one).
    pub fn fail_ending_notices(&self, times: u32) {
        *self.state.ending_failures.lock().unwrap() = times;
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
    /// The real `agentd serve` process is the first process (process 1) of a process-id namespace of its own, as in a session's
    /// microVM, where the kernel gives it no signal it has no handler for. Needs a machine that can make such a namespace
    /// ([`first_process_possible`]).
    pub first_process: bool,
    /// The command of `agentd serve` carries the argument that only a first process gives its worker
    /// (`agentd::first::WORKER_FLAG`). As a first process ([`Opts::first_process`]) agentd then takes no first-process role and serves
    /// itself, which is what every session ran and the arm without the role of the cost test. As an ordinary process the
    /// argument must change nothing, because the parent is not process 1.
    pub serves_itself: bool,
    /// The real `agentd serve` process gets the coverage profile variable of the test process ([`with_coverage_profile`]). For
    /// a test that lets the program end by itself (a stop signal it answers, a worker that ends) and waits for that end: the program
    /// then writes its profile, and the lines that only `agentd serve` as a process reaches count in a coverage run. Every other test
    /// ends the program with KILL at its own end and keeps the cleared environment: a program that is killed while it writes would
    /// leave half a profile behind.
    pub writes_profile: bool,
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
    /// `StartDirectRunRequest.deliveryNote`. The harness writes `Some(false)` unless a test says otherwise: the
    /// worker then tells the engine nothing about the delivery, so the first prompt of every test that does not look at the block, and the
    /// 13 golden streams, are byte for byte what they were. `None` leaves the member out of `request.json`, which is the request of a user
    /// who keeps the default: the worker writes the block of the stored target before the first prompt.
    pub delivery_note: Option<bool>,
    /// `StartDirectRunRequest.target` as the JSON of the request member. `None`
    /// is the zip target, `{"mode": "s3-only"}`, as in every test before.
    pub target: Option<serde_json::Value>,
    /// The fake AWS of the rig holds its acknowledgements back and notes how the bytes of every request came in
    /// ([`FakeAws::start_holding_acks`]). `false` is the fake of every test before.
    pub holds_acks: bool,
    /// The real `agentd serve` process runs on one processor of this machine, with everything it starts. Whether a streamed
    /// PUT of the worker leaves as one write or as two is a race between the thread that reads the file and the connection that
    /// writes the head; on one processor it falls otherwise than on all of them, and how differs from machine to machine.
    pub one_processor: bool,
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
            first_process: false,
            serves_itself: false,
            writes_profile: false,
            review: None,
            build: None,
            goal: None,
            repo_files: Vec::new(),
            repo_moved: Vec::new(),
            delivery_note: Some(false),
            target: None,
            holds_acks: false,
            one_processor: false,
        }
    }
}

/// The program that starts a command as the first process of a process-id namespace of its own.
const UNSHARE: &str = "unshare";

/// The arguments of [`UNSHARE`] before the command: a user namespace that maps the caller to root, a process-id namespace, and the command
/// as its first process.
const FIRST_PROCESS_ARGS: [&str; 4] = ["--user", "--map-root-user", "--pid", "--fork"];

/// Whether this machine can start a command as the first process of a process-id namespace of its own: a probe of the same command
/// line with `true`. A container that refuses `unshare` cannot; a test that needs process 1 says so and does not run there.
///
/// With `HAMH_TEST_REQUIRE_PROCESS_1=1` (a variable of this rig only) a machine that cannot make the namespace fails the test
/// in place of skipping it, so a run that must show the first-process tests cannot count a skip as a pass.
pub fn first_process_possible() -> bool {
    let possible = std::process::Command::new(UNSHARE)
        .args(FIRST_PROCESS_ARGS)
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(
        possible || std::env::var("HAMH_TEST_REQUIRE_PROCESS_1").as_deref() != Ok("1"),
        "HAMH_TEST_REQUIRE_PROCESS_1=1: this machine cannot start a process as the first one of a process-id namespace \
         (unshare --user --map-root-user --pid --fork), and a skipped first-process test is no pass here"
    );
    if !possible && let Some(record) = std::env::var_os(SKIP_RECORD_VAR) {
        record_skip_in(Path::new(&record));
    }
    possible
}

/// The variable that names the file in which a first-process test that skips leaves its `SKIPPED:` line.
const SKIP_RECORD_VAR: &str = "HAMH_TEST_SKIP_RECORD";

/// Appends the `SKIPPED:` line of the test that runs on this thread to `record`.
pub fn record_skip_in(record: &Path) {
    use std::io::Write;
    let thread = std::thread::current();
    let line = format!(
        "SKIPPED: {}: this machine cannot start a process as the first one of a process-id namespace\n",
        thread.name().unwrap_or("a test without a name")
    );
    // One write of the whole line in append mode, so that the lines of tests that run at the same time do not mix.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(record)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The variable that tells an instrumented program where to write its coverage profile. `cargo llvm-cov` sets it for the tests.
const PROFILE_VAR: &str = "LLVM_PROFILE_FILE";

/// Gives `command` the coverage profile variable of this test process, when it has one. The tests start `agentd serve` with
/// a cleared environment, so the program wrote no profile and the lines that only the program reaches counted as not run. With the
/// variable it writes its profile when it ends by itself; a program that a signal's default action ends writes none. Outside a
/// coverage run the variable is not set and nothing changes.
pub fn with_coverage_profile(command: &mut std::process::Command) {
    if let Some(profile) = std::env::var_os(PROFILE_VAR) {
        command.env(PROFILE_VAR, profile);
    }
}

/// A command that runs `program` as the first process of a process-id namespace of its own ([`first_process_possible`]).
pub fn as_first_process(program: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(UNSHARE);
    c.args(FIRST_PROCESS_ARGS).arg(program);
    c
}

/// The ended entries of this machine whose parent is `parent`: processes that have ended and that nobody has waited for (state `Z`),
/// as (process id, name). Read from `/proc/*/stat`, so it sees the processes of a namespace from outside it.
pub fn ended_entries_with_parent(parent: u32) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `pid (name) state parent ...`; the name may hold spaces and brackets, so the fields are read after its last bracket.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let mut rest = stat[close + 1..].split_whitespace();
        let (state, ppid) = (rest.next(), rest.next().and_then(|p| p.parse::<u32>().ok()));
        if state == Some("Z") && ppid == Some(parent) {
            found.push((pid, stat[open + 1..close].to_string()));
        }
    }
    found.sort();
    found
}

/// A `git` that leaves a detached process behind at every call and then is the real git: the shape measured in a session (one ended
/// entry per commit, a leader of a session of its own, handed to process 1). It appends one line to `calls` per call. The process it
/// leaves is a `sleep` of 0.2 s: handed over alive when git ends first, and as an ended entry when git runs longer. It
/// holds none of git's three streams, as a program that detaches itself does: a caller that reads git's output to its end does not
/// wait the 0.2 s for it.
pub fn leaving_git(dir: &Path) -> (PathBuf, PathBuf) {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("leaving-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = dir.join("git-calls.txt");
    let script = format!(
        "#!/bin/sh\nprintf 'call\\n' >> '{calls}'\nsetsid sleep 0.2 </dev/null >/dev/null 2>&1 &\nexec '{real}' \"$@\"\n",
        calls = calls.display()
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, calls)
}

/// The real `agentd serve` binary, killed when the test ends.
pub struct Process {
    child: std::process::Child,
    /// The process id of `agentd serve` on this machine when `child` is the program that made its namespace ([`Opts::first_process`]).
    in_namespace: Option<u32>,
    pub log_file: PathBuf,
    /// How long the program took from its start to its first `/ping` 200.
    pub start_took: Duration,
}

impl Drop for Process {
    fn drop(&mut self) {
        // The first process of a process-id namespace takes every process of the namespace with it; the program that made the namespace
        // would leave it running.
        if let Some(pid) = self.first_to_end() {
            let _ = std::process::Command::new("kill")
                .args(["-s", "KILL", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The first child of the process `outer`: `agentd serve` under the program that made its namespace, and the worker under a first
/// process.
pub fn child_of(outer: u32) -> u32 {
    let children = format!("/proc/{outer}/task/{outer}/children");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = std::fs::read_to_string(&children)
            .ok()
            .and_then(|c| c.split_whitespace().next().and_then(|p| p.parse().ok()));
        if let Some(pid) = found {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "agentd under the namespace program {outer}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Process {
    /// The process that the drop ends with KILL: `agentd serve` as process 1 of its namespace, and only while the program that made
    /// the namespace still runs. That program waits for process 1, so the number stays taken until it has ended; afterwards
    /// the machine may give the number to any other process, and the drop names none.
    pub fn first_to_end(&mut self) -> Option<u32> {
        let pid = self.in_namespace?;
        matches!(self.child.try_wait(), Ok(None)).then_some(pid)
    }

    /// The process id of `agentd serve` on this machine.
    pub fn pid(&self) -> u32 {
        self.in_namespace.unwrap_or_else(|| self.child.id())
    }

    /// The process id, on this machine, of the worker that `agentd serve` started as the first process of its namespace: its
    /// first child. (Where process 1 serves itself, this is whatever it started first.)
    pub fn worker_pid(&self) -> u32 {
        child_of(self.pid())
    }

    /// The process id `agentd serve` has in its own process-id namespace (the last number of the `NSpid` line of its status).
    pub fn own_pid(&self) -> Option<u32> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.pid())).ok()?;
        let line = status.lines().find(|l| l.starts_with("NSpid:"))?;
        line.split_whitespace().last()?.parse().ok()
    }

    /// Whether `agentd serve` runs.
    pub fn alive(&self) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &self.pid().to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Sends the process `signal` by its name (`TERM`, `INT`) with the `kill` program, as a container runtime or a person would.
    pub fn signal(&self, signal: &str) {
        let status = std::process::Command::new("kill")
            .args(["-s", signal, &self.pid().to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "kill -s {signal}: {status}");
    }

    /// `Some(code)` once the process has ended (`None` inside: a signal ended it); `None` while it runs.
    pub fn ended(&mut self) -> Option<Option<i32>> {
        self.child.try_wait().unwrap().map(|s| s.code())
    }

    /// What the process has written to its log so far.
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log_file).unwrap_or_default()
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
        let aws = if opts.holds_acks {
            FakeAws::start_holding_acks().await
        } else {
            FakeAws::start().await
        };
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
            "target": opts.target.clone().unwrap_or_else(|| serde_json::json!({"mode": "s3-only"})),
            "model": MODEL,
            "rules": opts.rules,
            "nudge": opts.nudge,
        });
        if let Some(on) = opts.delivery_note {
            request_value["deliveryNote"] = serde_json::json!(on);
        }
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
            let mut command = if opts.first_process {
                as_first_process(Path::new(env!("CARGO_BIN_EXE_agentd")))
            } else {
                std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
            };
            command
                .args(["serve", "--listen", &addr.to_string()])
                .args(opts.serves_itself.then_some(agentd::first::WORKER_FLAG))
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(env)
                .stderr(std::fs::File::create(&log_file).unwrap());
            if opts.writes_profile {
                with_coverage_profile(&mut command);
            }
            let began = Instant::now();
            // A process takes the processors of the thread that starts it.
            let all = opts.one_processor.then(on_one_processor);
            let child = command.spawn().unwrap();
            if let Some(all) = all {
                rustix::thread::sched_setaffinity(None, &all).unwrap();
            }
            let in_namespace = opts.first_process.then(|| child_of(child.id()));
            process = Some(Process {
                child,
                in_namespace,
                log_file,
                start_took: Duration::ZERO,
            });
            let deadline = Instant::now() + REAL_WORK_WITHIN;
            // A first process is looked for every 5 ms, so that the cost test can tell two starts apart; every other test keeps
            // its 50 ms.
            let pause = Duration::from_millis(if opts.first_process { 5 } else { 50 });
            while http("GET", &format!("http://{addr}/ping"), None).await.0 != 200 {
                assert!(Instant::now() < deadline, "agentd serve did not come up");
                tokio::time::sleep(pause).await;
            }
            if let Some(p) = process.as_mut() {
                p.start_took = began.elapsed();
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
            delivery: None,
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
        self.wait_result_every(Duration::from_millis(50)).await
    }

    /// [`Direct::wait_result`] with a look every `pause`. A test that measures how long a turn takes looks every millisecond: with a
    /// look every 50 ms two turns of the same length differ by nothing or by 50 ms.
    pub async fn wait_result_every(&self, pause: Duration) -> InvocationResponse {
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
            tokio::time::sleep(pause).await;
        }
    }

    pub async fn wait_healthy(&self) {
        self.wait_healthy_every(Duration::from_millis(50)).await;
    }

    /// [`Direct::wait_healthy`] with a look every `pause` (see [`Direct::wait_result_every`]).
    pub async fn wait_healthy_every(&self, pause: Duration) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.ping().await != PingStatus::Healthy {
            assert!(
                Instant::now() < deadline,
                "/ping stayed busy after the result"
            );
            tokio::time::sleep(pause).await;
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

/// Whether the bundle `bytes` names a commit it needs and does not hold (a copy that leaves out what the run's base holds).
pub fn needs_the_base(bytes: &[u8]) -> bool {
    let cut = bytes.len().min(65_536);
    !harness_protocol::bundle_header::needs(&bytes[..cut])
        .expect("a bundle header")
        .is_empty()
}

/// A checkout of the copy of the work stored at `copy_key`, on its `snapshot` branch, in `<dir>/<name>`. A copy whose header
/// names a commit it needs is put together the way every reader does it: the bundle the run started from is taken from the bucket
/// first and the copy is fetched into it. A copy that holds the whole history is cloned alone, as the client's `Look` line says.
pub fn checkout_of_copy(aws: &FakeAws, dir: &Path, copy_key: &str, name: &str) -> PathBuf {
    let copy = aws.get(copy_key).expect("the stored copy");
    checkout_of_copy_bytes(aws, dir, &copy, name)
}

/// [`checkout_of_copy`] for the bytes of a copy: an earlier version of the object, which the bucket keeps.
pub fn checkout_of_copy_bytes(aws: &FakeAws, dir: &Path, copy: &[u8], name: &str) -> PathBuf {
    let bundle = dir.join(format!("{name}.bundle"));
    std::fs::write(&bundle, copy).unwrap();
    let clone = dir.join(name);
    if needs_the_base(copy) {
        let base = dir.join(format!("{name}-base.bundle"));
        std::fs::write(
            &base,
            aws.get(&keys::repo_bundle_key(RUN))
                .expect("the bundle the run started from"),
        )
        .unwrap();
        git(
            dir,
            &[
                "clone",
                "-q",
                "--no-checkout",
                base.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(
            &clone,
            &[
                "fetch",
                "-q",
                bundle.to_str().unwrap(),
                "+refs/heads/snapshot:refs/heads/snapshot",
            ],
        );
        git(&clone, &["checkout", "-q", "snapshot"]);
    } else {
        git(
            dir,
            &[
                "clone",
                "-q",
                "-b",
                "snapshot",
                bundle.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
    }
    clone
}

pub fn no_secrets(s: &str) {
    for c in [CANARY_SECRET, CANARY_TOKEN, CANARY_NONCE] {
        assert!(!s.contains(c), "secret leaked: {c}");
    }
}

/// The fault switch of the session's storage for whole runs. It stands where the two writes of the storage watch are
/// (`App::storage_writes`): the host's writes, except that the first `markers` writes of the copies' marker and the first `tests` test
/// writes are answered with errno 28 without touching the storage. It counts the calls of each kind.
#[derive(Debug)]
pub struct StorageFaults {
    host: agentd::direct::storage_watch::HostWrites,
    refuse_markers: std::sync::atomic::AtomicU32,
    refuse_tests: std::sync::atomic::AtomicU32,
    /// The writes of the copies' marker the watch asked for.
    pub markers: std::sync::atomic::AtomicU32,
    /// The test writes the watch asked for.
    pub tests: std::sync::atomic::AtomicU32,
}

impl StorageFaults {
    /// Whether one more write of this kind is refused; each refusal is taken from the number the test gave.
    fn refuse(left: &std::sync::atomic::AtomicU32) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        left.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }

    fn no_space() -> agentd::direct::storage_watch::WriteError {
        agentd::direct::storage_watch::WriteError {
            step: "write",
            errno: Some(28),
            text: std::io::Error::from_raw_os_error(28).to_string(),
            test_switch: false,
        }
    }
}

impl agentd::direct::storage_watch::StorageWrites for StorageFaults {
    fn test(&self, run_dir: &Path) -> Result<(), agentd::direct::storage_watch::WriteError> {
        self.tests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if Self::refuse(&self.refuse_tests) {
            return Err(Self::no_space());
        }
        agentd::direct::storage_watch::StorageWrites::test(&self.host, run_dir)
    }

    fn marker(
        &self,
        run_dir: &Path,
        bytes: &[u8],
    ) -> Result<(), agentd::direct::storage_watch::WriteError> {
        self.markers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if Self::refuse(&self.refuse_markers) {
            return Err(Self::no_space());
        }
        agentd::direct::storage_watch::StorageWrites::marker(&self.host, run_dir, bytes)
    }
}

impl Direct {
    /// Gives the run the fault switch of the session's storage ([`StorageFaults`]): the first `markers` writes of the copies'
    /// marker and the first `tests` test writes of the watch are refused with errno 28. Call it before the run is invoked.
    pub fn refuse_storage(&self, markers: u32, tests: u32) -> Arc<StorageFaults> {
        let faults = Arc::new(StorageFaults {
            host: agentd::direct::storage_watch::HostWrites,
            refuse_markers: std::sync::atomic::AtomicU32::new(markers),
            refuse_tests: std::sync::atomic::AtomicU32::new(tests),
            markers: std::sync::atomic::AtomicU32::new(0),
            tests: std::sync::atomic::AtomicU32::new(0),
        });
        assert!(
            self.app.storage_writes.set(faults.clone()).is_ok(),
            "the run was given its storage writes before"
        );
        faults
    }
}
