//! SigV4-signed HTTP over hyper with rustls and the webpki root store (the same TLS stack as `ham`,
//! S11). Used for `/internal/*` (service `execute-api`) and S3 objects (service `s3`).

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use bytes::Bytes;
use harness_protocol::api::internal::ProcessCredentials;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A file that is streamed is read in pieces of this size, off the runtime's threads ([`FileBody`]).
pub const FILE_PIECE_BYTES: usize = 1024 * 1024;
/// The largest frame of a streamed body. The bytes that left for the connection are counted frame by frame, so a transfer
/// that moves this many bytes within the no-progress limit is seen to move ([`Limits::no_progress`]).
pub const FILE_FRAME_BYTES: usize = 64 * 1024;
/// How many pieces of a streamed file wait between the thread that reads them and the connection.
const FILE_PIECES_AHEAD: usize = 4;
/// The body of a download has at least this long, and one second more for each [`BODY_BYTES_PER_S`] bytes of it, when the
/// whole-time limit of its request is shorter ([`body_time`]). Before the body of a download had no limit at all, and the
/// restore of a session's repository is one download without a second try, so a body that still moves must not be cut where it
/// came through before. What ends a download that hangs is the no-progress limit ([`Limits::no_progress`]); this limit ends only a
/// body that moves a little now and then and would never end.
pub const BODY_TIME: Duration = Duration::from_secs(30);
/// The slowest download a body is given time for: 64 KiB a second on average, far below what a working path gives. A
/// repository of 2 GiB has a little over nine hours. (At 1 MiB a second, the time the store of a copy has, a restore of 2 GiB over a
/// path of half a megabyte a second was cut after 35 minutes, where it took 68 and came through before the body had a limit.)
pub const BODY_BYTES_PER_S: u64 = 64 * 1024;

/// How long the body of a download of `bytes` bytes has, whatever the whole-time limit of its request: [`BODY_TIME`] and one
/// second more for each [`BODY_BYTES_PER_S`] bytes.
pub fn body_time(bytes: u64) -> Duration {
    BODY_TIME + Duration::from_secs(bytes / BODY_BYTES_PER_S)
}

/// The two limits of one streamed request ([`SignedHttp::put_file`], [`SignedHttp::get_to_file`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The whole request, from its start to the end of the answer; for a download that is the last byte of its body (a body whose
    /// size the answer names has [`body_time`] when that is longer).
    pub total: Duration,
    /// The request is given up when nothing of it moved for this long: no byte of the body of an upload left for the connection, the
    /// head of its answer did not come after that body had ended, or no byte of the body of a download arrived once the head of its
    /// answer had come (that head has `total`: nothing of a download moves before it).
    pub no_progress: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("bad URL {0}")]
    BadUrl(String),
    #[error("SigV4 signing failed: {0}")]
    Sign(String),
    #[error("transport: {0}")]
    Transport(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// `x-amzn-errortype` of an AWS error answer.
    pub error_type: Option<String>,
    /// Every header of the answer, names in lower case (the answer to a HEAD carries its facts here).
    pub headers: Vec<(String, String)>,
}

impl HttpResponse {
    /// The value of the response header `name` (lower case), if the answer has it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct SignedRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub creds: &'a ProcessCredentials,
    pub content_type: Option<&'a str>,
    pub body: Vec<u8>,
}

/// The headers SigV4 adds (`authorization`, `x-amz-date`, `x-amz-security-token`, and for S3
/// `x-amz-content-sha256`). The host header is signed from the URL authority.
pub fn signed_headers(
    method: &str,
    url: &str,
    service: &str,
    region: &str,
    creds: &ProcessCredentials,
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>, HttpError> {
    signed_headers_with(method, url, service, region, creds, &[], body, time)
}

/// [`signed_headers`] with `extra` request headers put into the signature (S3 needs SigV4 for a conditional write such as
/// `if-none-match`, and refuses an `x-amz-*` header that the signature does not cover). The result is still only the headers SigV4
/// adds: the caller puts `extra` on the request itself, once.
#[allow(clippy::too_many_arguments)]
pub fn signed_headers_with(
    method: &str,
    url: &str,
    service: &str,
    region: &str,
    creds: &ProcessCredentials,
    extra: &[(&str, &str)],
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>, HttpError> {
    signed_headers_of(
        method,
        url,
        service,
        region,
        creds,
        extra,
        SignableBody::Bytes(body),
        time,
    )
}

/// [`signed_headers_with`] for a body whose SHA-256 the caller has already (`sha256_hex`, lower-case hexadecimal): the body of a
/// streamed PUT is hashed once, off the runtime's threads, and never held in memory. The headers are the ones
/// [`signed_headers_with`] gives for a body with that hash.
#[allow(clippy::too_many_arguments)]
pub fn signed_headers_precomputed(
    method: &str,
    url: &str,
    service: &str,
    region: &str,
    creds: &ProcessCredentials,
    extra: &[(&str, &str)],
    sha256_hex: &str,
    time: SystemTime,
) -> Result<Vec<(String, String)>, HttpError> {
    signed_headers_of(
        method,
        url,
        service,
        region,
        creds,
        extra,
        SignableBody::Precomputed(sha256_hex.to_string()),
        time,
    )
}

#[allow(clippy::too_many_arguments)]
fn signed_headers_of(
    method: &str,
    url: &str,
    service: &str,
    region: &str,
    creds: &ProcessCredentials,
    extra: &[(&str, &str)],
    body: SignableBody<'_>,
    time: SystemTime,
) -> Result<Vec<(String, String)>, HttpError> {
    let uri: http::Uri = url
        .parse()
        .map_err(|_| HttpError::BadUrl(url.to_string()))?;
    if uri.scheme().is_none() || uri.authority().is_none() {
        return Err(HttpError::BadUrl(url.to_string()));
    }
    let identity = Credentials::new(
        creds.access_key_id.clone(),
        creds.secret_access_key.clone(),
        Some(creds.session_token.clone()),
        None,
        "agentd",
    )
    .into();
    let mut settings = SigningSettings::default();
    if service == "s3" {
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    }
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(time)
        .settings(settings)
        .build()
        .map_err(|e| HttpError::Sign(e.to_string()))?
        .into();
    let request = SignableRequest::new(method, url, extra.iter().copied(), body)
        .map_err(|e| HttpError::Sign(e.to_string()))?;
    let (instructions, _signature) = sign(request, &params)
        .map_err(|e| HttpError::Sign(e.to_string()))?
        .into_parts();
    let mut carrier = http::Request::new(());
    instructions.apply_to_request_http1x(&mut carrier);
    Ok(carrier
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

/// A PUT whose body is the file at `path`, streamed ([`SignedHttp::put_file`]). `digest` is the file's SHA-256 and size, made
/// before the request (`s3::digest_file`); `extra` headers are signed and sent, as for [`SignedHttp::send_with_headers`].
#[derive(Debug)]
pub struct FilePut<'a> {
    pub url: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub creds: &'a ProcessCredentials,
    pub content_type: Option<&'a str>,
    pub path: &'a Path,
    pub digest: &'a crate::s3::FileDigest,
    pub extra: &'a [(&'a str, &'a str)],
}

/// The body of a streamed PUT. A thread of the blocking pool reads the file in pieces of [`FILE_PIECE_BYTES`] and hands them
/// over a channel that holds [`FILE_PIECES_AHEAD`] of them, so the file is never whole in memory and no runtime thread reads it. The
/// connection takes frames of at most [`FILE_FRAME_BYTES`]; the bytes it took are counted (`handed`), which is how the request sees
/// whether it still moves. The reading thread ends when the body is dropped.
struct FileBody {
    pieces: tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>,
    /// What is left of the piece that is being handed out.
    piece: Bytes,
    /// The bytes of the file that the connection has not taken yet.
    left: u64,
    handed: Arc<AtomicU64>,
}

impl FileBody {
    /// The body of the first `bytes` bytes of the file at `path`, and the count of the bytes the connection has taken.
    fn open(path: &Path, bytes: u64) -> (Self, Arc<AtomicU64>) {
        let (tx, pieces) = tokio::sync::mpsc::channel(FILE_PIECES_AHEAD);
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || read_pieces(&path, bytes, &tx));
        let handed = Arc::new(AtomicU64::new(0));
        (
            Self {
                pieces,
                piece: Bytes::new(),
                left: bytes,
                handed: handed.clone(),
            },
            handed,
        )
    }
}

/// Reads the first `bytes` bytes of the file at `path` in pieces and sends them; ends when the receiver is gone. Runs on a thread of
/// the blocking pool.
fn read_pieces(path: &Path, bytes: u64, tx: &tokio::sync::mpsc::Sender<std::io::Result<Bytes>>) {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            let _ = tx.blocking_send(Err(e));
            return;
        }
    };
    let mut left = bytes;
    while left > 0 {
        let want = usize::try_from(left)
            .unwrap_or(FILE_PIECE_BYTES)
            .min(FILE_PIECE_BYTES);
        let mut piece = vec![0_u8; want];
        let mut filled = 0;
        while filled < want {
            match file.read(&mut piece[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    return;
                }
            }
        }
        if filled == 0 {
            // The file is shorter than it was when it was hashed: the body says so when the channel closes with bytes left.
            return;
        }
        piece.truncate(filled);
        left -= filled as u64;
        if tx.blocking_send(Ok(Bytes::from(piece))).is_err() {
            return;
        }
    }
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = &mut *self;
        loop {
            if !this.piece.is_empty() {
                let n = this.piece.len().min(FILE_FRAME_BYTES);
                let frame = this.piece.split_to(n);
                this.left = this.left.saturating_sub(n as u64);
                this.handed.fetch_add(n as u64, Ordering::Relaxed);
                return Poll::Ready(Some(Ok(Frame::data(frame))));
            }
            match this.pieces.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(piece))) => this.piece = piece,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) if this.left == 0 => return Poll::Ready(None),
                Poll::Ready(None) => {
                    return Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the file ended before the bytes that were hashed",
                    ))));
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0 && self.piece.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}

/// The file a download is written to until its last byte has come: `<dest>.part`, written through the blocking pool, renamed
/// to `dest` by [`PartFile::finish`] and removed on every other way out.
struct PartFile {
    path: PathBuf,
    file: Option<tokio::fs::File>,
}

impl PartFile {
    async fn create(dest: &Path) -> std::io::Result<Self> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        let path = dest.with_file_name(name);
        let file = tokio::fs::File::create(&path).await?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        match self.file.as_mut() {
            Some(file) => file.write_all(bytes).await,
            None => Ok(()),
        }
    }

    /// The last byte is written: the file gets its name. When it cannot get it (the last bytes cannot be written, the rename
    /// fails), the part file is removed here, as on every other way out.
    async fn finish(mut self, dest: &Path) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        let named = async {
            file.flush().await?;
            drop(file);
            tokio::fs::rename(&self.path, dest).await
        }
        .await;
        if named.is_err() {
            let _ = tokio::fs::remove_file(&self.path).await;
        }
        named
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        // Still open: the download did not end. The name is removed here and now, so that whoever gets the error finds no file; a
        // write that is still on its way goes to a file without a name, which is gone when it is closed.
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Waits for `work`, a request, within `limits`. `moved` is a count that rises while the request moves. The request is given
/// up with `<url> timed out` when it has run for `limits.total`, and with `<url>: no progress for <n> s` when the count stood still
/// for `limits.no_progress`. Both are watched here, in the request's own future.
async fn watched<T>(
    url: &str,
    limits: Limits,
    work: impl Future<Output = Result<T, HttpError>>,
    moved: impl Fn() -> u64,
) -> Result<T, HttpError> {
    tokio::pin!(work);
    let total = tokio::time::sleep(limits.total);
    tokio::pin!(total);
    // The count is read often enough to give a stalled request up within a quarter of its limit after the limit has passed.
    let every = (limits.no_progress / 4).clamp(Duration::from_millis(5), Duration::from_secs(1));
    let mut look = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seen = moved();
    let mut since = tokio::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            out = &mut work => return out,
            () = &mut total => return Err(HttpError::Transport(format!("{url} timed out"))),
            _ = look.tick() => {
                let now = moved();
                if now != seen {
                    seen = now;
                    since = tokio::time::Instant::now();
                } else if since.elapsed() >= limits.no_progress {
                    return Err(HttpError::Transport(format!(
                        "{url}: no progress for {} s",
                        limits.no_progress.as_secs_f32()
                    )));
                }
            }
        }
    }
}

/// The status and the headers of an answer, with an empty body.
fn head_of(resp: &http::Response<Incoming>) -> HttpResponse {
    HttpResponse {
        status: resp.status().as_u16(),
        body: Vec::new(),
        error_type: resp
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        headers: resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect(),
    }
}

/// An answer read whole into memory.
async fn read_answer(resp: http::Response<Incoming>) -> Result<HttpResponse, HttpError> {
    let mut answer = head_of(&resp);
    answer.body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| HttpError::Transport(e.to_string()))?
        .to_bytes()
        .to_vec();
    Ok(answer)
}

pub struct SignedHttp {
    client: Client<HttpsConnector<HttpConnector>, BoxBody<Bytes, std::io::Error>>,
    /// How long [`send`](Self::send) waits for a whole answer; [`REQUEST_TIMEOUT`] unless a test shortens it.
    timeout: Duration,
}

impl std::fmt::Debug for SignedHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedHttp")
    }
}

impl Default for SignedHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl SignedHttp {
    pub fn new() -> Self {
        // The connector `HttpsConnectorBuilder::build` makes.
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        // An explicit provider: in workspace builds another crate enables rustls' aws-lc-rs too, and
        // with two providers compiled in rustls cannot choose a process default.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .unwrap_or_else(|e| panic!("ring TLS provider rejected its own defaults: {e}"))
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// The same client with another limit for [`send`](Self::send) and [`send_with_headers`](Self::send_with_headers). The product keeps
    /// [`REQUEST_TIMEOUT`]; a test uses this to end a request whose answer never comes within a moment.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Signs and sends `r` (SigV4, the session credentials) and reads the whole answer, within [`REQUEST_TIMEOUT`].
    pub async fn send(&self, r: SignedRequest<'_>) -> Result<HttpResponse, HttpError> {
        self.send_timeout(r, self.timeout).await
    }

    /// [`send`](Self::send) with `extra` headers, which are signed and sent (`if-none-match: *` for a create-only put, the checksum
    /// headers of an object write and of a HEAD).
    pub async fn send_with_headers(
        &self,
        r: SignedRequest<'_>,
        extra: &[(&str, &str)],
    ) -> Result<HttpResponse, HttpError> {
        self.send_timeout_with(r, extra, self.timeout).await
    }

    pub async fn send_timeout(
        &self,
        r: SignedRequest<'_>,
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        self.send_timeout_with(r, &[], timeout).await
    }

    /// Signs `r` with `extra` in the signature, sends it with `extra` on the request and reads the whole answer within `timeout`.
    pub async fn send_timeout_with(
        &self,
        r: SignedRequest<'_>,
        extra: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let headers = signed_headers_with(
            r.method,
            r.url,
            r.service,
            r.region,
            r.creds,
            extra,
            &r.body,
            SystemTime::now(),
        )?;
        let mut builder = http::Request::builder().method(r.method).uri(r.url);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }
        if let Some(ct) = r.content_type {
            builder = builder.header("content-type", ct);
        }
        let req = builder
            .body(whole_body(r.body))
            .map_err(|_| HttpError::BadUrl(r.url.to_string()))?;
        let resp = tokio::time::timeout(timeout, self.client.request(req))
            .await
            .map_err(|_| HttpError::Transport(format!("{} timed out", r.url)))?
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        read_answer(resp).await
    }

    /// The limit of one request of this client ([`with_timeout`](Self::with_timeout)).
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// PUTs the file `put.path` as a stream. The request is signed with the hash the caller made (`put.digest`), so the file is
    /// read once more and only by the thread that feeds the connection ([`FileBody`]): no runtime thread reads or hashes it, and it is
    /// never whole in memory. The answer is read whole (it is small). The request ends within `limits`: with `<url> timed out` after
    /// `limits.total`, and with `<url>: no progress for <n> s` when no byte of the body left for the connection for
    /// `limits.no_progress`, or the head of the answer did not come that long after the body had ended.
    pub async fn put_file(
        &self,
        put: FilePut<'_>,
        limits: Limits,
    ) -> Result<HttpResponse, HttpError> {
        let headers = signed_headers_precomputed(
            "PUT",
            put.url,
            put.service,
            put.region,
            put.creds,
            put.extra,
            &put.digest.sha256_hex,
            SystemTime::now(),
        )?;
        let mut builder = http::Request::builder()
            .method("PUT")
            .uri(put.url)
            .header("content-length", put.digest.bytes);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        for (k, v) in put.extra {
            builder = builder.header(*k, *v);
        }
        if let Some(ct) = put.content_type {
            builder = builder.header("content-type", ct);
        }
        let (body, handed) = FileBody::open(put.path, put.digest.bytes);
        let req = builder
            .body(body.boxed())
            .map_err(|_| HttpError::BadUrl(put.url.to_string()))?;
        let answer = async {
            let resp = self
                .client
                .request(req)
                .await
                .map_err(|e| HttpError::Transport(e.to_string()))?;
            read_answer(resp).await
        };
        watched(put.url, limits, answer, || handed.load(Ordering::Relaxed)).await
    }

    /// Sends `r` (a GET) and writes the body of a 200 answer to `dest` as it arrives: to `<dest>.part` through the blocking
    /// pool, renamed after the last byte, so the body is never whole in memory and a download that ends early leaves no file. The
    /// answer comes back with an empty body. Any other status is read into memory and comes back as it is (an error document is
    /// small). The head of the answer has `limits.total`, as it had; the body has what is left of it, or of
    /// [`body_time`] of the size the answer names when that is longer, so a large object that moves is not cut by a limit that was
    /// made for a request. The body is given up when nothing of it arrived for `limits.no_progress`.
    pub async fn get_to_file(
        &self,
        r: SignedRequest<'_>,
        dest: &Path,
        limits: Limits,
    ) -> Result<HttpResponse, HttpError> {
        let started = tokio::time::Instant::now();
        let headers = signed_headers_with(
            r.method,
            r.url,
            r.service,
            r.region,
            r.creds,
            &[],
            &r.body,
            SystemTime::now(),
        )?;
        let mut builder = http::Request::builder().method(r.method).uri(r.url);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        if let Some(ct) = r.content_type {
            builder = builder.header("content-type", ct);
        }
        let req = builder
            .body(whole_body(r.body))
            .map_err(|_| HttpError::BadUrl(r.url.to_string()))?;
        // Nothing of a GET moves before the head of its answer comes, so the no-progress limit does not count that wait: the head has
        // the whole time of the request, as before the body was streamed.
        let resp = tokio::time::timeout(limits.total, self.client.request(req))
            .await
            .map_err(|_| HttpError::Transport(format!("{} timed out", r.url)))?
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        if resp.status().as_u16() != 200 {
            let left = limits.total.saturating_sub(started.elapsed());
            let limits = Limits {
                total: left,
                ..limits
            };
            return watched(r.url, limits, read_answer(resp), || 0).await;
        }
        let answer = head_of(&resp);
        let named = answer
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok());
        let total = named.map_or(limits.total, |bytes| limits.total.max(body_time(bytes)));
        let arrived = AtomicU64::new(0);
        let body = async {
            let io = |e: std::io::Error| HttpError::Transport(format!("{}: {e}", dest.display()));
            let mut part = PartFile::create(dest).await.map_err(io)?;
            let mut body = resp.into_body();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| HttpError::Transport(e.to_string()))?;
                if let Ok(data) = frame.into_data() {
                    arrived.fetch_add(data.len() as u64, Ordering::Relaxed);
                    part.write(&data).await.map_err(io)?;
                }
            }
            part.finish(dest).await.map_err(io)
        };
        let limits = Limits {
            total: total.saturating_sub(started.elapsed()),
            ..limits
        };
        watched(r.url, limits, body, || arrived.load(Ordering::Relaxed)).await?;
        Ok(answer)
    }
}

/// A body that is whole in memory, in the type the client takes.
fn whole_body(bytes: Vec<u8>) -> BoxBody<Bytes, std::io::Error> {
    Full::new(Bytes::from(bytes))
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// The request of a streamed PUT is signed with the hash of a body that agentd does not hold. The signature must be the one
    /// that the bytes themselves give, with and without the headers of a create-only write.
    #[test]
    fn b860_precomputed_signature_equals_the_signature_over_the_bytes() {
        let creds = ProcessCredentials {
            version: 1,
            // Joined at run time, so no key-shaped literal is committed.
            access_key_id: format!("ASIA{}", "TESTTESTTESTTEST"),
            secret_access_key: "stand-in-secret-for-a-signature-test".into(),
            session_token: "stand-in-token-for-a-signature-test".into(),
            expiration: "2030-01-01T00:00:00Z".into(),
        };
        let body = b"the bytes of a bundle\n".repeat(500);
        let hex: String = Sha256::digest(&body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let url = "https://bucket.s3.us-east-1.amazonaws.com/runs/01RUN/agents/direct/turns/1/snapshot.bundle";
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let plain: &[(&str, &str)] = &[];
        let create_only: &[(&str, &str)] = &[
            ("if-none-match", "*"),
            ("x-amz-checksum-sha256", "c3RhbmQtaW4="),
        ];
        for extra in [plain, create_only] {
            let over_the_bytes =
                signed_headers_with("PUT", url, "s3", "us-east-1", &creds, extra, &body, time)
                    .unwrap();
            let precomputed = signed_headers_precomputed(
                "PUT",
                url,
                "s3",
                "us-east-1",
                &creds,
                extra,
                &hex,
                time,
            )
            .unwrap();
            assert!(
                over_the_bytes
                    .iter()
                    .any(|(k, v)| k == "x-amz-content-sha256" && *v == hex),
                "the signed request names the hash of its body: {over_the_bytes:?}"
            );
            assert_eq!(precomputed, over_the_bytes);
        }
    }
}
