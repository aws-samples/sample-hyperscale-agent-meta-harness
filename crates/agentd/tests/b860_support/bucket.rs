//! A stand-in for the bucket that never holds a body. It is a small HTTP/1.1 server on threads of its own (never on the runtime
//! of the test, so what a test measures on its runtime thread is agentd's work alone). It reads the body of a PUT in pieces, throws the
//! bytes away, counts them and hashes them; it serves a GET with bytes it makes as it sends them; and a [`Plan`] tells it to stop
//! reading, to read slowly, to stop sending, to send slowly or to answer 500.
use super::{hex, noise};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The size of the pieces the stand-in reads and sends.
const PIECE: usize = 64 * 1024;
/// The size of the blocks a served body is made of.
const BLOCK: usize = 1024 * 1024;

/// What the stand-in does with the requests it gets. The default takes every PUT and answers 200.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// A PUT: after this many bytes of the body the stand-in reads nothing more and never answers; the connection stays open.
    pub stop_reading_after: Option<u64>,
    /// A PUT: the body is read this many bytes at a time, with this pause after each read.
    pub read_slowly: Option<(usize, Duration)>,
    /// The next so many PUTs are answered 500 after their whole body was read.
    pub answer_500: u32,
    /// A GET: the size of the body.
    pub get_bytes: u64,
    /// A GET: the stand-in waits this long before it sends the head of its answer.
    pub head_after: Option<Duration>,
    /// A GET: after the head and this many bytes of the body the stand-in sends nothing more; the connection stays open.
    pub stop_sending_after: Option<u64>,
    /// A GET: the body is sent this many bytes at a time, with this pause after each write.
    pub send_slowly: Option<(usize, Duration)>,
}

/// One PUT whose whole body the stand-in read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutSeen {
    pub path: String,
    pub bytes: u64,
    pub sha256_hex: String,
    pub status: u16,
    /// The value of the request's `x-amz-content-sha256` header.
    pub signed_sha256: String,
}

/// One GET whose whole body the stand-in sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetServed {
    pub path: String,
    pub bytes: u64,
    pub sha256_hex: String,
}

struct State {
    plan: Mutex<Plan>,
    puts: Mutex<Vec<PutSeen>>,
    gets: Mutex<Vec<GetServed>>,
    /// The bytes of the body in flight that the stand-in has read (a PUT) or sent (a GET).
    moved: AtomicU64,
    /// A request is held: the stand-in stopped reading or sending.
    holding: AtomicBool,
    closed: AtomicBool,
}

pub struct Bucket {
    /// `http://127.0.0.1:<port>`: the endpoint of an `S3Objects`.
    pub endpoint: String,
    addr: SocketAddr,
    state: Arc<State>,
}

impl Bucket {
    pub fn start(plan: Plan) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State {
            plan: Mutex::new(plan),
            puts: Mutex::new(Vec::new()),
            gets: Mutex::new(Vec::new()),
            moved: AtomicU64::new(0),
            holding: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        let shared = state.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if shared.closed.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let _ = serve(conn, &shared);
                });
            }
        });
        Self {
            endpoint: format!("http://{addr}"),
            addr,
            state,
        }
    }

    /// The `s3://` address of `key` in the stand-in.
    pub fn uri(&self, key: &str) -> String {
        format!("s3://stand-in-bucket/{key}")
    }

    /// What the stand-in does from the next request on.
    pub fn set_plan(&self, plan: Plan) {
        *self.state.plan.lock().unwrap() = plan;
    }

    /// The PUTs whose whole body was read, in order.
    pub fn puts(&self) -> Vec<PutSeen> {
        self.state.puts.lock().unwrap().clone()
    }

    /// The GETs whose whole body was sent, in order.
    pub fn gets(&self) -> Vec<GetServed> {
        self.state.gets.lock().unwrap().clone()
    }

    /// Whether a request is held now (the stand-in stopped reading or sending).
    pub fn holding(&self) -> bool {
        self.state.holding.load(Ordering::SeqCst)
    }

    /// The bytes of the body in flight that the stand-in has read or sent.
    pub fn moved(&self) -> u64 {
        self.state.moved.load(Ordering::SeqCst)
    }

    /// Waits until a request is held, at most `limit`.
    pub fn wait_holding(&self, limit: Duration) -> bool {
        let began = Instant::now();
        while !self.holding() {
            if began.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

impl Drop for Bucket {
    fn drop(&mut self) {
        // The held requests end, and one connection wakes the thread that accepts them so that it ends too.
        self.state.closed.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

/// Holds a request until the stand-in is dropped.
fn hold(s: &State) {
    s.holding.store(true, Ordering::SeqCst);
    while !s.closed.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn serve(conn: TcpStream, s: &State) -> std::io::Result<()> {
    conn.set_nodelay(true)?;
    let mut out = conn.try_clone()?;
    let mut reader = BufReader::with_capacity(PIECE, conn);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut words = line.split_whitespace();
        let method = words.next().unwrap_or_default().to_string();
        let path = words.next().unwrap_or_default().to_string();
        let mut length: u64 = 0;
        let mut signed_sha256 = String::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                } else if name.eq_ignore_ascii_case("x-amz-content-sha256") {
                    signed_sha256 = value.trim().to_string();
                }
            }
        }
        let plan = s.plan.lock().unwrap().clone();
        match method.as_str() {
            "PUT" => {
                let mut hasher = Sha256::new();
                let mut got: u64 = 0;
                let mut piece = vec![0_u8; PIECE];
                s.moved.store(0, Ordering::SeqCst);
                while got < length {
                    if plan.stop_reading_after.is_some_and(|stop| got >= stop) {
                        hold(s);
                        return Ok(());
                    }
                    let mut want = u64::try_from(piece.len()).unwrap().min(length - got);
                    if let Some(stop) = plan.stop_reading_after {
                        want = want.min(stop - got);
                    }
                    if let Some((slow, _)) = plan.read_slowly {
                        want = want.min(slow as u64);
                    }
                    let n = reader.read(&mut piece[..usize::try_from(want).unwrap()])?;
                    if n == 0 {
                        return Ok(());
                    }
                    hasher.update(&piece[..n]);
                    got += n as u64;
                    s.moved.store(got, Ordering::SeqCst);
                    if let Some((_, pause)) = plan.read_slowly {
                        std::thread::sleep(pause);
                    }
                }
                let fail = {
                    let mut plan = s.plan.lock().unwrap();
                    let fail = plan.answer_500 > 0;
                    plan.answer_500 = plan.answer_500.saturating_sub(1);
                    fail
                };
                let status = if fail { 500 } else { 200 };
                let seen = {
                    let mut puts = s.puts.lock().unwrap();
                    puts.push(PutSeen {
                        path,
                        bytes: got,
                        sha256_hex: hex(&hasher.finalize()),
                        status,
                        signed_sha256,
                    });
                    puts.len()
                };
                if fail {
                    let body = "<Error><Code>InternalError</Code><Message>stand-in failure</Message></Error>";
                    write!(
                        out,
                        "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )?;
                } else {
                    write!(
                        out,
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nx-amz-version-id: stand-in-{seen}\r\n\r\n"
                    )?;
                }
                out.flush()?;
            }
            "GET" => {
                if let Some(wait) = plan.head_after {
                    std::thread::sleep(wait);
                }
                write!(
                    out,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/octet-stream\r\n\r\n",
                    plan.get_bytes
                )?;
                out.flush()?;
                let mut hasher = Sha256::new();
                let mut sent: u64 = 0;
                let mut seed = 0;
                s.moved.store(0, Ordering::SeqCst);
                while sent < plan.get_bytes {
                    let block = noise(
                        seed,
                        usize::try_from((plan.get_bytes - sent).min(BLOCK as u64)).unwrap(),
                    );
                    seed += 1;
                    let step = plan.send_slowly.map_or(PIECE, |(slow, _)| slow);
                    for part in block.chunks(step) {
                        let mut part = part;
                        if let Some(stop) = plan.stop_sending_after {
                            if sent >= stop {
                                out.flush()?;
                                hold(s);
                                return Ok(());
                            }
                            let room = usize::try_from(stop - sent).unwrap_or(usize::MAX);
                            part = &part[..part.len().min(room)];
                        }
                        out.write_all(part)?;
                        hasher.update(part);
                        sent += part.len() as u64;
                        s.moved.store(sent, Ordering::SeqCst);
                        if let Some((_, pause)) = plan.send_slowly {
                            out.flush()?;
                            std::thread::sleep(pause);
                        }
                        if plan.stop_sending_after.is_some_and(|stop| sent >= stop) {
                            out.flush()?;
                            hold(s);
                            return Ok(());
                        }
                    }
                }
                out.flush()?;
                s.gets.lock().unwrap().push(GetServed {
                    path,
                    bytes: sent,
                    sha256_hex: hex(&hasher.finalize()),
                });
            }
            "HEAD" => {
                write!(out, "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")?;
                out.flush()?;
            }
            _ => {
                write!(
                    out,
                    "HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n"
                )?;
                out.flush()?;
            }
        }
    }
}
