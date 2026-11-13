//! Response bodies, and the per-request bookkeeping that ends with the `pkgproxy <prefix> <status> <bytes> <ms>` log line.
//!
//! A streamed body is never held whole: each frame from the upstream goes to the client as it arrives. It is cut off with an error
//! (the client sees a short body, never a padded one) when the upstream is silent for `idle`, or when the request has run past its
//! deadline.

use crate::Log;
use crate::stats::Stats;
use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};

/// When a connection last moved bytes, and how many requests are in flight on it. The idle watchdog closes a connection that has
/// been silent for the idle timeout with nothing in flight.
#[derive(Debug)]
pub(crate) struct Activity {
    base: Instant,
    last_ms: AtomicU64,
    inflight: AtomicUsize,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            last_ms: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
        }
    }

    pub fn touch(&self) {
        let ms = Instant::now()
            .saturating_duration_since(self.base)
            .as_millis();
        self.last_ms
            .store(u64::try_from(ms).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub fn last(&self) -> Instant {
        self.base + Duration::from_millis(self.last_ms.load(Ordering::Relaxed))
    }

    pub fn busy(&self) -> bool {
        self.inflight.load(Ordering::Relaxed) > 0
    }
}

/// One request's life: in flight on its connection, and, once the status is known, counted and logged when it ends.
pub(crate) struct Life {
    stats: Arc<Stats>,
    log: Arc<dyn Log>,
    act: Arc<Activity>,
    started: Instant,
    label: &'static str,
    status: u16,
    bytes: u64,
    recorded: bool,
}

impl Life {
    pub fn new(stats: Arc<Stats>, log: Arc<dyn Log>, act: Arc<Activity>) -> Self {
        act.inflight.fetch_add(1, Ordering::Relaxed);
        act.touch();
        Self {
            stats,
            log,
            act,
            started: Instant::now(),
            label: "-",
            status: 0,
            bytes: 0,
            recorded: false,
        }
    }

    /// The status is known: count the answer and remember what the log line will say.
    pub fn record(&mut self, prefix: Option<&'static str>, status: u16, refused: bool) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        self.label = prefix.unwrap_or("-");
        self.status = status;
        self.stats.record(prefix, status, refused);
    }

    /// Upstream data arrived: the connection is alive.
    pub fn progress(&self) {
        self.act.touch();
    }

    pub fn add_bytes(&mut self, n: usize) {
        let n = n as u64;
        self.bytes += n;
        self.stats.add_bytes(n);
    }
}

impl Drop for Life {
    fn drop(&mut self) {
        self.act.inflight.fetch_sub(1, Ordering::Relaxed);
        self.act.touch();
        if self.recorded {
            let ms = self.started.elapsed().as_millis();
            self.log.line(&format!(
                "pkgproxy {} {} {} {}",
                self.label, self.status, self.bytes, ms
            ));
        }
    }
}

#[derive(Debug)]
pub(crate) struct BodyError(pub &'static str);

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for BodyError {}

enum Kind {
    Empty,
    Full(Option<Bytes>),
    Stream {
        inner: Incoming,
        len: Option<u64>,
        idle: Duration,
        idle_sleep: Pin<Box<Sleep>>,
        deadline: Pin<Box<Sleep>>,
    },
}

/// What the proxy sends a client.
pub(crate) struct OutBody {
    kind: Kind,
    life: Life,
}

impl fmt::Debug for OutBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutBody")
    }
}

impl OutBody {
    pub fn empty(life: Life) -> Self {
        Self {
            kind: Kind::Empty,
            life,
        }
    }

    pub fn full(life: Life, bytes: Bytes) -> Self {
        Self {
            kind: Kind::Full(Some(bytes)),
            life,
        }
    }

    pub fn stream(
        life: Life,
        inner: Incoming,
        len: Option<u64>,
        idle: Duration,
        deadline: Instant,
    ) -> Self {
        Self {
            kind: Kind::Stream {
                inner,
                len,
                idle,
                idle_sleep: Box::pin(tokio::time::sleep(idle)),
                deadline: Box::pin(tokio::time::sleep_until(deadline)),
            },
            life,
        }
    }
}

impl Body for OutBody {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        let this = self.get_mut();
        match &mut this.kind {
            Kind::Empty => Poll::Ready(None),
            Kind::Full(b) => match b.take() {
                Some(bytes) if !bytes.is_empty() => {
                    this.life.add_bytes(bytes.len());
                    Poll::Ready(Some(Ok(Frame::data(bytes))))
                }
                _ => Poll::Ready(None),
            },
            Kind::Stream {
                inner,
                idle,
                idle_sleep,
                deadline,
                ..
            } => {
                if deadline.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Some(Err(BodyError("request time limit reached"))));
                }
                loop {
                    match Pin::new(&mut *inner).poll_frame(cx) {
                        // Trailers are never passed on: only the listed headers go to a client.
                        Poll::Ready(Some(Ok(frame))) if frame.is_trailers() => continue,
                        Poll::Ready(Some(Ok(frame))) => {
                            if let Some(data) = frame.data_ref() {
                                this.life.add_bytes(data.len());
                            }
                            idle_sleep.as_mut().reset(Instant::now() + *idle);
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        Poll::Ready(Some(Err(_))) => {
                            return Poll::Ready(Some(Err(BodyError("upstream body failed"))));
                        }
                        Poll::Ready(None) => return Poll::Ready(None),
                        Poll::Pending => {
                            return if idle_sleep.as_mut().poll(cx).is_ready() {
                                Poll::Ready(Some(Err(BodyError("upstream idle"))))
                            } else {
                                Poll::Pending
                            };
                        }
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            Kind::Empty => true,
            Kind::Full(b) => b.as_ref().is_none_or(Bytes::is_empty),
            Kind::Stream { inner, .. } => inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            Kind::Empty => SizeHint::with_exact(0),
            Kind::Full(b) => SizeHint::with_exact(b.as_ref().map_or(0, |b| b.len() as u64)),
            Kind::Stream { len: Some(n), .. } => SizeHint::with_exact(*n),
            Kind::Stream { len: None, .. } => SizeHint::default(),
        }
    }
}
