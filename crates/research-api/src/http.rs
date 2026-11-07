//! The HTTPS client of the Lambdas (NET lane): Tavily from `research-api`, GitHub archives from `research-job`.
//!
//! One pooled hyper client with verified TLS (the webpki roots, the ring provider, no switch to skip it) and three guards the callers rely
//! on:
//!
//! - **Addresses.** A name is resolved by the system resolver and every resolved address must be globally routable
//!   ([`globally_routable`]); one address that is not refuses the name, and the connection goes to the checked addresses. An IP literal in
//!   the URL is judged the same way before anything is sent. Loopback, private, link-local, CGNAT, multicast, documentation and similar
//!   ranges never get a connection from a Lambda that holds a key or reads the stage's S3 bucket.
//! - **Size.** A response is read only up to a limit; the body stops there (`too_large`), whether the server declared a length or streams.
//! - **Redirects.** The client never follows one; a 3xx comes back with its `location` so the caller decides (the GitHub source allows one
//!   hop to `codeload.github.com` and nothing else).
//!
//! Connections are not kept between requests: a Lambda environment is frozen between invocations and a pooled socket is dead when it thaws.
//! An error names a class and never a URL, a header value or a body.

use bytes::Bytes;
use http::header::{AUTHORIZATION, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

/// What every request carries unless the caller sets its own `User-Agent`.
pub const DEFAULT_USER_AGENT: &str = "hyperscale-agent-meta-harness-research/1";

/// Timeouts and the response cap of a client; [`HttpClient::send_limited`] overrides the last two for one call.
#[derive(Clone, Debug)]
pub struct HttpOptions {
    pub connect_timeout: Duration,
    /// The whole request: connect, send, headers and body.
    pub total_timeout: Duration,
    pub max_response_bytes: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(30),
            max_response_bytes: 8 * 1024 * 1024,
        }
    }
}

/// A request. `Debug` shows the method, the host, the header names and the body length: never a header value (the `Authorization` value
/// holds the key), the path or query, or a body byte.
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = self
            .url
            .parse::<Uri>()
            .ok()
            .and_then(|u| u.host().map(str::to_string))
            .unwrap_or_else(|| "<unparsed>".to_string());
        let names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("host", &host)
            .field("header_names", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// An answer. `Debug` shows the status, the header names and the body length, never a header value or a body byte.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Lower-case names, in the order received (a repeated header appears repeatedly).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("header_names", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl HttpResponse {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request failed. A class, never the URL, a header value or any of the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// A resolved address (or an IP literal) is not globally routable.
    Blocked,
    /// The response is longer than the limit.
    TooLarge,
    Timeout,
    Dns,
    Tls,
    Connect,
    Reset,
    Protocol,
    /// The request itself is unusable (scheme, URL, header); the reason is a fixed phrase.
    InvalidRequest(&'static str),
}

impl HttpError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Tls => "tls",
            Self::Connect => "connect",
            Self::Reset => "reset",
            Self::Protocol => "protocol",
            Self::InvalidRequest(_) => "invalid_request",
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(why) => write!(f, "http invalid_request: {why}"),
            other => write!(f, "http {}", other.class()),
        }
    }
}

impl std::error::Error for HttpError {}

// ---- addresses -------------------------------------------------------------------------------------------------------------------------

/// Whether a connection to `ip` is allowed: only globally routable unicast addresses. An IPv4-mapped IPv6 address is judged by the
/// IPv4 address inside it.
pub fn globally_routable(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4_global(v4),
        IpAddr::V6(v6) => v6_global(v6),
    }
}

fn v4_global(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || (a == 100 && (64..=127).contains(&b))
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn v6_global(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Global unicast is 2000::/3; carve out the ranges inside it that are not routable.
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        && !(s[0] == 0x2001 && s[1] < 0x0200)
        && s[0] != 0x2002
        && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
}

// ---- name resolution ---------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Resolver {
    /// `Some` only in the tests: a fixed table, no checks.
    table: Option<Arc<BTreeMap<String, SocketAddr>>>,
}

type ResolveFuture =
    Pin<Box<dyn Future<Output = io::Result<std::vec::IntoIter<SocketAddr>>> + Send>>;

impl tower_service::Service<Name> for Resolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = ResolveFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let host = name.as_str().to_ascii_lowercase();
        let table = self.table.clone();
        Box::pin(async move {
            if let Some(table) = table {
                return match table.get(&host) {
                    Some(addr) => Ok(vec![*addr].into_iter()),
                    None => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "dns resolution failed",
                    )),
                };
            }
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "dns resolution failed"))?
                .collect();
            if found.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "dns resolution failed",
                ));
            }
            // Every answer is checked: one address that is not routable refuses the name, whatever else it resolves to.
            if found.iter().any(|a| !globally_routable(a.ip())) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "address not allowed",
                ));
            }
            Ok(found.into_iter())
        })
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

type Upstream = Client<HttpsConnector<HttpConnector<Resolver>>, Full<Bytes>>;

/// A short class for a failed request, read from the error chain's text. Nothing from the chain is kept.
fn classify(err: &(dyn std::error::Error + 'static)) -> HttpError {
    let mut text = String::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        text.push_str(&e.to_string().to_ascii_lowercase());
        text.push('|');
        cur = e.source();
    }
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&["address not allowed"]) {
        HttpError::Blocked
    } else if has(&["dns resolution failed", "dns error"]) {
        HttpError::Dns
    } else if has(&[
        "close_notify",
        "peer closed",
        "unexpected eof",
        "broken pipe",
        "connection reset",
        "connection closed",
    ]) {
        HttpError::Reset
    } else if has(&[
        "certificate",
        "unknownissuer",
        "invalid peer",
        "handshake",
        "tls",
        "alert",
    ]) {
        HttpError::Tls
    } else if has(&["timed out", "timeout", "deadline"]) {
        HttpError::Timeout
    } else if has(&["reset", "eof", "closed before", "incomplete message"]) {
        HttpError::Reset
    } else if has(&["connect", "refused", "unreachable"]) {
        HttpError::Connect
    } else {
        HttpError::Protocol
    }
}

// ---- the client --------------------------------------------------------------------------------------------------------------------------

/// The client. Cloning shares the connector.
#[derive(Clone)]
pub struct HttpClient {
    upstream: Upstream,
    options: HttpOptions,
    /// Plain `http://` is accepted only by the test seam.
    allow_plain: bool,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// The production client: system resolver with the address check, verified TLS, `https` only.
    pub fn new(options: HttpOptions) -> Result<Self, HttpError> {
        Self::build(options, None)
    }

    fn build(
        options: HttpOptions,
        test: Option<BTreeMap<String, SocketAddr>>,
    ) -> Result<Self, HttpError> {
        let allow_plain = test.is_some();
        let resolver = Resolver {
            table: test.map(Arc::new),
        };
        let mut connector = HttpConnector::new_with_resolver(resolver);
        connector.enforce_http(false);
        connector.set_connect_timeout(Some(options.connect_timeout));
        connector.set_nodelay(true);
        let schemes = HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider())
            .map_err(|_| HttpError::Tls)?;
        let https = if allow_plain {
            schemes
                .https_or_http()
                .enable_http1()
                .wrap_connector(connector)
        } else {
            schemes
                .https_only()
                .enable_http1()
                .wrap_connector(connector)
        };
        // No idle connections are kept: a frozen Lambda environment thaws with dead sockets.
        let upstream = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_max_idle_per_host(0)
            .build(https);
        Ok(Self {
            upstream,
            options,
            allow_plain,
        })
    }

    /// The client's timeouts and response cap.
    pub fn options(&self) -> &HttpOptions {
        &self.options
    }

    /// Sends with the client's own limit and timeout.
    pub async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_limited(
            req,
            self.options.max_response_bytes,
            self.options.total_timeout,
        )
        .await
    }

    /// Sends with the limit and the total timeout given for this call.
    pub async fn send_limited(
        &self,
        req: HttpRequest,
        max_response_bytes: usize,
        total_timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let request = self.prepare(req)?;
        match tokio::time::timeout(total_timeout, self.run(request, max_response_bytes)).await {
            Ok(result) => result,
            Err(_) => Err(HttpError::Timeout),
        }
    }

    fn prepare(&self, req: HttpRequest) -> Result<Request<Full<Bytes>>, HttpError> {
        let mut uri: Uri = req
            .url
            .parse()
            .map_err(|_| HttpError::InvalidRequest("the URL is malformed"))?;
        match uri.scheme_str() {
            Some("https") => {}
            Some("http") if self.allow_plain => {}
            _ => return Err(HttpError::InvalidRequest("only https URLs are allowed")),
        }
        if self.allow_plain {
            // The test seam talks plain TCP to its fakes: the production URL (https, the real host name) is kept for the caller and the
            // `Host` header, and only the transport changes.
            let mut parts = uri.into_parts();
            parts.scheme = Some(http::uri::Scheme::HTTP);
            uri = Uri::from_parts(parts)
                .map_err(|_| HttpError::InvalidRequest("the URL is malformed"))?;
        }
        let host = uri
            .host()
            .filter(|h| !h.is_empty())
            .ok_or(HttpError::InvalidRequest("the URL has no host"))?;
        // An IP literal never reaches the resolver, so it is judged here.
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>()
            && !globally_routable(ip)
        {
            return Err(HttpError::Blocked);
        }
        let mut builder = Request::builder().method(req.method).uri(uri);
        let mut headers = HeaderMap::new();
        for (name, value) in &req.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| HttpError::InvalidRequest("a header name is not valid"))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| HttpError::InvalidRequest("a header value is not valid"))?;
            if name == AUTHORIZATION {
                value.set_sensitive(true);
            }
            headers.append(name, value);
        }
        if !headers.contains_key(USER_AGENT) {
            headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        }
        if let Some(h) = builder.headers_mut() {
            *h = headers;
        }
        builder
            .body(Full::new(Bytes::from(req.body)))
            .map_err(|_| HttpError::InvalidRequest("the request cannot be built"))
    }

    async fn run(
        &self,
        request: Request<Full<Bytes>>,
        max_response_bytes: usize,
    ) -> Result<HttpResponse, HttpError> {
        let response = self
            .upstream
            .request(request)
            .await
            .map_err(|e| classify(&e))?;
        let (parts, mut body) = response.into_parts();
        let declared = parts
            .headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > max_response_bytes as u64) {
            return Err(HttpError::TooLarge);
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| classify(&e))?;
            if let Some(data) = frame.data_ref() {
                if buf.len() + data.len() > max_response_bytes {
                    return Err(HttpError::TooLarge);
                }
                buf.extend_from_slice(data);
            }
        }
        let headers = parts
            .headers
            .iter()
            .filter_map(|(n, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (n.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        Ok(HttpResponse {
            status: parts.status.as_u16(),
            headers,
            body: buf,
        })
    }
}

/// Test seams only: a fixed host-name-to-address table instead of the resolver (no address check) and plain `http://` upstreams on
/// loopback. No production code enables the `testing` feature.
#[cfg(feature = "testing")]
pub mod testing {
    use super::{HttpClient, HttpError, HttpOptions};
    use std::collections::BTreeMap;
    use std::net::SocketAddr;

    /// Lower-case host names and the address that answers for each. A URL without a port connects to the mapped port.
    #[derive(Clone, Debug, Default)]
    pub struct TestNet {
        pub hosts: BTreeMap<String, SocketAddr>,
    }

    impl HttpClient {
        pub fn with_test_net(options: HttpOptions, net: TestNet) -> Result<Self, HttpError> {
            Self::build(options, Some(net.hosts))
        }
    }
}
