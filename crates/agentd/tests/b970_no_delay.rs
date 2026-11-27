//! The worker's HTTP clients send without waiting for an acknowledgement. A streamed PUT (`SignedHttp::put_file`) leaves as
//! two writes: the head, and the body once the first piece of the file is read. With Nagle's algorithm on, the kernel holds the
//! second write back until the first is acknowledged, and the receiving side may hold that acknowledgement back for its
//! delayed-acknowledgement time. Two uploads at the end of a turn waited that way. Every connection of the worker's clients now has
//! no-delay set (`agentd::http::connector`), and no setting changes it.
//!
//! Three tests hold it, and none types in a time:
//! - the option itself, read on a connection the worker's connector opens, beside a connection of the library's own connector;
//! - the wait itself: the same request as two writes through a client with Nagle's algorithm on, through a client with no-delay and
//!   through the worker's own `put_file`, against a stand-in on the loopback interface, judged by this run's own figures;
//! - the sources: no file of the worker but `src/http.rs` builds a connector, so every client has the one.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::http::{FilePut, Limits, SignedHttp};
use agentd::s3::FileDigest;
use common::{creds, scratch};
use hyper_rustls::MaybeHttpsStream;
use hyper_util::client::legacy::connect::HttpConnector;
use rustix::fs::{CWD, FileType, Mode, mknodat};
use rustix::net::sockopt::set_tcp_quickack;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tower_service::Service;

/// How long a step of a test may take before the test gives up. It ends a test that hangs; nothing is judged by it.
const LIVENESS: Duration = Duration::from_secs(60);

/// The requests of each client that are judged, after [`WARM_UPS`] that are not.
const ROUNDS: usize = 15;
const WARM_UPS: usize = 3;
/// Two thirds of the judged requests: how many must say the same for a verdict.
const NEED: usize = (2 * ROUNDS).div_ceil(3);
/// The cause is present in a run when the client with Nagle's algorithm on takes at least this many times as long as the client
/// with no-delay.
const FACTOR: u32 = 10;

/// What the stand-in says about a request.
#[derive(Debug)]
enum Seen {
    /// The whole head is read. `nth` counts the requests of its connection from 0; `with_head` is how many bytes of the body had
    /// come with the head.
    Head {
        nth: usize,
        head: Vec<u8>,
        with_head: usize,
    },
    /// The last byte of the body is read.
    Body { at: Instant },
}

/// A stand-in for the bucket on the loopback interface. It reads a request, says when it has the head and when it has the body, and
/// answers 200. A connection is kept. After a connection is accepted and before every answer it asks the kernel to hold its
/// acknowledgements back (`TCP_QUICKACK` off), as a peer does that has just answered a request: without that, the kernel's own
/// state decides from request to request whether the acknowledgement of a head waits.
struct StandIn {
    addr: SocketAddr,
    seen: mpsc::UnboundedReceiver<Seen>,
}

impl StandIn {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, seen) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, tx.clone()));
            }
        });
        Self { addr, seen }
    }

    async fn next(&mut self) -> Seen {
        tokio::time::timeout(LIVENESS, self.seen.recv())
            .await
            .expect("the stand-in said nothing")
            .expect("the stand-in ended")
    }

    /// The head of the next request.
    async fn head(&mut self) -> (usize, Vec<u8>, usize) {
        match self.next().await {
            Seen::Head {
                nth,
                head,
                with_head,
            } => (nth, head, with_head),
            other => panic!("the stand-in read {other:?} where a head was due"),
        }
    }

    /// When the body of the request was whole at the stand-in.
    async fn body(&mut self) -> Instant {
        match self.next().await {
            Seen::Body { at } => at,
            other => panic!("the stand-in read {other:?} where a body was due"),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The value of `content-length` in a request head.
fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .expect("a request without content-length")
}

async fn serve(mut stream: TcpStream, tx: mpsc::UnboundedSender<Seen>) {
    if set_tcp_quickack(&stream, false).is_err() {
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    let mut nth = 0;
    loop {
        let end = loop {
            if let Some(at) = find(&buf, b"\r\n\r\n") {
                break at + 4;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = buf[..end].to_vec();
        let want = content_length(&head);
        let with_head = buf.len() - end;
        if tx
            .send(Seen::Head {
                nth,
                head,
                with_head,
            })
            .is_err()
        {
            return;
        }
        while buf.len() - end < want {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let at = Instant::now();
        buf.drain(..end + want);
        if set_tcp_quickack(&stream, false).is_err() || tx.send(Seen::Body { at }).is_err() {
            return;
        }
        if stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        nth += 1;
    }
}

/// One request of a client: how long the body took from the moment it was handed over to its last byte at the stand-in, and whether
/// the request was the first of its connection.
#[derive(Debug, Clone, Copy)]
struct Sample {
    took: Duration,
    first_of_its_connection: bool,
}

/// A client of the test's own: one kept connection, and a request sent as two writes.
struct Raw {
    stream: TcpStream,
}

impl Raw {
    async fn open(addr: SocketAddr, no_delay: bool) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        stream.set_nodelay(no_delay).unwrap();
        assert_eq!(stream.nodelay().unwrap(), no_delay);
        Self { stream }
    }

    /// Writes the head, and the body once the stand-in has read the head.
    async fn request(&mut self, stand_in: &mut StandIn, head: &[u8], body: &[u8]) -> Sample {
        self.stream.write_all(head).await.unwrap();
        let (nth, _, with_head) = stand_in.head().await;
        assert_eq!(with_head, 0, "the body was not sent yet");
        let began = Instant::now();
        self.stream.write_all(body).await.unwrap();
        let at = stand_in.body().await;
        let mut answer = Vec::new();
        let mut chunk = [0_u8; 256];
        while !answer.ends_with(b"\r\n\r\n") {
            let n = tokio::time::timeout(LIVENESS, self.stream.read(&mut chunk))
                .await
                .expect("no answer")
                .unwrap();
            assert!(n > 0, "the stand-in closed the connection");
            answer.extend_from_slice(&chunk[..n]);
        }
        assert!(answer.starts_with(b"HTTP/1.1 200 "));
        Sample {
            took: at.saturating_duration_since(began),
            first_of_its_connection: nth == 0,
        }
    }
}

/// The worker's own client and its streamed PUT. The file of a request is a FIFO that the test fills only after the stand-in has
/// read the head, so the head and the body are two writes in every request, as they are when the first piece of a file is not read
/// yet.
struct Worker {
    http: Arc<SignedHttp>,
    dir: PathBuf,
    url: String,
    made: usize,
}

impl Worker {
    fn new(addr: SocketAddr) -> Self {
        Self {
            http: Arc::new(SignedHttp::new()),
            dir: scratch("b970"),
            url: format!("http://{addr}/bucket/runs/r1/agents/direct/turns/1/snapshot.json"),
            made: 0,
        }
    }

    /// One streamed PUT of `body`: its sample and the head the stand-in read.
    async fn request(&mut self, stand_in: &mut StandIn, body: &[u8]) -> (Sample, Vec<u8>) {
        self.made += 1;
        let fifo = self.dir.join(format!("body-{}", self.made));
        mknodat(CWD, &fifo, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
        // Opened for reading and writing: neither this open nor the worker's open for reading waits for the other end.
        let mut feed = pipe::OpenOptions::new()
            .read_write(true)
            .open_sender(&fifo)
            .unwrap();
        let put = tokio::spawn(put(
            self.http.clone(),
            self.url.clone(),
            fifo,
            digest_of(body),
        ));
        let (nth, head, with_head) = stand_in.head().await;
        assert_eq!(
            with_head, 0,
            "the head of the worker's streamed PUT left alone: the file was not filled yet"
        );
        let began = Instant::now();
        feed.write_all(body).await.unwrap();
        let at = stand_in.body().await;
        let status = tokio::time::timeout(LIVENESS, put)
            .await
            .expect("the streamed PUT did not end")
            .unwrap();
        assert_eq!(status, 200);
        let sample = Sample {
            took: at.saturating_duration_since(began),
            first_of_its_connection: nth == 0,
        };
        (sample, head)
    }
}

/// The SHA-256 and the size `put_file` signs with. It does not send the base64 form.
fn digest_of(body: &[u8]) -> FileDigest {
    FileDigest {
        sha256_hex: Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        sha256_base64: String::new(),
        bytes: body.len() as u64,
    }
}

async fn put(http: Arc<SignedHttp>, url: String, path: PathBuf, digest: FileDigest) -> u16 {
    let creds = creds("2099-01-01T00:00:00Z");
    let limits = Limits {
        total: LIVENESS,
        no_progress: LIVENESS,
    };
    http.put_file(
        FilePut {
            url: &url,
            service: "s3",
            region: "us-east-1",
            creds: &creds,
            content_type: Some("application/json"),
            path: &path,
            digest: &digest,
            extra: &[],
        },
        limits,
    )
    .await
    .unwrap()
    .status
}

fn median(of: &[Duration]) -> Duration {
    let mut sorted = of.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

fn micros(of: &[Duration]) -> Vec<u128> {
    of.iter().map(Duration::as_micros).collect()
}

/// The wait itself, and its absence in the worker's client. Three clients send the same request as two writes to the stand-in, in
/// turns: one with Nagle's algorithm on, one with no-delay, and the worker's `put_file`. The first two send the very head the
/// worker's client sent. Judged by this run alone: half of the median of the client with Nagle's algorithm on parts a request that
/// waited from one that did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b970_a_streamed_put_of_the_worker_does_not_wait_for_the_acknowledgement_of_its_head() {
    let mut stand_in = StandIn::start().await;
    let body = vec![b'{'; 176];
    let mut worker = Worker::new(stand_in.addr);
    let mut head = Vec::new();
    for _ in 0..WARM_UPS {
        head = worker.request(&mut stand_in, &body).await.1;
    }
    let mut nagle = Raw::open(stand_in.addr, false).await;
    let mut no_delay = Raw::open(stand_in.addr, true).await;
    for _ in 0..WARM_UPS {
        nagle.request(&mut stand_in, &head, &body).await;
        no_delay.request(&mut stand_in, &head, &body).await;
    }

    let (mut n, mut d, mut w) = (Vec::new(), Vec::new(), Vec::new());
    let mut fresh = 0;
    let mut round = 0;
    while w.len() < ROUNDS {
        assert!(
            round < 3 * ROUNDS,
            "INCONCLUSIVE: the worker's client opened a new connection for {fresh} of {round} requests, so only {} of them could \
             be judged",
            w.len()
        );
        // The three clients take turns, and the turn moves on with every round.
        for arm in 0..3 {
            match (arm + round) % 3 {
                0 => n.push(nagle.request(&mut stand_in, &head, &body).await.took),
                1 => d.push(no_delay.request(&mut stand_in, &head, &body).await.took),
                _ => {
                    let (sample, _) = worker.request(&mut stand_in, &body).await;
                    // A new connection is acknowledged at once by the kernel's own rule: such a request says nothing.
                    if sample.first_of_its_connection {
                        fresh += 1;
                    } else {
                        w.push(sample.took);
                    }
                }
            }
        }
        round += 1;
    }
    n.truncate(ROUNDS);
    d.truncate(ROUNDS);

    let half = median(&n) / 2;
    let waited = |took: &&Duration| **took >= half;
    let n_waited = n.iter().filter(waited).count();
    let d_waited = d.iter().filter(waited).count();
    let w_waited = w.iter().filter(waited).count();
    eprintln!(
        "B970 body_us nagle={:?} no_delay={:?} worker={:?} half_of_nagle_median_us={} waited nagle={n_waited} no_delay={d_waited} \
         worker={w_waited} of {ROUNDS} new_connections_of_the_worker={fresh}",
        micros(&n),
        micros(&d),
        micros(&w),
        half.as_micros(),
    );

    // TRIGGER PRESENT: on this machine, in this run, a client that keeps Nagle's algorithm on waits for the acknowledgement of its
    // head, and a client with no-delay does not.
    assert!(
        median(&n) >= median(&d) * FACTOR && n_waited >= NEED && d_waited <= ROUNDS - NEED,
        "TRIGGER ABSENT: a client with Nagle's algorithm on did not wait for the acknowledgement of its head in this run (median \
         {:?}, {n_waited} of {ROUNDS} requests waited) where a client with no-delay took {:?} in the median ({d_waited} waited): \
         this run can say nothing about the worker's client",
        median(&n),
        median(&d),
    );
    assert!(
        ROUNDS - w_waited >= NEED,
        "the worker's streamed PUT waited for the acknowledgement of its head in {w_waited} of {ROUNDS} requests: its body took \
         {:?} in the median, a client with Nagle's algorithm on {:?}, a client with no-delay {:?}",
        median(&w),
        median(&n),
        median(&d),
    );
}

/// The option itself. A connection that the worker's connector opens has no-delay set; a connection of the library's own connector,
/// built as the worker's was before, has not (TRIGGER PRESENT: the library still leaves Nagle's algorithm on).
#[tokio::test]
async fn b970_a_connection_of_the_workers_connector_has_no_delay_set() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri: http::Uri = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();

    let mut librarys = HttpConnector::new();
    librarys.enforce_http(false);
    std::future::poll_fn(|cx| librarys.poll_ready(cx))
        .await
        .unwrap();
    let io = librarys.call(uri.clone()).await.unwrap();
    assert!(
        !io.inner().nodelay().unwrap(),
        "TRIGGER ABSENT: the library's connector sets no-delay by itself now; the worker's own line may go"
    );

    let mut workers = agentd::http::connector();
    std::future::poll_fn(|cx| workers.poll_ready(cx))
        .await
        .unwrap();
    let MaybeHttpsStream::Http(io) = workers.call(uri).await.unwrap() else {
        panic!("a plain http address gave a TLS stream");
    };
    assert!(
        io.inner().nodelay().unwrap(),
        "a connection of the worker's connector has Nagle's algorithm on: a streamed PUT waits for the acknowledgement of its head"
    );
}

/// Every Rust file below `dir`, with its path from `root` and its lines that are no comment.
fn sources(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(root, &path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            let code: String = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let name = path.strip_prefix(root).unwrap().display().to_string();
            out.push((name, code));
        }
    }
}

/// The sources. No file of the worker but `src/http.rs` builds a connector, and that file builds one, with no-delay: so every HTTP
/// client of the worker (the signed client and the transcript store) has it.
#[test]
fn b970_every_http_client_of_the_worker_is_built_from_the_one_connector() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut all = Vec::new();
    sources(&root, &root, &mut all);
    all.sort();
    assert!(all.len() >= 100, "the worker's sources were not found");
    let builds: Vec<&str> = all
        .iter()
        .filter(|(_, code)| {
            code.contains("HttpConnector::") || code.contains("HttpsConnectorBuilder")
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        builds,
        ["http.rs"],
        "a file of the worker builds a connector of its own: use agentd::http::connector"
    );
    let http = &all.iter().find(|(name, _)| name == "http.rs").unwrap().1;
    assert_eq!(http.matches("HttpConnector::new()").count(), 1);
    assert_eq!(http.matches(".set_nodelay(true)").count(), 1);
    let users: Vec<&str> = all
        .iter()
        .filter(|(_, code)| code.contains("Client::builder("))
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        users,
        ["direct/transcript/s3store.rs", "http.rs"],
        "the clients of the worker; a new one is built with agentd::http::connector"
    );
    for (name, code) in all
        .iter()
        .filter(|(name, _)| users.contains(&name.as_str()))
    {
        assert!(
            code.contains("connector()"),
            "{name} builds its client without the worker's connector"
        );
    }
}
