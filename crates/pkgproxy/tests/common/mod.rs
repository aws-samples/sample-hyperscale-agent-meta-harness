//! Shared test rig for the pkgproxy integration tests: a per-run certificate authority (made with the `openssl`
//! command in a scratch directory, never committed), fake HTTPS upstreams (a CodeArtifact endpoint, an S3 file host and a
//! Go module bucket), a raw HTTP client that can send malformed requests, and the proxy under test started through the
//! `testing` seams (a fake DNS table, the test authority as the only trusted root, a fake clock).
#![allow(dead_code, clippy::unwrap_used)]

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use pkgproxy::testing::{TestNet, start_with};
use pkgproxy::{Config, Handle, Log, Secret, codeartifact_upstreams, go_upstream};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::Sleep;
use tokio_rustls::TlsAcceptor;

pub const CA_HOST: &str = "pkgproxy-test.d.codeartifact.us-east-1.amazonaws.com";
pub const S3_HOST: &str = "pkgproxy-files.s3.us-east-1.amazonaws.com";
pub const GO_HOST: &str = "hamh-direct-gomod-000000000000.s3.us-east-1.amazonaws.com";
pub const REPO: &str = "packages";

/// A package token stand-in, built at run time so no token-shaped literal sits in the source.
pub fn token() -> String {
    ["tok", "test", "0123456789", "not", "real"].join("-")
}

/// `base64("aws:" + token)`, the Basic credential CodeArtifact's pypi and maven endpoints take.
pub fn basic_for(token: &str) -> String {
    format!("Basic {}", b64(format!("aws:{token}").as_bytes()))
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------------------------------------------------
// Certificates

/// A throwaway certificate authority and one server certificate valid for `names`, made by the `openssl` command.
pub struct Pki {
    pub ca_der: Vec<u8>,
    pub server: Arc<rustls::ServerConfig>,
}

fn openssl(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the `openssl` command is needed by the pkgproxy tests");
    assert!(
        out.status.success(),
        "openssl {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl Pki {
    /// Uses only options that OpenSSL 1.0.2 and a current OpenSSL both take (no `-addext`, no
    /// `-pkeyopt`): an EC key from `ecparam`, configuration files for the extensions, and `-sha256` (webpki refuses SHA-1).
    pub fn new(names: &[&str]) -> Pki {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(
            d.join("ca.cnf"),
            "[req]\ndistinguished_name=dn\nprompt=no\n[dn]\nCN=pkgproxy-test-ca\n[v3_ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n",
        )
        .unwrap();
        std::fs::write(
            d.join("leaf.cnf"),
            "[req]\ndistinguished_name=dn\nprompt=no\n[dn]\nCN=pkgproxy-test-leaf\n",
        )
        .unwrap();
        openssl(
            d,
            &[
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
                "ca.key",
            ],
        );
        openssl(
            d,
            &[
                "req",
                "-x509",
                "-new",
                "-key",
                "ca.key",
                "-sha256",
                "-days",
                "2",
                "-config",
                "ca.cnf",
                "-extensions",
                "v3_ca",
                "-out",
                "ca.pem",
            ],
        );
        openssl(
            d,
            &[
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
                "leaf.sec1",
            ],
        );
        openssl(
            d,
            &[
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                "leaf.sec1",
                "-out",
                "leaf.key",
            ],
        );
        openssl(
            d,
            &[
                "req", "-new", "-key", "leaf.key", "-sha256", "-config", "leaf.cnf", "-out",
                "leaf.csr",
            ],
        );
        let san = names
            .iter()
            .map(|n| format!("DNS:{n}"))
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            d.join("ext.cnf"),
            format!("[ext]\nsubjectAltName={san}\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature\n"),
        )
        .unwrap();
        openssl(
            d,
            &[
                "x509",
                "-req",
                "-in",
                "leaf.csr",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-sha256",
                "-out",
                "leaf.pem",
                "-days",
                "2",
                "-extfile",
                "ext.cnf",
                "-extensions",
                "ext",
            ],
        );
        let ca = CertificateDer::from_pem_file(d.join("ca.pem")).unwrap();
        let leaf = CertificateDer::from_pem_file(d.join("leaf.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_file(d.join("leaf.key")).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf], key)
            .unwrap();
        Pki {
            ca_der: ca.as_ref().to_vec(),
            server: Arc::new(server),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Fake HTTPS upstream

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// What the fake sends as the body.
#[derive(Clone)]
pub enum Plan {
    Fixed(Bytes),
    /// `total` bytes of 'x' in 64 KiB chunks, generated on the fly (nothing held in memory).
    Gen {
        total: u64,
    },
    /// `first`, then the stream never ends (the upstream stalls).
    ThenStall(Bytes),
    /// `first`, then the connection breaks (a reset in the middle of the body).
    ThenError(Bytes),
    /// `body`, then these trailer fields (needs a chunked response that names them in `Trailer`).
    WithTrailers {
        body: Bytes,
        trailers: Vec<(String, String)>,
    },
    /// `count` chunks of one byte, `gap` apart.
    Trickle {
        gap: Duration,
        count: u32,
    },
}

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub plan: Plan,
    /// Content-Length is sent from the plan's size unless this is set.
    pub chunked: bool,
    /// Wait this long before the response head.
    pub delay: Duration,
    /// Close the connection without answering.
    pub abort: bool,
}

impl Reply {
    pub fn ok(body: impl Into<Bytes>) -> Reply {
        Reply {
            status: 200,
            headers: vec![],
            plan: Plan::Fixed(body.into()),
            chunked: false,
            delay: Duration::ZERO,
            abort: false,
        }
    }

    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            ..Reply::ok(Bytes::new())
        }
    }

    pub fn redirect(status: u16, location: &str) -> Reply {
        Reply::status(status).header("Location", location)
    }

    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn plan(mut self, plan: Plan) -> Reply {
        self.plan = plan;
        self
    }

    pub fn chunked(mut self) -> Reply {
        self.chunked = true;
        self
    }

    pub fn delayed(mut self, d: Duration) -> Reply {
        self.delay = d;
        self
    }

    pub fn aborted() -> Reply {
        Reply {
            abort: true,
            ..Reply::status(200)
        }
    }
}

pub type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

pub fn handler(f: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Handler {
    Arc::new(f)
}

pub fn always(r: Reply) -> Handler {
    handler(move |_| r.clone())
}

pub struct GenBody {
    plan: Plan,
    sent: u64,
    sleep: Option<Pin<Box<Sleep>>>,
    fired: u32,
    /// Hide the exact size so the server sends the body chunked.
    hide_size: bool,
}

static CHUNK: [u8; 65536] = [b'x'; 65536];

impl GenBody {
    fn new(plan: Plan, hide_size: bool) -> Self {
        Self {
            plan,
            sent: 0,
            sleep: None,
            fired: 0,
            hide_size,
        }
    }
}

impl Body for GenBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        match &this.plan {
            Plan::Fixed(b) => {
                if this.sent == 0 && !b.is_empty() {
                    this.sent = 1;
                    Poll::Ready(Some(Ok(Frame::data(b.clone()))))
                } else {
                    Poll::Ready(None)
                }
            }
            Plan::Gen { total } => {
                if this.sent >= *total {
                    return Poll::Ready(None);
                }
                let n = (*total - this.sent).min(CHUNK.len() as u64) as usize;
                this.sent += n as u64;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(&CHUNK[..n])))))
            }
            Plan::ThenStall(b) => {
                if this.sent == 0 {
                    this.sent = 1;
                    Poll::Ready(Some(Ok(Frame::data(b.clone()))))
                } else {
                    Poll::Pending
                }
            }
            Plan::ThenError(b) => {
                if this.sent == 0 {
                    this.sent = 1;
                    Poll::Ready(Some(Ok(Frame::data(b.clone()))))
                } else {
                    // Let the head and the first part go out before the break, so the client sees a reset mid-body.
                    let sleep = this.sleep.get_or_insert_with(|| {
                        Box::pin(tokio::time::sleep(Duration::from_millis(100)))
                    });
                    match sleep.as_mut().poll(cx) {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(()) => {
                            Poll::Ready(Some(Err(std::io::Error::other("fake upstream reset"))))
                        }
                    }
                }
            }
            Plan::WithTrailers { body, trailers } => match this.sent {
                0 => {
                    this.sent = 1;
                    Poll::Ready(Some(Ok(Frame::data(body.clone()))))
                }
                1 => {
                    this.sent = 2;
                    let mut map = http::HeaderMap::new();
                    for (k, v) in trailers {
                        if let (Ok(k), Ok(v)) = (
                            http::header::HeaderName::from_bytes(k.as_bytes()),
                            http::header::HeaderValue::from_str(v),
                        ) {
                            map.append(k, v);
                        }
                    }
                    Poll::Ready(Some(Ok(Frame::trailers(map))))
                }
                _ => Poll::Ready(None),
            },
            Plan::Trickle { gap, count } => {
                if this.fired >= *count {
                    return Poll::Ready(None);
                }
                let sleep = this
                    .sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(*gap)));
                match sleep.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(()) => {
                        this.sleep = None;
                        this.fired += 1;
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"t")))))
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        if self.hide_size {
            return SizeHint::default();
        }
        match &self.plan {
            Plan::Fixed(b) => SizeHint::with_exact(b.len() as u64),
            Plan::Gen { total } => SizeHint::with_exact(*total),
            Plan::Trickle { count, .. } => SizeHint::with_exact(*count as u64),
            _ => SizeHint::default(),
        }
    }
}

pub struct Fake {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    pub async fn start(pki: &Pki, handler: Handler) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(pki.server.clone());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let accepts = Arc::new(AtomicUsize::new(0));
        let (seen2, accepts2) = (seen.clone(), accepts.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                accepts2.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                let handler = handler.clone();
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let handler = handler.clone();
                        let seen = seen.clone();
                        async move {
                            let s = Seen {
                                method: req.method().to_string(),
                                target: req
                                    .uri()
                                    .path_and_query()
                                    .map(|p| p.as_str().to_string())
                                    .unwrap_or_default(),
                                headers: req
                                    .headers()
                                    .iter()
                                    .map(|(k, v)| {
                                        (
                                            k.to_string(),
                                            String::from_utf8_lossy(v.as_bytes()).to_string(),
                                        )
                                    })
                                    .collect(),
                            };
                            seen.lock().unwrap().push(s.clone());
                            let r = handler(&s);
                            if r.abort {
                                return Err(std::io::Error::other("fake upstream abort"));
                            }
                            if !r.delay.is_zero() {
                                tokio::time::sleep(r.delay).await;
                            }
                            let body = GenBody::new(r.plan.clone(), r.chunked);
                            let mut b = Response::builder().status(r.status);
                            for (k, v) in &r.headers {
                                b = b.header(k, v);
                            }
                            if !r.chunked
                                && !r
                                    .headers
                                    .iter()
                                    .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                && let Some(n) = body.size_hint().exact()
                                && req.method() != hyper::Method::HEAD
                            {
                                b = b.header("Content-Length", n);
                            }
                            Ok::<_, std::io::Error>(b.body(body).unwrap())
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), svc)
                        .await;
                });
            }
        });
        Fake {
            addr,
            seen,
            accepts,
            task,
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// TCP connections accepted so far (connection reuse shows as fewer connections than requests).
    pub fn connections(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The rig

#[derive(Default)]
pub struct MemLog(Mutex<Vec<String>>);

impl Log for MemLog {
    fn line(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_string());
    }
}

impl MemLog {
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

pub struct Opts {
    pub ca: Handler,
    pub s3: Handler,
    pub go: Handler,
    pub max_connections: usize,
    pub request_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_redirects: u8,
    pub expires_at_unix: Option<u64>,
    pub clock_now: u64,
    /// The package token; `token()` when `None`.
    pub token: Option<String>,
}

impl Opts {
    pub fn new(ca: Handler, s3: Handler, go: Handler) -> Opts {
        Opts {
            ca,
            s3,
            go,
            max_connections: 64,
            request_timeout: Duration::from_secs(900),
            idle_timeout: Duration::from_secs(60),
            max_redirects: 5,
            expires_at_unix: None,
            clock_now: 1_000_000,
            token: None,
        }
    }
}

pub struct Rig {
    pub proxy: Handle,
    pub ca: Fake,
    pub s3: Fake,
    pub go: Fake,
    pub log: Arc<MemLog>,
    pub clock: Arc<AtomicU64>,
    pub token: String,
}

impl Rig {
    pub fn addr(&self) -> SocketAddr {
        self.proxy.addr()
    }

    pub fn upstream_requests(&self) -> usize {
        self.ca.count() + self.s3.count() + self.go.count()
    }

    pub async fn get(&self, path: &str) -> Resp {
        get(self.addr(), path).await
    }
}

pub fn nothing() -> Handler {
    always(Reply::status(404))
}

pub async fn rig(ca: Handler, s3: Handler, go: Handler) -> Rig {
    rig_with(Opts::new(ca, s3, go)).await
}

pub async fn rig_with(o: Opts) -> Rig {
    let pki = Pki::new(&[CA_HOST, S3_HOST, GO_HOST]);
    let ca = Fake::start(&pki, o.ca).await;
    let s3 = Fake::start(&pki, o.s3).await;
    let go = Fake::start(&pki, o.go).await;
    let log = Arc::new(MemLog::default());
    let clock = Arc::new(AtomicU64::new(o.clock_now));
    let tok = o.token.clone().unwrap_or_else(token);
    let mut upstreams = codeartifact_upstreams(CA_HOST, REPO, Secret::new(tok.clone()));
    upstreams.push(go_upstream(&format!("https://{GO_HOST}")));
    let cfg = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstreams,
        expires_at_unix: o.expires_at_unix,
        max_connections: o.max_connections,
        request_timeout: o.request_timeout,
        idle_timeout: o.idle_timeout,
        max_redirects: o.max_redirects,
        log: log.clone(),
    };
    let net = TestNet {
        roots_der: vec![pki.ca_der.clone()],
        dns: BTreeMap::from([
            (CA_HOST.to_string(), ca.addr),
            (S3_HOST.to_string(), s3.addr),
            (GO_HOST.to_string(), go.addr),
        ]),
        clock: Some(clock.clone()),
    };
    let proxy = start_with(cfg, net).await.unwrap();
    Rig {
        proxy,
        ca,
        s3,
        go,
        log,
        clock,
        token: tok,
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Raw HTTP client

#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The whole declared body arrived (Content-Length reached, last chunk seen, or read to EOF).
    pub complete: bool,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every value of the header `name`, in order.
    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// A well-formed request with `Host` set to the proxy and `Connection: close`.
pub fn request(method: &str, path: &str, addr: SocketAddr, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut s = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for (k, v) in extra {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s.into_bytes()
}

pub async fn get(addr: SocketAddr, path: &str) -> Resp {
    send_raw(addr, &request("GET", path, addr, &[]), false).await
}

pub async fn get_with(addr: SocketAddr, path: &str, extra: &[(&str, &str)]) -> Resp {
    send_raw(addr, &request("GET", path, addr, extra), false).await
}

pub async fn head(addr: SocketAddr, path: &str) -> Resp {
    send_raw(addr, &request("HEAD", path, addr, &[]), true).await
}

/// Sends `raw` on a fresh connection and reads one response. Gives up after 20 s.
pub async fn send_raw(addr: SocketAddr, raw: &[u8], head_only: bool) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(raw).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), read_response(&mut s, head_only))
        .await
        .expect("the proxy answered within 20 s")
}

pub async fn read_response(s: &mut TcpStream, head_only: bool) -> Resp {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 65536];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return Resp {
                status: 0,
                headers: vec![],
                body: buf,
                complete: false,
            };
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k.trim().to_string(), v.trim().to_string())
        })
        .collect();
    let find_h = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.clone())
    };
    let mut body: Vec<u8> = buf[head_end..].to_vec();
    let no_body = head_only || status == 204 || status == 304 || (100..200).contains(&status);
    let complete;
    if no_body {
        complete = true;
        body.clear();
    } else if let Some(len) = find_h("content-length").and_then(|v| v.parse::<usize>().ok()) {
        while body.len() < len {
            let n = s.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        complete = body.len() >= len;
        body.truncate(len);
    } else if find_h("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        let (decoded, done) = read_chunked(s, body).await;
        body = decoded;
        complete = done;
    } else {
        loop {
            let n = s.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        complete = true;
    }
    Resp {
        status,
        headers,
        body,
        complete,
    }
}

async fn read_chunked(s: &mut TcpStream, mut raw: Vec<u8>) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut tmp = vec![0u8; 65536];
    loop {
        let Some(line_end) = find(&raw, b"\r\n") else {
            let n = s.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                return (out, false);
            }
            raw.extend_from_slice(&tmp[..n]);
            continue;
        };
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&raw[..line_end])
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )
        .unwrap_or(0);
        if size == 0 {
            return (out, true);
        }
        let need = line_end + 2 + size + 2;
        while raw.len() < need {
            let n = s.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                return (out, false);
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&raw[line_end + 2..line_end + 2 + size]);
        raw.drain(..need);
    }
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn contains_str(hay: &[u8], needle: &str) -> bool {
    find(hay, needle.as_bytes()).is_some()
}

/// Resident set size of this process in KiB.
pub fn vm_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap()
}

pub fn infallible<T>(r: Result<T, Infallible>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => match e {},
    }
}
