//! The listener, the per-connection state and the request path.
//!
//! Order of checks for a request, each failing with the status the contract names: method (405), origin-form target (400),
//! `Host` equals the listener (400), no request body (400), path rules (400), configured prefix (403), token not expired (401),
//! then the upstream request with its redirects (502 on a refused redirect or an upstream failure), then the answer, rewritten
//! when textual and streamed otherwise.

use crate::body::{Activity, Life, OutBody};
use crate::cargo;
use crate::net::{self, TestNet, UpstreamClient};
use crate::redirect::{self, Loc};
use crate::rewrite::{self, Rewriter};
use crate::stats::Stats;
use crate::target::{self, Refusal};
use crate::{Auth, Config, Log, Prefix};
use base64::Engine;
use bytes::Bytes;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, ALLOW, AUTHORIZATION, CACHE_CONTROL, CONNECTION,
    CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HOST,
    HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED,
    LOCATION, RANGE, TRANSFER_ENCODING, USER_AGENT, VARY,
};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::convert::Infallible;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use zeroize::Zeroizing;

/// Slowest a client may be to finish the request head, at most.
const HEADER_READ_CAP: Duration = Duration::from_secs(30);
/// Textual answers read whole for a rewrite at the same time (each up to 64 MiB, twice over while it is rewritten: 4 x 128 MiB at worst).
const MAX_CONCURRENT_REWRITES: usize = 4;
/// The host an npm tarball link names for the npm CLI. npm replaces it with the registry it is configured to use when it fetches
/// the tarball (`replace-registry-host`, default `npmjs`), and writes it, not the loopback URL, into `package-lock.json`.
const PUBLIC_NPM_HOST: &str = "https://registry.npmjs.org";
/// Connections answered 503 at the same time; beyond this a connection is dropped unanswered.
const MAX_OVERFLOW: usize = 64;
/// Retries of an upstream request (GET and HEAD, the only methods the proxy accepts) that failed before it sent a usable answer.
const UPSTREAM_RETRIES: u32 = 2;
/// The first wait before a retry; the second is twice it. Up to half of the wait is added, so clients that failed together do not
/// retry together.
const RETRY_BASE: Duration = Duration::from_millis(250);

/// The wait before retry number `retry_no` (1 for the first).
fn retry_pause(retry_no: u32) -> Duration {
    let base = RETRY_BASE * 2u32.saturating_pow(retry_no.saturating_sub(1));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    base + base * u32::try_from(nanos % 501).unwrap_or(0) / 1000
}

/// A failed connection worth another try: the peer reset or closed it, or nothing was listening for a moment. A wait that timed out
/// is not (it would triple the wait), and neither are a name that did not resolve, an address the proxy blocks or a certificate it
/// does not trust.
fn retry_class(class: &str) -> bool {
    matches!(class, "reset" | "connect")
}

/// An answer head that says "try again": throttled or the service in the way.
fn retry_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// A parsed upstream base URL.
#[derive(Debug, Clone)]
pub(crate) struct Base {
    pub host: String,
    pub port: u16,
    /// Starts with `/` or is empty; no trailing slash.
    pub path: String,
}

impl Base {
    fn loc(&self, path_and_query: String) -> Loc {
        Loc {
            host: self.host.clone(),
            port: self.port,
            path_and_query,
        }
    }

    /// The text of the base as it appears in absolute links.
    fn link_text(&self) -> String {
        self.loc(self.path.clone()).url()
    }
}

/// `https://host[:port][/path]` with no userinfo, query or fragment.
pub(crate) fn parse_base(base: &str) -> Result<Base, &'static str> {
    let rest = base
        .strip_prefix("https://")
        .ok_or("an upstream base must start with https://")?;
    if rest
        .bytes()
        .any(|c| c <= b' ' || c == 0x7f || matches!(c, b'?' | b'#' | b'\\' | b'@' | b'%'))
    {
        return Err("an upstream base has a character it may not have");
    }
    let end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| "an upstream base has a bad port")?,
        ),
        None => (authority, 443),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        });
    if !host_ok || port == 0 {
        return Err("an upstream base has a bad host");
    }
    Ok(Base {
        host: host.to_ascii_lowercase(),
        port,
        path: path.trim_end_matches('/').to_string(),
    })
}

fn prefix_name(p: Prefix) -> &'static str {
    match p {
        Prefix::Pypi => "pypi",
        Prefix::Npm => "npm",
        Prefix::Maven => "maven",
        Prefix::Go => "go",
        Prefix::Cargo => "cargo",
    }
}

/// One configured upstream.
pub(crate) struct Up {
    prefix: Prefix,
    name: &'static str,
    base: Base,
    auth: Auth,
}

impl Up {
    /// The `Authorization` value, built per request so the token is not kept in a second long-lived copy. The text is wiped when
    /// dropped; the header value hyper holds while the request is in flight is freed with the request.
    fn authorization(&self) -> Option<HeaderValue> {
        let mut line: Zeroizing<String> = Zeroizing::new(String::new());
        match &self.auth {
            Auth::None => return None,
            Auth::Basic { user, secret } => {
                let raw = Zeroizing::new(format!("{user}:{}", secret.expose()));
                let enc = Zeroizing::new(
                    base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()),
                );
                line.push_str("Basic ");
                line.push_str(&enc);
            }
            Auth::Bearer(secret) => {
                line.push_str("Bearer ");
                line.push_str(secret.expose());
            }
            Auth::Raw(secret) => line.push_str(secret.expose()),
        }
        let mut v = HeaderValue::from_str(&line).ok()?;
        v.set_sensitive(true);
        Some(v)
    }

    fn credentialed(&self) -> bool {
        !matches!(self.auth, Auth::None)
    }
}

/// Everything a request needs, shared by all connections.
pub(crate) struct Shared {
    host_header: String,
    ups: Vec<Up>,
    rewriter: Rewriter,
    /// The rewrite for an npm CLI: the npm links go to [`PUBLIC_NPM_HOST`], everything else as in `rewriter`.
    npm_cli_rewriter: Rewriter,
    expires_at: Option<u64>,
    clock: Option<Arc<AtomicU64>>,
    request_timeout: Duration,
    idle_timeout: Duration,
    /// Longest a client may take to send a request head, whatever the idle time is.
    head_cap: Duration,
    max_redirects: u8,
    log: Arc<dyn Log>,
    pub stats: Arc<Stats>,
    client: UpstreamClient,
    /// Answers being read whole for a rewrite at the same time.
    rewrite_slots: Arc<Semaphore>,
}

impl Shared {
    fn now_unix(&self) -> u64 {
        match &self.clock {
            Some(c) => c.load(Ordering::SeqCst),
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    fn expired(&self) -> bool {
        self.expires_at.is_some_and(|t| self.now_unix() >= t)
    }
}

/// What `start` needs from a test.
pub(crate) struct TestInputs {
    pub net: TestNet,
    pub clock: Option<Arc<AtomicU64>>,
    /// Replaces [`HEADER_READ_CAP`], so a test need not wait 30 s to see the cap act.
    pub head_cap: Option<Duration>,
}

/// A running proxy.
pub(crate) struct Running {
    pub addr: SocketAddr,
    pub shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Running {
    pub fn signal_stop(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn stop(mut self) {
        self.signal_stop();
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

fn validate(cfg: &Config) -> io::Result<()> {
    match cfg.listen.ip() {
        std::net::IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST => {}
        _ => return Err(invalid("the proxy listens on 127.0.0.1 only")),
    }
    if cfg.max_connections == 0 {
        return Err(invalid("max_connections must be at least 1"));
    }
    if cfg.request_timeout.is_zero() || cfg.idle_timeout.is_zero() {
        return Err(invalid("timeouts must be more than zero"));
    }
    let mut seen = Vec::new();
    for u in &cfg.upstreams {
        if seen.contains(&u.prefix) {
            return Err(invalid("two upstreams share a prefix"));
        }
        seen.push(u.prefix);
        parse_base(&u.base).map_err(invalid)?;
    }
    Ok(())
}

/// Binds and serves. `test` is `None` in production.
pub(crate) async fn launch(cfg: Config, test: Option<TestInputs>) -> io::Result<Running> {
    validate(&cfg)?;
    let client = net::build_client(test.as_ref().map(|t| &t.net))?;
    let head_cap = test
        .as_ref()
        .and_then(|t| t.head_cap)
        .unwrap_or(HEADER_READ_CAP);
    let listener = TcpListener::bind(cfg.listen).await?;
    let addr = listener.local_addr()?;
    let local = format!("http://127.0.0.1:{}", addr.port());
    let mut ups = Vec::new();
    let mut links = Vec::new();
    let mut npm_cli_links = Vec::new();
    for u in cfg.upstreams {
        let base = parse_base(&u.base).map_err(invalid)?;
        let name = prefix_name(u.prefix);
        // Only the pypi and npm indexes carry absolute links to themselves. Maven and Go answers are checksummed by their clients
        // (.pom with .sha1, go.mod with go.sum), so their bytes are never touched.
        if matches!(u.prefix, Prefix::Pypi | Prefix::Npm) {
            links.push((base.link_text(), format!("{local}/{name}")));
            npm_cli_links.push((
                base.link_text(),
                if u.prefix == Prefix::Npm {
                    PUBLIC_NPM_HOST.to_string()
                } else {
                    format!("{local}/{name}")
                },
            ));
        }
        let up = Up {
            prefix: u.prefix,
            name,
            base,
            auth: u.auth,
        };
        if up.credentialed() && up.authorization().is_none() {
            return Err(invalid("a credential cannot be sent as a header"));
        }
        ups.push(up);
    }
    ups.sort_by_key(|u| u.prefix);
    let shared = Arc::new(Shared {
        host_header: format!("127.0.0.1:{}", addr.port()),
        ups,
        rewriter: Rewriter::new(&links),
        npm_cli_rewriter: Rewriter::new(&npm_cli_links),
        expires_at: cfg.expires_at_unix,
        clock: test.and_then(|t| t.clock),
        request_timeout: cfg.request_timeout,
        idle_timeout: cfg.idle_timeout,
        head_cap,
        max_redirects: cfg.max_redirects,
        log: cfg.log,
        stats: Arc::new(Stats::default()),
        client,
        rewrite_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_REWRITES)),
    });
    let (stop_tx, stop_rx) = watch::channel(false);
    let task = tokio::spawn(accept_loop(
        listener,
        shared.clone(),
        stop_rx,
        cfg.max_connections,
    ));
    Ok(Running {
        addr,
        shared,
        stop: stop_tx,
        task: Some(task),
    })
}

/// A connection from anything but loopback is closed.
pub(crate) fn peer_is_loopback(peer: SocketAddr) -> bool {
    peer.ip().to_canonical().is_loopback()
}

async fn accept_loop(
    listener: TcpListener,
    sh: Arc<Shared>,
    mut stop: watch::Receiver<bool>,
    max_connections: usize,
) {
    let slots = Arc::new(Semaphore::new(max_connections));
    let overflow = Arc::new(AtomicUsize::new(0));
    let mut conns: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(x) => x,
                    Err(_) => {
                        // Out of descriptors or a reset while accepting: back off briefly and carry on.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if !peer_is_loopback(peer) {
                    drop(stream);
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let sh = sh.clone();
                match slots.clone().try_acquire_owned() {
                    Ok(permit) => {
                        conns.spawn(async move {
                            serve(stream, sh, false).await;
                            drop(permit);
                        });
                    }
                    Err(_) => {
                        if overflow.fetch_add(1, Ordering::SeqCst) >= MAX_OVERFLOW {
                            overflow.fetch_sub(1, Ordering::SeqCst);
                            drop(stream);
                            continue;
                        }
                        let overflow = overflow.clone();
                        conns.spawn(async move {
                            serve(stream, sh, true).await;
                            overflow.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                }
                while conns.try_join_next().is_some() {}
            }
        }
    }
    drop(listener);
    conns.abort_all();
    while conns.join_next().await.is_some() {}
}

/// Passes bytes through and stamps the connection's activity.
struct ActivityIo {
    inner: TcpStream,
    act: Arc<Activity>,
}

impl AsyncRead for ActivityIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.act.touch();
        }
        r
    }
}

impl AsyncWrite for ActivityIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.act.touch();
        }
        r
    }

    // Vectored writes let hyper queue the `Bytes` of a body as they are instead of copying them into one buffer; without them a
    // rewritten answer would be copied and the rewrite slot (which rides on the original `Bytes`) would be released too soon.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.act.touch();
        }
        r
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Serves one client connection until it ends, goes idle for `idle_timeout` with nothing in flight, or the proxy stops.
async fn serve(stream: TcpStream, sh: Arc<Shared>, overflow: bool) {
    let act = Arc::new(Activity::new());
    let io = TokioIo::new(ActivityIo {
        inner: stream,
        act: act.clone(),
    });
    let (svc_sh, svc_act) = (sh.clone(), act.clone());
    let svc = service_fn(move |req: Request<Incoming>| {
        let (sh, act) = (svc_sh.clone(), svc_act.clone());
        async move {
            let life = Life::new(sh.stats.clone(), sh.log.clone(), act);
            let resp = if overflow {
                busy(life)
            } else {
                dispatch(&sh, life, req).await
            };
            Ok::<_, Infallible>(resp)
        }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(sh.idle_timeout.min(sh.head_cap))
        .max_buf_size(16 * 1024)
        .max_headers(64)
        .keep_alive(true);
    let conn = builder.serve_connection(io, svc);
    tokio::pin!(conn);
    loop {
        tokio::select! {
            _ = &mut conn => return,
            () = tokio::time::sleep_until(act.last() + conn_limit(&act, sh.idle_timeout)) => {
                if Instant::now() >= act.last() + conn_limit(&act, sh.idle_timeout) {
                    return;
                }
            }
        }
    }
}

/// How long a connection may go with no byte moving before it is closed: the idle time, and twice that with a request in flight.
/// The upstream of a request in flight is held to the idle time itself (and a redirect gets a fresh wait), so a connection silent
/// for twice that is a client that stopped reading or writing, and must not keep its slot for ever.
fn conn_limit(act: &Activity, idle: Duration) -> Duration {
    if act.busy() { idle * 2 } else { idle }
}

// ---------------------------------------------------------------------------------------------------------------------
// Answers the proxy makes itself

struct Refuse {
    status: u16,
    message: &'static str,
    close: bool,
    allow: bool,
}

fn refuse(
    mut life: Life,
    prefix: Option<&'static str>,
    r: Refuse,
    refused: bool,
) -> Response<OutBody> {
    life.record(prefix, r.status, refused);
    let mut resp = Response::new(OutBody::full(life, Bytes::from(format!("{}\n", r.message))));
    *resp.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let h = resp.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if r.allow {
        h.insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    }
    if r.close {
        h.insert(CONNECTION, HeaderValue::from_static("close"));
    }
    resp
}

fn bad(life: Life, message: &'static str) -> Response<OutBody> {
    refuse(
        life,
        None,
        Refuse {
            status: 400,
            message,
            close: false,
            allow: false,
        },
        true,
    )
}

fn busy(life: Life) -> Response<OutBody> {
    refuse(
        life,
        None,
        Refuse {
            status: 503,
            message: "too many connections",
            close: true,
            allow: false,
        },
        true,
    )
}

// ---------------------------------------------------------------------------------------------------------------------
// The request path

/// Request headers that go upstream; everything else, the client's `Authorization`, `Cookie` and `Proxy-*` first, is dropped.
const FORWARD_REQUEST: [HeaderName; 6] = [
    ACCEPT,
    USER_AGENT,
    IF_NONE_MATCH,
    IF_MODIFIED_SINCE,
    RANGE,
    IF_RANGE,
];
/// Request headers a client uses to ask for part of a body, or only for a changed one. The cargo registry's `config.json` is made from the
/// whole upstream answer, so none of them goes up for it.
const CONDITIONAL_REQUEST: [HeaderName; 4] = [IF_NONE_MATCH, IF_MODIFIED_SINCE, RANGE, IF_RANGE];
/// Request headers that ask for part of a body. Cargo never sends them, so they do not go up on `/cargo`.
const RANGE_REQUEST: [HeaderName; 2] = [RANGE, IF_RANGE];
/// The response headers of an index or crate answer of the cargo prefix: the type and the two validators cargo's sparse protocol reads. The
/// framing is the proxy's own. Nothing else the upstream sends (caching rules, a file name, ranges, cookies, a challenge) comes back.
const FORWARD_RESPONSE_CARGO: [HeaderName; 3] = [CONTENT_TYPE, ETAG, LAST_MODIFIED];
/// Response headers that come back.
const FORWARD_RESPONSE: [HeaderName; 7] = [
    CONTENT_TYPE,
    ETAG,
    LAST_MODIFIED,
    CACHE_CONTROL,
    CONTENT_RANGE,
    ACCEPT_RANGES,
    CONTENT_DISPOSITION,
];

enum FetchErr {
    RedirectRefused,
    TooManyRedirects,
    TooLarge,
    Encoded,
    /// The cargo registry's `config.json` is not a JSON object.
    BadConfig,
    /// The registry answered the cargo config with this 4xx or 5xx status (cargo keys its retries on the number; the body is the proxy's).
    ConfigAnswered(u16),
    /// The registry has no config for the cargo repository (a 404 is not "no such crate" for a config).
    ConfigMissing,
    /// The answer for the cargo config is a status or a representation the proxy does not take (2xx but 200, 3xx, a part).
    ConfigUnexpected,
    /// An index or crate answer of the cargo prefix that is a part (a 206 or a `Content-Range`); cargo asks for the whole.
    Unexpected,
    /// Every rewrite slot stayed taken for the idle time.
    Busy,
    Upstream(&'static str),
}

fn fetch_failure(life: Life, prefix: &'static str, e: &FetchErr) -> Response<OutBody> {
    let (status, message, refused) = match e {
        FetchErr::RedirectRefused => (502, "redirect refused".to_string(), true),
        FetchErr::TooManyRedirects => (502, "too many redirects".to_string(), true),
        FetchErr::TooLarge => (502, "response too large to rewrite".to_string(), true),
        FetchErr::Encoded => (502, "encoded text cannot be rewritten".to_string(), true),
        FetchErr::BadConfig => (
            502,
            "registry config is not a JSON object".to_string(),
            true,
        ),
        FetchErr::ConfigAnswered(n) => (
            *n,
            format!("package registry answered {n} for the registry config"),
            true,
        ),
        FetchErr::ConfigMissing => (
            502,
            "package registry has no cargo config".to_string(),
            true,
        ),
        FetchErr::ConfigUnexpected => (
            502,
            "unexpected answer for the registry config".to_string(),
            true,
        ),
        FetchErr::Unexpected => (
            502,
            "unexpected answer from the package registry".to_string(),
            true,
        ),
        FetchErr::Busy => (503, "proxy busy".to_string(), true),
        FetchErr::Upstream(class) => (502, format!("upstream error: {class}"), false),
    };
    let mut life = life;
    life.record(Some(prefix), status, refused);
    let mut resp = Response::new(OutBody::full(life, Bytes::from(format!("{message}\n"))));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

async fn dispatch(sh: &Shared, life: Life, req: Request<Incoming>) -> Response<OutBody> {
    let (parts, _body) = req.into_parts();
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return refuse(
            life,
            None,
            Refuse {
                status: 405,
                message: "method not allowed",
                close: false,
                allow: true,
            },
            true,
        );
    }
    if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
        return bad(life, "only origin-form request targets are accepted");
    }
    let mut hosts = parts.headers.get_all(HOST).iter();
    match (hosts.next(), hosts.next()) {
        (Some(h), None) if h.as_bytes() == sh.host_header.as_bytes() => {}
        _ => return bad(life, "bad Host header"),
    }
    let has_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .is_some_and(|v| v.as_bytes() != b"0");
    if has_length || parts.headers.contains_key(TRANSFER_ENCODING) {
        return refuse(
            life,
            None,
            Refuse {
                status: 400,
                message: "a request body is not accepted",
                close: true,
                allow: false,
            },
            true,
        );
    }
    let routed = match target::route(parts.uri.path(), parts.uri.query()) {
        Ok(r) => r,
        Err(Refusal(m)) => return bad(life, m),
    };
    let Some(up) = sh.ups.iter().find(|u| u.name == routed.prefix) else {
        return refuse(
            life,
            None,
            Refuse {
                status: 403,
                message: "prefix not allowed",
                close: false,
                allow: false,
            },
            true,
        );
    };
    if up.prefix == Prefix::Go && routed.query.is_some() {
        // The module protocol has no query; a bucket would answer `?list-type=2`, `?acl` and `?versionId=` on the same path.
        return refuse(
            life,
            Some(up.name),
            Refuse {
                status: 400,
                message: "no query is accepted on this prefix",
                close: false,
                allow: false,
            },
            true,
        );
    }
    if up.prefix == Prefix::Cargo {
        // Cargo asks for a registry's config, an index file or a crate and nothing else, without a query.
        if routed.query.is_some() {
            return refuse(
                life,
                Some(up.name),
                Refuse {
                    status: 400,
                    message: "no query is accepted on this prefix",
                    close: false,
                    allow: false,
                },
                true,
            );
        }
        if !cargo::path_ok(routed.rest) {
            return refuse(
                life,
                Some(up.name),
                Refuse {
                    status: 400,
                    message: "this path is not served on this prefix",
                    close: false,
                    allow: false,
                },
                true,
            );
        }
    }
    let cargo_config = up.prefix == Prefix::Cargo && routed.rest == cargo::CONFIG_PATH;
    if up.credentialed() && sh.expired() {
        return refuse(
            life,
            Some(up.name),
            Refuse {
                // Cargo reads a 401 for the registry config as "this registry needs a credential provider" and prints advice that no
                // package token can act on (measured with cargo 1.98.1); a 403 prints the body, which says what happened.
                status: if cargo_config { 403 } else { 401 },
                message: "package token expired",
                close: false,
                allow: false,
            },
            true,
        );
    }

    let started = Instant::now();
    let deadline = started + sh.request_timeout;
    let mut forward = HeaderMap::new();
    for name in FORWARD_REQUEST {
        if cargo_config && CONDITIONAL_REQUEST.contains(&name) {
            continue;
        }
        if up.prefix == Prefix::Cargo && RANGE_REQUEST.contains(&name) {
            continue;
        }
        for v in parts.headers.get_all(&name) {
            forward.append(name.clone(), v.clone());
        }
    }
    forward.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let mut path_and_query = format!("{}{}", up.base.path, routed.rest);
    if path_and_query.is_empty() {
        path_and_query.push('/');
    }
    if let Some(q) = routed.query {
        path_and_query.push('?');
        path_and_query.push_str(q);
    }
    match fetch(
        sh,
        up,
        &parts.method,
        up.base.loc(path_and_query),
        &forward,
        deadline,
    )
    .await
    {
        Ok(resp) => {
            let npm_cli = parts
                .headers
                .get(USER_AGENT)
                .is_some_and(|v| v.as_bytes().starts_with(b"npm/"));
            answer(
                sh,
                life,
                up,
                &parts.method,
                resp,
                deadline,
                Kind {
                    npm_cli,
                    cargo_config,
                },
            )
            .await
        }
        Err(e) => fetch_failure(life, up.name, &e),
    }
}

/// The upstream request for `loc`. The credential goes to the upstream's own host only, never to a redirect target, on every try.
fn upstream_request(
    up: &Up,
    origin: &Loc,
    loc: &Loc,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Request<Empty<Bytes>>, FetchErr> {
    let mut req = Request::builder()
        .method(method.clone())
        .uri(loc.url())
        .body(Empty::<Bytes>::new())
        .map_err(|_| FetchErr::RedirectRefused)?;
    *req.headers_mut() = headers.clone();
    if loc.host == origin.host
        && loc.port == origin.port
        && let Some(auth) = up.authorization()
    {
        req.headers_mut().insert(AUTHORIZATION, auth);
    }
    Ok(req)
}

/// Sends the request, following redirects by the rule of `redirect.rs`. Each hop is tried again, at most [`UPSTREAM_RETRIES`] times
/// and only while the request deadline leaves room for the wait, when the connection failed in a way that passes ([`retry_class`])
/// or the upstream answered a head that says "try again" ([`retry_status`]); when the tries run out the last answer goes through as
/// it is, and a failed connection is the same 502 as before.
async fn fetch(
    sh: &Shared,
    up: &Up,
    method: &Method,
    first: Loc,
    headers: &HeaderMap,
    deadline: Instant,
) -> Result<Response<Incoming>, FetchErr> {
    // `origin.path_and_query` is the configured base path: a redirect back to this host must stay under it.
    let origin = up.base.loc(up.base.path.clone());
    let mut loc = first;
    for hop in 0..=u32::from(sh.max_redirects) {
        let mut retries = 0u32;
        let resp = loop {
            let req = upstream_request(up, &origin, &loc, method, headers)?;
            let budget = sh
                .idle_timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            // What went wrong with this try, and the answer to hand on when there is no try left.
            let (why, class, kept): (String, &'static str, Option<Response<Incoming>>) =
                match tokio::time::timeout(budget, sh.client.request(req)).await {
                    Err(_) => return Err(FetchErr::Upstream("timeout")),
                    Ok(Err(e)) => {
                        let class = net::classify(&e);
                        if !retry_class(class) {
                            return Err(FetchErr::Upstream(class));
                        }
                        (class.to_string(), class, None)
                    }
                    Ok(Ok(r)) if retry_status(r.status().as_u16()) => {
                        (r.status().as_u16().to_string(), "", Some(r))
                    }
                    Ok(Ok(r)) => break r,
                };
            let pause = retry_pause(retries + 1);
            if retries >= UPSTREAM_RETRIES
                || deadline.saturating_duration_since(Instant::now()) <= pause
            {
                match kept {
                    Some(r) => break r,
                    None => return Err(FetchErr::Upstream(class)),
                }
            }
            retries += 1;
            sh.log
                .line(&format!("pkgproxy {} retry {retries} {why}", up.name));
            drop(kept);
            tokio::time::sleep(pause).await;
        };
        if redirect::is_redirect(resp.status().as_u16()) {
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
                .map(str::to_string);
            if let Some(location) = location {
                if hop == u32::from(sh.max_redirects) {
                    return Err(FetchErr::TooManyRedirects);
                }
                drop(resp);
                loc =
                    redirect::resolve(&origin, &loc, &location).ok_or(FetchErr::RedirectRefused)?;
                continue;
            }
        }
        return Ok(resp);
    }
    Err(FetchErr::TooManyRedirects)
}

fn content_length(h: &HeaderMap) -> Option<u64> {
    h.get(CONTENT_LENGTH)?.to_str().ok()?.trim().parse().ok()
}

/// Reads a body of at most `cap` bytes, each frame within the idle time and the whole within the deadline.
async fn collect_capped(
    mut body: Incoming,
    cap: usize,
    idle: Duration,
    deadline: Instant,
    life: &Life,
) -> Result<Vec<u8>, FetchErr> {
    let mut out = Vec::new();
    loop {
        let budget = idle.min(deadline.saturating_duration_since(Instant::now()));
        match tokio::time::timeout(budget, body.frame()).await {
            Err(_) => return Err(FetchErr::Upstream("timeout")),
            Ok(None) => return Ok(out),
            Ok(Some(Err(e))) => return Err(FetchErr::Upstream(net::classify(&e))),
            Ok(Some(Ok(frame))) => {
                // Data is arriving: the connection is not stalled even though nothing goes to the client until the end.
                life.progress();
                if let Ok(data) = frame.into_data() {
                    if out.len() + data.len() > cap {
                        return Err(FetchErr::TooLarge);
                    }
                    out.extend_from_slice(&data);
                }
            }
        }
    }
}

/// What the request asked for beyond the prefix.
#[derive(Clone, Copy)]
struct Kind {
    /// The client is the npm CLI (see [`PUBLIC_NPM_HOST`]).
    npm_cli: bool,
    /// The request is for the cargo registry's `config.json`, which the proxy rewrites.
    cargo_config: bool,
}

async fn answer(
    sh: &Shared,
    life: Life,
    up: &Up,
    method: &Method,
    resp: Response<Incoming>,
    deadline: Instant,
    kind: Kind,
) -> Response<OutBody> {
    let Kind {
        npm_cli,
        cargo_config,
    } = kind;
    let status = resp.status();
    if cargo_config && status == StatusCode::UNAUTHORIZED {
        // The registry refused the token on the one path where cargo would answer with advice about credential providers.
        return refuse(
            life,
            Some(up.name),
            Refuse {
                status: 403,
                message: "package registry refused the token",
                close: false,
                allow: false,
            },
            true,
        );
    }
    let (parts, body) = resp.into_parts();
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let textual =
        rewrite::is_textual(content_type) && matches!(up.prefix, Prefix::Pypi | Prefix::Npm);
    let partial =
        status == StatusCode::PARTIAL_CONTENT || parts.headers.contains_key(CONTENT_RANGE);
    let encoded = parts
        .headers
        .get(CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().eq_ignore_ascii_case("identity"));
    let head = *method == Method::HEAD;
    let bodyless = head
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || status.is_informational();

    let upstream_length = content_length(&parts.headers);
    // The cargo registry's config is not passed on: it is made new from the upstream's, and only from an unencoded, whole 200 that is not
    // too large. Every other answer for it is a fixed text of the proxy, for HEAD as for GET, and never the upstream's body or headers.
    if cargo_config {
        let refusal = match status.as_u16() {
            200 if partial => Some(FetchErr::ConfigUnexpected),
            200 if encoded => Some(FetchErr::Encoded),
            200 if upstream_length.is_some_and(|n| n > cargo::MAX_CONFIG_BYTES as u64) => {
                Some(FetchErr::TooLarge)
            }
            200 => None,
            404 => Some(FetchErr::ConfigMissing),
            n @ 400..=599 => Some(FetchErr::ConfigAnswered(n)),
            _ => Some(FetchErr::ConfigUnexpected),
        };
        if let Some(e) = refusal {
            return fetch_failure(life, up.name, &e);
        }
    } else if up.prefix == Prefix::Cargo && partial {
        return fetch_failure(life, up.name, &FetchErr::Unexpected);
    }

    let mut out = HeaderMap::new();
    if cargo_config {
        // The upstream's validators, caching rules and ranges describe bytes that are not sent: cargo asks again each run, and the
        // answer is a few hundred bytes.
        out.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    } else {
        let passed: &[HeaderName] = if up.prefix == Prefix::Cargo {
            &FORWARD_RESPONSE_CARGO
        } else {
            &FORWARD_RESPONSE
        };
        for name in passed {
            for v in parts.headers.get_all(name) {
                out.append(name.clone(), v.clone());
            }
        }
    }
    // The npm CLI and every other client are given different links in the same packument (see `PUBLIC_NPM_HOST`): the answer
    // depends on the request's `User-Agent`, whatever its status or method.
    if textual && up.prefix == Prefix::Npm {
        out.append(VARY, HeaderValue::from_static("User-Agent"));
    }

    let (payload, headers) = if cargo_config && !bodyless {
        // The config is parsed and written new under the same admission control as a rewrite of pypi or npm, so a burst of them cannot pile
        // up memory: the input is at most 64 KiB, and a parsed tree of it can be some times that.
        let _slot =
            match tokio::time::timeout(sh.idle_timeout, sh.rewrite_slots.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                _ => return fetch_failure(life, up.name, &FetchErr::Busy),
            };
        let bytes = match collect_capped(
            body,
            cargo::MAX_CONFIG_BYTES,
            sh.idle_timeout,
            deadline,
            &life,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => return fetch_failure(life, up.name, &e),
        };
        let local = format!("http://{}", sh.host_header);
        let Some(config) = cargo::rewrite_config(&bytes, &local) else {
            return fetch_failure(life, up.name, &FetchErr::BadConfig);
        };
        out.insert(CONTENT_LENGTH, HeaderValue::from(config.len()));
        (Payload::Full(Bytes::from(config)), out)
    } else if textual && !partial && !bodyless {
        if encoded {
            return fetch_failure(life, up.name, &FetchErr::Encoded);
        }
        if upstream_length.is_some_and(|n| n > rewrite::MAX_REWRITE_BYTES as u64) {
            return fetch_failure(life, up.name, &FetchErr::TooLarge);
        }
        // Textual answers are held whole (up to 64 MiB) while they are rewritten; only a few at a time, so 64 clients cannot
        // pile up gigabytes.
        let slot =
            match tokio::time::timeout(sh.idle_timeout, sh.rewrite_slots.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                _ => return fetch_failure(life, up.name, &FetchErr::Busy),
            };
        let bytes = match collect_capped(
            body,
            rewrite::MAX_REWRITE_BYTES,
            sh.idle_timeout,
            deadline,
            &life,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => return fetch_failure(life, up.name, &e),
        };
        let rewriter = if npm_cli && up.prefix == Prefix::Npm {
            &sh.npm_cli_rewriter
        } else {
            &sh.rewriter
        };
        let rewritten = rewriter.apply(&bytes);
        drop(bytes);
        out.insert(CONTENT_LENGTH, HeaderValue::from(rewritten.len()));
        // The rewritten text is not the upstream's bytes: a range of it would not line up with them.
        out.remove(ACCEPT_RANGES);
        // The slot goes with the bytes (not with the body object, which hyper drops as soon as it has read the one frame): it
        // stays taken until hyper has written them, or the connection is cut and they are freed.
        (Payload::Full(Bytes::from_owner(Held(rewritten, slot))), out)
    } else if bodyless {
        // The status and headers describe the body a GET would carry; there is none to send. A rewritten type has no honest length.
        if let Some(n) = upstream_length.filter(|_| !((textual || cargo_config) && !partial)) {
            out.insert(CONTENT_LENGTH, HeaderValue::from(n));
        }
        (Payload::Empty, out)
    } else {
        if let Some(n) = upstream_length {
            out.insert(CONTENT_LENGTH, HeaderValue::from(n));
        }
        if encoded {
            for v in parts.headers.get_all(CONTENT_ENCODING) {
                out.append(CONTENT_ENCODING, v.clone());
            }
        }
        (Payload::Stream(body, upstream_length), out)
    };

    let mut life = life;
    life.record(Some(up.name), status.as_u16(), false);
    let out_body = match payload {
        Payload::Full(b) => OutBody::full(life, b),
        Payload::Empty => OutBody::empty(life),
        Payload::Stream(b, len) => OutBody::stream(life, b, len, sh.idle_timeout, deadline),
    };
    let mut response = Response::new(out_body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// A rewritten answer and the slot that pays for its memory; the slot is released when the last `Bytes` over it is dropped.
struct Held(Vec<u8>, #[allow(dead_code)] OwnedSemaphorePermit);

impl AsRef<[u8]> for Held {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

enum Payload {
    Full(Bytes),
    Empty,
    Stream(Incoming, Option<u64>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_base_into_host_port_and_path_without_a_trailing_slash() {
        let b = parse_base("https://Dom-1.d.codeartifact.us-east-1.amazonaws.com/pypi/packages/")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (b.host.as_str(), b.port, b.path.as_str()),
            (
                "dom-1.d.codeartifact.us-east-1.amazonaws.com",
                443,
                "/pypi/packages"
            )
        );
        let b = parse_base("https://bucket.s3.us-east-1.amazonaws.com")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((b.port, b.path.as_str()), (443, ""));
        assert_eq!(b.link_text(), "https://bucket.s3.us-east-1.amazonaws.com");
        let b = parse_base("https://h.example:8443/x").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (b.port, b.link_text().as_str()),
            (8443, "https://h.example:8443/x")
        );
    }

    #[test]
    fn refuses_a_base_that_could_leak_or_misroute() {
        for bad in [
            "",
            "http://h.example/x",
            "ftp://h.example/x",
            "h.example/x",
            "https://",
            "https:///x",
            "https://user@h.example/x",
            "https://h.example/x?y=1",
            "https://h.example/x#f",
            "https://h.example:0/x",
            "https://h.example:99999/x",
            "https://h.example:/x",
            "https://h..example/x",
            "https://h_x.example/x",
            "https://h.example/a b",
            "https://h.example/a\\b",
            "https://h.example/a%2fb",
            "https://[::1]/x",
        ] {
            assert!(parse_base(bad).is_err(), "{bad:?} should be refused");
        }
    }

    struct Quiet;

    impl Log for Quiet {
        fn line(&self, _text: &str) {}
    }

    fn bare_config(idle: Duration) -> Config {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            upstreams: Vec::new(),
            expires_at_unix: None,
            max_connections: 4,
            request_timeout: Duration::from_secs(900),
            idle_timeout: idle,
            max_redirects: 5,
            log: Arc::new(Quiet),
        }
    }

    #[test]
    fn the_request_head_cap_is_the_documented_thirty_seconds() {
        assert_eq!(HEADER_READ_CAP, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn production_launch_applies_the_thirty_second_head_cap() {
        let run = launch(bare_config(Duration::from_secs(60)), None)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run.shared.head_cap, HEADER_READ_CAP);
        run.stop().await;
    }

    #[tokio::test]
    async fn a_request_head_that_never_finishes_is_cut_at_the_head_cap_not_at_the_idle_time() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Idle time 600 s; head cap 300 ms (the test seam): only the cap can end this connection within seconds.
        let inputs = TestInputs {
            net: TestNet::default(),
            clock: None,
            head_cap: Some(Duration::from_millis(300)),
        };
        let run = launch(bare_config(Duration::from_secs(600)), Some(inputs))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut c = TcpStream::connect(run.addr)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        c.write_all(b"GET /pypi/x HTT")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut buf = [0u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
            .await
            .expect("the connection is cut at the head cap")
            .unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(n == 0 || head.starts_with("HTTP/1.1 4"), "{head:?}");
        run.stop().await;
    }

    #[test]
    fn prefix_names_are_the_five_path_words() {
        assert_eq!(
            [
                Prefix::Pypi,
                Prefix::Npm,
                Prefix::Maven,
                Prefix::Go,
                Prefix::Cargo
            ]
            .map(prefix_name),
            ["pypi", "npm", "maven", "go", "cargo"]
        );
    }
}
