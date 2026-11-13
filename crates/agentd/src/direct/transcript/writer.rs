//! The transcript writer.
//!
//! `push` is called on the read path of the engine connection: it copies the message into a bounded queue and returns, whatever the
//! store does. Two tasks do the rest. The **processor** turns messages into numbered record lines ([`Pipeline`]), gathers them into
//! batches and cuts a batch at the byte limit, 30 s after its first record, at the answer of a prompt, at a cancel and at the end. The
//! **uploader** writes each batch as one write-once object (`If-None-Match: *`), retries a failure, and takes the next number when the
//! key is taken. Nothing here fails the run: a batch that cannot be stored becomes a gap that the next object names, and the final
//! flush is bounded by a deadline.
//!
//! Numbers. Records are numbered from 0 in each writer (incarnation); objects take `seq` from the highest valid object in the prefix
//! plus one. A key that exists and is not this writer's own earlier write is a conflict: the writer lists after it and takes the first
//! free number after the run of keys it found, so a prefix full of keys already taken costs one refused put, not one per key. A put whose
//! answer was lost is found again by its header (same incarnation and range) and counts as stored.
//!
//! Bounds. The capture queue and the waiting objects together hold at most `queue_bytes`; past that a tool call's bodies are dropped
//! first (the record stays), then a whole message, and the waiting objects are dropped oldest first; each drop is counted and named in
//! a gap. The last object of a clean end holds an `end` record and says `closed`, so a restart can tell a clean end from a lost tail.

use crate::acp::Dir;
use crate::direct::transcript::chunk::{Header, MAX_RECORDS, decode_header, encode};
use crate::direct::transcript::pipeline::{Line, Pipeline, Stats, WireMsg, gap_line};
use crate::direct::transcript::settings::Settings;
use crate::direct::transcript::store::{ChunkStore, PutOutcome};
use crate::secrets::SecretSet;
use crate::time::{format_rfc3339, now_unix};
use harness_core::redact::Redactor;
use harness_protocol::direct::s3::{transcript_chunk_key, transcript_prefix};
use harness_protocol::transcript::{
    CompletenessEntry, CompletenessKind, TranscriptSummary, seq_of_name,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};

/// How many of the highest keys are examined for a valid header when the writer resumes.
const RESUME_CANDIDATES: usize = 8;
/// How many bytes of an object are fetched to read its header.
const HEAD_BYTES: usize = 8 * 1024;
/// One put, one list and one head give up after this long.
const PUT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(8);
/// The processor waits this long for the resume answer before it numbers its first record.
const RESUME_WAIT: Duration = Duration::from_secs(10);
/// A batch is cut before it passes this many bytes of lines: the reader's body limit is 1 MiB.
const HARD_BODY: usize = 900 * 1024;
/// The most a retry pause grows to.
const MAX_PAUSE: Duration = Duration::from_secs(5);
/// A batch meets at most this many foreign keys before it is given up.
const MAX_CONFLICTS: u32 = 64;

/// What a writer needs to know.
pub struct WriterConfig {
    pub run_id: String,
    /// The engine's display label and the harness protocol version of the writer: two labels of each header.
    pub engine: String,
    pub protocol: String,
    /// A ULID made once per writer: the identity of its records is `(incarnation, n)`.
    pub incarnation: String,
    pub settings: Settings,
    /// Exact values to hide besides the shapes (the session credentials).
    pub secrets: Vec<String>,
}

impl std::fmt::Debug for WriterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the secrets are not printed
        f.debug_struct("WriterConfig")
            .field("run_id", &self.run_id)
            .field("incarnation", &self.incarnation)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

enum Msg {
    Wire {
        dir: Dir,
        value: Value,
        stale: bool,
        at: i64,
        size: usize,
    },
    Shed,
    Flush,
    Final(oneshot::Sender<()>),
}

/// A cut of the conversation waiting for upload.
struct Batch {
    lines: Vec<Line>,
    bytes: usize,
    closed: bool,
}

impl Batch {
    fn first(&self) -> u64 {
        self.lines.first().map_or(0, |l| l.n)
    }
    fn last(&self) -> u64 {
        self.lines.last().map_or(0, |l| l.n)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LostKind {
    Failed,
    Shed,
}

#[derive(Clone, Copy, Debug)]
struct Lost {
    from: u64,
    to: u64,
    kind: LostKind,
}

#[derive(Clone, Debug)]
struct Resume {
    next_seq: u64,
    /// The highest valid object found and whether it said it was closed.
    prior: Option<(u64, bool)>,
}

#[derive(Default)]
struct Upload {
    next_seq: u64,
    last_ok: Option<u64>,
    /// Ranges of record numbers that are not in any stored object and not yet named by one.
    lost: Vec<Lost>,
    failed_records: u64,
    shed_records: u64,
    /// Key numbers that were taken by someone else, merged into runs.
    conflicts: Vec<(u64, u64)>,
    prior: Option<(u64, bool)>,
    /// The batch being uploaded, for the final accounting.
    inflight: Option<(u64, u64)>,
}

struct Shared {
    settings: Settings,
    run_id: String,
    engine: String,
    protocol: String,
    incarnation: String,
    /// Bytes in the capture queue and in the waiting objects (an object in flight counts until it is stored or given up).
    queued: AtomicUsize,
    peak: AtomicUsize,
    shed_pending: AtomicU64,
    shed_marked: AtomicBool,
    /// Messages whose tool bodies were dropped under load.
    thinned: AtomicU64,
    redactor: Mutex<Arc<Redactor>>,
    /// The run's own secret set: every value in it is a value to hide, whenever it was minted. `followed_len` is how many it held when the
    /// redactor last took them over, so the redactor is rebuilt only when the set has grown.
    followed: Mutex<Option<SecretSet>>,
    followed_len: AtomicUsize,
    stats: Mutex<Stats>,
    up: Mutex<Upload>,
    outbox: Mutex<VecDeque<Batch>>,
    wake: tokio::sync::Notify,
    /// Bumped when an object is stored or given up, so `finish` waits without missing it.
    progress: watch::Sender<u64>,
    resume: watch::Sender<Option<Resume>>,
    store: Arc<dyn ChunkStore>,
}

impl Shared {
    /// The redactor to use for the next message: the one this writer has, after it took over what the followed secret set gained since the last
    /// look (nothing is read when the set has not grown).
    fn current_redactor(&self) -> Arc<Redactor> {
        let followed = self
            .followed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(set) = followed
            && set.len() != self.followed_len.load(Ordering::SeqCst)
        {
            let values = set.snapshot();
            let mut g = self.redactor.lock().unwrap_or_else(|p| p.into_inner());
            let mut next = (**g).clone();
            for v in &values {
                next.add_secret(v);
            }
            *g = Arc::new(next);
            self.followed_len.store(values.len(), Ordering::SeqCst);
        }
        self.redactor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn add_queued(&self, n: usize) {
        let now = self.queued.fetch_add(n, Ordering::SeqCst) + n;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    fn sub_queued(&self, n: usize) {
        let _ = self
            .queued
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |q| {
                Some(q.saturating_sub(n))
            });
    }

    fn bump(&self) {
        self.progress.send_modify(|v| *v += 1);
    }

    fn key(&self, seq: u64) -> String {
        transcript_chunk_key(&self.run_id, seq)
    }

    fn prefix(&self) -> String {
        transcript_prefix(&self.run_id)
    }

    fn lost_batch(&self, first: u64, last: u64, count: u64, kind: LostKind) {
        let mut up = self.up.lock().unwrap_or_else(|p| p.into_inner());
        match kind {
            LostKind::Failed => up.failed_records += count,
            LostKind::Shed => up.shed_records += count,
        }
        // Adjacent ranges of one kind are one range.
        if let Some(l) = up.lost.last_mut()
            && l.kind == kind
            && l.to + 1 >= first
        {
            l.to = l.to.max(last);
            return;
        }
        up.lost.push(Lost {
            from: first,
            to: last,
            kind,
        });
    }
}

/// A message of at most this many bytes is admitted a little past the bound (a prompt's answer must not be the one that is dropped).
const SMALL_MESSAGE: usize = 8 * 1024;
const RESERVE: usize = 256 * 1024;

/// A rough size of a JSON value in bytes, without serializing it.
fn approx_size(v: &Value) -> usize {
    let mut total = 0usize;
    let mut stack = vec![v];
    while let Some(x) = stack.pop() {
        total += 8;
        match x {
            Value::String(s) => total += s.len(),
            Value::Array(a) => stack.extend(a.iter()),
            Value::Object(m) => {
                for (k, y) in m {
                    total += k.len() + 4;
                    stack.push(y);
                }
            }
            _ => {}
        }
    }
    total
}

/// The same message with the bodies of a tool call replaced by a note, or `None` when it is no tool call.
fn thin(v: &Value) -> Option<Value> {
    let kind = v
        .pointer("/params/update/sessionUpdate")
        .and_then(Value::as_str)?;
    if !matches!(kind, "tool_call" | "tool_call_update") {
        return None;
    }
    let mut out = v.clone();
    let upd = out.pointer_mut("/params/update")?.as_object_mut()?;
    for k in ["rawInput", "rawOutput", "content"] {
        if upd.contains_key(k) {
            upd.insert(k.to_string(), json!({"truncated": {"reason": "queue"}}));
        }
    }
    Some(out)
}

/// The transcript writer of one run.
pub struct Writer {
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    finished: tokio::sync::Mutex<Option<TranscriptSummary>>,
}

impl Writer {
    /// Starts the two tasks. Call it inside a tokio runtime.
    pub fn start(cfg: WriterConfig, store: Arc<dyn ChunkStore>) -> Writer {
        let (tx, rx) = mpsc::unbounded_channel();
        let (progress, _) = watch::channel(0u64);
        let (resume, _) = watch::channel(None);
        let shared = Arc::new(Shared {
            settings: cfg.settings,
            run_id: cfg.run_id,
            engine: cfg.engine,
            protocol: cfg.protocol,
            incarnation: cfg.incarnation,
            queued: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            shed_pending: AtomicU64::new(0),
            shed_marked: AtomicBool::new(false),
            thinned: AtomicU64::new(0),
            redactor: Mutex::new(Arc::new(Redactor::new(cfg.secrets))),
            followed: Mutex::new(None),
            followed_len: AtomicUsize::new(0),
            stats: Mutex::new(Stats::default()),
            up: Mutex::new(Upload::default()),
            outbox: Mutex::new(VecDeque::new()),
            wake: tokio::sync::Notify::new(),
            progress,
            resume,
            store,
        });
        let uploader = tokio::spawn(run_uploader(shared.clone()));
        let processor = tokio::spawn(run_processor(shared.clone(), rx));
        Writer {
            tx,
            shared,
            tasks: Mutex::new(vec![uploader, processor]),
            finished: tokio::sync::Mutex::new(None),
        }
    }

    /// Queues one message of the engine connection and returns at once. When the queue is full a tool call loses its bodies, and then
    /// any message is dropped and counted.
    pub fn push(&self, dir: Dir, msg: &Value, stale: bool) {
        let s = &self.shared;
        let size = approx_size(msg);
        let queued = s.queued.load(Ordering::SeqCst);
        let limit = |n: usize| {
            if n <= SMALL_MESSAGE {
                s.settings.queue_bytes + RESERVE
            } else {
                s.settings.queue_bytes
            }
        };
        let (value, size) = if queued.saturating_add(size) > limit(size) {
            match thin(msg) {
                Some(v) if queued.saturating_add(approx_size(&v)) <= limit(approx_size(&v)) => {
                    let sz = approx_size(&v);
                    s.thinned.fetch_add(1, Ordering::SeqCst);
                    (v, sz)
                }
                _ => {
                    s.shed_pending.fetch_add(1, Ordering::SeqCst);
                    if !s.shed_marked.swap(true, Ordering::SeqCst) {
                        let _ = self.tx.send(Msg::Shed);
                    }
                    return;
                }
            }
        } else {
            (msg.clone(), size)
        };
        s.add_queued(size);
        if self
            .tx
            .send(Msg::Wire {
                dir,
                value,
                stale,
                at: now_unix(),
                size,
            })
            .is_err()
        {
            s.sub_queued(size);
        }
    }

    /// Hides every value of the run's secret set, as it is when each message is processed. A value minted by a refresh inside the
    /// keeper's 60 second window, or one the keeper never reads (the refresh nonce, the package token), is hidden from the next message on.
    pub fn follow_secrets(&self, set: SecretSet) {
        *self
            .shared
            .followed
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(set);
    }

    /// More exact values to hide (the session credentials rotate).
    pub fn add_secrets(&self, secrets: &[String]) {
        let mut g = self
            .shared
            .redactor
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut next = (**g).clone();
        for s in secrets {
            next.add_secret(s);
        }
        *g = Arc::new(next);
    }

    /// Writes what is pending now, without waiting.
    pub fn flush_now(&self) {
        let _ = self.tx.send(Msg::Flush);
    }

    /// The most bytes the capture queue and the waiting objects held together.
    pub fn queued_peak(&self) -> usize {
        self.shared.peak.load(Ordering::SeqCst)
    }

    /// What the pipeline left out, cut or lost so far.
    pub fn stats(&self) -> Stats {
        self.shared
            .stats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Stops both tasks at once, without a final flush: the microVM is gone.
    pub fn abort(&self) {
        for t in self.tasks.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            t.abort();
        }
    }

    /// The summary of what the writer holds and lost, as of now.
    pub fn summary(&self) -> TranscriptSummary {
        summarize(&self.shared)
    }

    /// The final flush: everything queued is written with an end record, in an object that says it is closed, within the settings'
    /// deadline. What could not be stored by then is counted as failed. Asking twice gives the first answer.
    pub async fn finish(&self) -> TranscriptSummary {
        let mut done = self.finished.lock().await;
        if let Some(s) = done.as_ref() {
            return s.clone();
        }
        let deadline = Instant::now() + self.shared.settings.final_deadline;
        let (ack, acked) = oneshot::channel();
        if self.tx.send(Msg::Final(ack)).is_ok() {
            let _ = tokio::time::timeout_at(deadline, acked).await;
        }
        let mut progress = self.shared.progress.subscribe();
        loop {
            if idle(&self.shared) {
                break;
            }
            tokio::select! {
                changed = progress.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                () = sleep_until(deadline) => break,
            }
        }
        if !idle(&self.shared) {
            // The deadline passed: what is still waiting or in flight is not stored.
            self.abort();
            give_up(&self.shared);
        }
        self.abort();
        let s = summarize(&self.shared);
        *done = Some(s.clone());
        s
    }
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("run_id", &self.shared.run_id)
            .field("incarnation", &self.shared.incarnation)
            .finish_non_exhaustive()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.abort();
    }
}

fn idle(s: &Shared) -> bool {
    // The uploader takes a batch and marks it in flight under this lock, so the two facts are read together.
    let ob = s.outbox.lock().unwrap_or_else(|p| p.into_inner());
    ob.is_empty()
        && s.up
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .inflight
            .is_none()
}

/// Counts every batch that is still waiting or in flight as failed.
fn give_up(s: &Shared) {
    let waiting: Vec<Batch> = s
        .outbox
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain(..)
        .collect();
    for b in waiting {
        s.lost_batch(b.first(), b.last(), b.lines.len() as u64, LostKind::Failed);
        s.sub_queued(b.bytes);
    }
    let inflight =
        s.up.lock()
            .unwrap_or_else(|p| p.into_inner())
            .inflight
            .take();
    if let Some((first, last)) = inflight {
        s.lost_batch(first, last, last - first + 1, LostKind::Failed);
    }
}

fn summarize(s: &Shared) -> TranscriptSummary {
    let stats = s.stats.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let up = s.up.lock().unwrap_or_else(|p| p.into_inner());
    let mut entries = Vec::new();
    let count = |kind: CompletenessKind, n: u64| CompletenessEntry {
        kind,
        from_seq: None,
        to_seq: None,
        count: Some(n),
    };
    if up.failed_records > 0 {
        entries.push(count(CompletenessKind::Failed, up.failed_records));
    }
    let shed = stats.shed + up.shed_records + s.thinned.load(Ordering::SeqCst);
    if shed > 0 {
        entries.push(count(CompletenessKind::Shed, shed));
    }
    if stats.suppressed > 0 {
        entries.push(count(CompletenessKind::Suppressed, stats.suppressed));
    }
    for (from, to) in &up.conflicts {
        entries.push(CompletenessEntry {
            kind: CompletenessKind::Conflict,
            from_seq: Some(*from),
            to_seq: Some(*to),
            count: Some(to - from + 1),
        });
    }
    if let Some((seq, false)) = up.prior {
        entries.push(CompletenessEntry {
            kind: CompletenessKind::TailUnknown,
            from_seq: Some(seq),
            to_seq: Some(seq),
            count: None,
        });
    }
    if stats.truncated > 0 {
        entries.push(count(CompletenessKind::Truncated, stats.truncated));
    }
    if stats.excluded > 0 {
        entries.push(count(CompletenessKind::Excluded, stats.excluded));
    }
    // A summary stays inside the protocol's bound: the many small entries of conflicts fold into one range.
    while entries.len() > harness_protocol::transcript::MAX_COMPLETENESS_ENTRIES {
        let Some(i) = entries
            .iter()
            .rposition(|e| e.kind == CompletenessKind::Conflict)
        else {
            break;
        };
        entries.remove(i);
    }
    TranscriptSummary {
        last_transcript_seq: up.last_ok.or(up.prior.map(|p| p.0)),
        entries,
    }
}

// ---- the processor ---------------------------------------------------------------------------------------------------------------

/// Lines gathered for the next object.
struct Gather {
    lines: Vec<Line>,
    bytes: usize,
    started: Option<Instant>,
}

impl Gather {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            bytes: 0,
            started: None,
        }
    }
}

fn cut(s: &Shared, g: &mut Gather, closed: bool) {
    if g.lines.is_empty() && !closed {
        return;
    }
    let lines = std::mem::take(&mut g.lines);
    let batch = Batch {
        lines,
        bytes: std::mem::take(&mut g.bytes),
        closed,
    };
    g.started = None;
    s.add_queued(batch.bytes);
    let mut dropped: Vec<Batch> = Vec::new();
    {
        let mut ob = s.outbox.lock().unwrap_or_else(|p| p.into_inner());
        ob.push_back(batch);
        // Past the bound the oldest waiting objects go, but never the newest and never the one that closes the run.
        while s.queued.load(Ordering::SeqCst) > s.settings.queue_bytes && ob.len() > 1 {
            let Some(old) = ob.pop_front() else { break };
            s.sub_queued(old.bytes);
            dropped.push(old);
        }
    }
    for b in dropped {
        s.lost_batch(b.first(), b.last(), b.lines.len() as u64, LostKind::Shed);
    }
    s.wake.notify_one();
}

fn add_lines(s: &Shared, g: &mut Gather, lines: Vec<Line>, now: Instant) {
    for line in lines {
        let size = line.bytes.len() + 1;
        if !g.lines.is_empty() && (g.bytes + size > HARD_BODY || g.lines.len() >= MAX_RECORDS - 16)
        {
            cut(s, g, false);
        }
        if g.lines.is_empty() {
            g.started = Some(now);
        }
        let ends_object = matches!(line.kind.as_str(), "prompt_result" | "cancel");
        g.bytes += size;
        g.lines.push(line);
        if ends_object || g.bytes >= s.settings.flush_bytes {
            cut(s, g, false);
        }
    }
}

async fn run_processor(s: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut pipeline = Pipeline::new(s.settings.clone(), Instant::now().into_std());
    let mut g = Gather::new();
    // The first record is numbered after the uploader has looked at what is already in the prefix.
    let mut resumed = s.resume.subscribe();
    let resume = match timeout(RESUME_WAIT, resumed.wait_for(|r| r.is_some())).await {
        Ok(Ok(r)) => r.clone(),
        _ => None,
    };
    if let Some(Resume {
        prior: Some((seq, false)),
        ..
    }) = resume
    {
        let line = pipeline.gap(
            now_unix(),
            json!({"kind": "tail-unknown", "afterSeq": seq, "reason": "a previous writer ended without a final flush"}),
        );
        add_lines(&s, &mut g, vec![line], Instant::now());
    }
    loop {
        let wake_at = [
            pipeline.deadline().map(Instant::from_std),
            g.started.map(|t| t + s.settings.flush_every),
        ]
        .into_iter()
        .flatten()
        .min();
        let msg = tokio::select! {
            m = rx.recv() => m,
            () = async {
                match wake_at {
                    Some(t) => sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = Instant::now();
                let r = s.current_redactor();
                let lines = pipeline.tick(&r, now.into_std());
                add_lines(&s, &mut g, lines, now);
                if g.started.is_some_and(|t| now >= t + s.settings.flush_every) {
                    cut(&s, &mut g, false);
                }
                *s.stats.lock().unwrap_or_else(|p| p.into_inner()) = pipeline.stats().clone();
                continue;
            }
        };
        let Some(msg) = msg else { break };
        let now = Instant::now();
        let r = s.current_redactor();
        match msg {
            Msg::Wire {
                dir,
                value,
                stale,
                at,
                size,
            } => {
                s.sub_queued(size);
                let lines = pipeline.feed(
                    &r,
                    now.into_std(),
                    WireMsg {
                        dir,
                        value,
                        stale,
                        at,
                    },
                );
                add_lines(&s, &mut g, lines, now);
            }
            Msg::Shed => {
                let n = s.shed_pending.swap(0, Ordering::SeqCst);
                s.shed_marked.store(false, Ordering::SeqCst);
                let lines = pipeline.note_shed(&r, n, "queue", now_unix());
                add_lines(&s, &mut g, lines, now);
            }
            Msg::Flush => {
                let lines = pipeline.flush(&r, now.into_std());
                add_lines(&s, &mut g, lines, now);
                cut(&s, &mut g, false);
            }
            Msg::Final(ack) => {
                let lines = pipeline.finish(&r);
                add_lines(&s, &mut g, lines, now);
                let end = pipeline.end(now_unix());
                if g.lines.is_empty() {
                    g.started = Some(now);
                }
                g.bytes += end.bytes.len() + 1;
                g.lines.push(end);
                *s.stats.lock().unwrap_or_else(|p| p.into_inner()) = pipeline.stats().clone();
                cut(&s, &mut g, true);
                let _ = ack.send(());
                break;
            }
        }
        *s.stats.lock().unwrap_or_else(|p| p.into_inner()) = pipeline.stats().clone();
    }
}

// ---- the uploader ----------------------------------------------------------------------------------------------------------------

async fn resolve(s: &Shared) -> Resume {
    let prefix = s.prefix();
    let listed = match timeout(LIST_TIMEOUT, s.store.list(&prefix, None)).await {
        Ok(Ok(keys)) => keys,
        _ => {
            return Resume {
                next_seq: 0,
                prior: None,
            };
        }
    };
    let mut seqs: Vec<u64> = listed
        .iter()
        .filter_map(|k| seq_of_name(k.rsplit('/').next()?))
        .collect();
    seqs.sort_unstable_by(|a, b| b.cmp(a));
    for seq in seqs.into_iter().take(RESUME_CANDIDATES) {
        let key = s.key(seq);
        if let Ok(Ok(bytes)) = timeout(LIST_TIMEOUT, s.store.head(&key, HEAD_BYTES)).await
            && let Some(h) = decode_header(&bytes)
            && h.seq == seq
        {
            return Resume {
                next_seq: seq + 1,
                prior: Some((seq, h.closed)),
            };
        }
    }
    Resume {
        next_seq: 0,
        prior: None,
    }
}

/// The first number after the run of keys that follow `seq` without a gap (all of them taken by someone else).
async fn after_run(s: &Shared, seq: u64) -> u64 {
    let start = s.key(seq);
    let mut next = seq + 1;
    if let Ok(Ok(keys)) = timeout(LIST_TIMEOUT, s.store.list(&s.prefix(), Some(&start))).await {
        let mut found: Vec<u64> = keys
            .iter()
            .filter_map(|k| seq_of_name(k.rsplit('/').next()?))
            .collect();
        found.sort_unstable();
        for x in found {
            if x == next {
                next += 1;
            } else if x > next {
                break;
            }
        }
    }
    next
}

fn note_conflict(up: &mut Upload, from: u64, to: u64) {
    if let Some(last) = up.conflicts.last_mut()
        && last.1 + 1 >= from
    {
        last.1 = last.1.max(to);
        return;
    }
    up.conflicts.push((from, to));
}

/// The gap records that name what is lost, in front of the next object's own lines.
fn gap_lines(lost: &[Lost], at: i64) -> Vec<Line> {
    lost.iter()
        .map(|l| {
            let count = l.to - l.from + 1;
            let (kind, reason) = match l.kind {
                LostKind::Failed => ("failed", "storage"),
                LostKind::Shed => ("shed", "queue"),
            };
            gap_line(
                l.from,
                at,
                json!({"kind": kind, "count": count, "reason": reason}),
            )
        })
        .collect()
}

enum Stored {
    Yes,
    No,
}

async fn upload(s: &Shared, batch: &Batch) -> Stored {
    let at = now_unix();
    let mut lines: Vec<Line> = Vec::new();
    let carried: Vec<Lost> = s.up.lock().unwrap_or_else(|p| p.into_inner()).lost.clone();
    lines.extend(gap_lines(&carried, at));
    lines.extend(batch.lines.iter().cloned());
    let first = lines.first().map_or(0, |l| l.n);
    let last = lines.last().map_or(0, |l| l.n);
    let mut tries = 0u32;
    let mut conflicts = 0u32;
    loop {
        let seq = s.up.lock().unwrap_or_else(|p| p.into_inner()).next_seq;
        let header = Header {
            seq,
            incarnation: s.incarnation.clone(),
            first_rec: first,
            last_rec: last,
            created_at: format_rfc3339(now_unix()),
            engine: s.engine.clone(),
            protocol: s.protocol.clone(),
            closed: batch.closed,
        };
        let Ok(body) = encode(&header, &lines) else {
            return Stored::No;
        };
        let key = s.key(seq);
        let outcome = match timeout(PUT_TIMEOUT, s.store.put_new(&key, body)).await {
            Ok(o) => o,
            Err(_) => PutOutcome::Failed("the put timed out".into()),
        };
        match outcome {
            PutOutcome::Stored => {
                let mut up = s.up.lock().unwrap_or_else(|p| p.into_inner());
                up.next_seq = seq + 1;
                up.last_ok = Some(seq);
                up.lost.clear();
                return Stored::Yes;
            }
            PutOutcome::Exists => {
                // Our own write whose answer was lost, or someone else's key?
                let own = match timeout(LIST_TIMEOUT, s.store.head(&key, HEAD_BYTES)).await {
                    Ok(Ok(bytes)) => decode_header(&bytes).is_some_and(|h| {
                        h.seq == seq
                            && h.incarnation == s.incarnation
                            && h.first_rec == first
                            && h.last_rec == last
                    }),
                    _ => false,
                };
                if own {
                    let mut up = s.up.lock().unwrap_or_else(|p| p.into_inner());
                    up.next_seq = seq + 1;
                    up.last_ok = Some(seq);
                    up.lost.clear();
                    return Stored::Yes;
                }
                conflicts += 1;
                let next = after_run(s, seq).await;
                {
                    let mut up = s.up.lock().unwrap_or_else(|p| p.into_inner());
                    note_conflict(&mut up, seq, next - 1);
                    up.next_seq = next;
                }
                if conflicts >= MAX_CONFLICTS {
                    return Stored::No;
                }
            }
            PutOutcome::Failed(_) => {
                tries += 1;
                if tries >= s.settings.upload_tries {
                    return Stored::No;
                }
                let pause = (s.settings.retry_base * 2u32.saturating_pow(tries - 1)).min(MAX_PAUSE);
                tokio::time::sleep(pause).await;
            }
        }
    }
}

async fn run_uploader(s: Arc<Shared>) {
    let resume = resolve(&s).await;
    {
        let mut up = s.up.lock().unwrap_or_else(|p| p.into_inner());
        up.next_seq = resume.next_seq;
        up.prior = resume.prior;
    }
    s.resume.send_replace(Some(resume));
    loop {
        let next = {
            let mut ob = s.outbox.lock().unwrap_or_else(|p| p.into_inner());
            let b = ob.pop_front();
            if let Some(b) = &b {
                s.up.lock().unwrap_or_else(|p| p.into_inner()).inflight =
                    Some((b.first(), b.last()));
            }
            b
        };
        let Some(batch) = next else {
            s.wake.notified().await;
            continue;
        };
        let result = upload(&s, &batch).await;
        {
            let mut up = s.up.lock().unwrap_or_else(|p| p.into_inner());
            up.inflight = None;
        }
        if matches!(result, Stored::No) {
            s.lost_batch(
                batch.first(),
                batch.last(),
                batch.lines.len() as u64,
                LostKind::Failed,
            );
        }
        s.sub_queued(batch.bytes);
        s.bump();
    }
}
