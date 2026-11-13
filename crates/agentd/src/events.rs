//! Agent events: envelopes with `authority=agent`, batched to `/internal/events` every
//! 2 s with a heartbeat every 60 s. A failed post is logged and never fails the turn.

use crate::BoxFuture;
use crate::creds::CredStore;
use crate::http::{SignedHttp, SignedRequest};
use crate::logging::LogSink;
use crate::time::{format_rfc3339, new_ulid, now_unix};
use harness_protocol::api::internal::{
    EventsBatchRequest, EventsBatchResponse, HeartbeatRequest, PendingControl, TurnCompleteRequest,
};
use harness_protocol::direct::{InstructAction, InstructOrigin};
use harness_protocol::envelope::{
    Authority, Envelope, EventType, MAX_INLINE_PAYLOAD_BYTES, Payload, SCHEMA_VERSION,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// Events kept while the internal API is unreachable; older ones are dropped first.
const MAX_BUFFERED: usize = 2000;

pub trait EventSink: Send + Sync {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>>;
    fn heartbeat<'a>(&'a self, req: HeartbeatRequest) -> BoxFuture<'a, Result<(), String>>;
}

/// The private internal API, signed with the session credentials.
#[derive(Debug)]
pub struct HttpEventSink {
    http: SignedHttp,
    creds: Arc<CredStore>,
    base_url: String,
    region: String,
    /// The run's completion key: sent as a header on `/internal/events` and `/internal/turn-complete`, the two routes
    /// worker-api guards with it. The engine holds the same session credentials and never this key. Never printed.
    completion_key: Option<String>,
}

impl HttpEventSink {
    pub fn new(creds: Arc<CredStore>, base_url: String, region: String) -> Self {
        Self {
            http: SignedHttp::new(),
            creds,
            base_url,
            region,
            completion_key: None,
        }
    }

    /// The sink with the run's completion key.
    #[must_use]
    pub fn with_completion_key(mut self, key: String) -> Self {
        self.completion_key = Some(key);
        self
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<Vec<u8>, String> {
        let creds = self.creds.current().await.map_err(|e| e.to_string())?;
        let url = format!("{}{path}", self.base_url.trim_end_matches('/'));
        let guarded = matches!(path, "/internal/events" | "/internal/turn-complete");
        let key_header = self
            .completion_key
            .as_deref()
            .filter(|_| guarded)
            .map(|k| [(harness_protocol::store::COMPLETION_KEY_HEADER, k)]);
        let resp = self
            .http
            .send_with_headers(
                SignedRequest {
                    method: "POST",
                    url: &url,
                    service: "execute-api",
                    region: &self.region,
                    creds: &creds,
                    content_type: Some("application/json"),
                    body,
                },
                key_header.as_ref().map_or(&[][..], |h| &h[..]),
            )
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(format!("{path} returned {}", resp.status));
        }
        Ok(resp.body)
    }
}

impl HttpEventSink {
    /// `POST /internal/turn-complete`: the turn's facts, no verdict.
    pub async fn turn_complete(&self, req: &TurnCompleteRequest) -> Result<(), String> {
        let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
        self.post("/internal/turn-complete", body).await.map(|_| ())
    }
}

impl EventSink for HttpEventSink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&batch).map_err(|e| e.to_string())?;
            let bytes = self.post("/internal/events", body).await?;
            serde_json::from_slice(&bytes)
                .map_err(|_| "events response is not an EventsBatchResponse".to_string())
        })
    }

    fn heartbeat<'a>(&'a self, req: HeartbeatRequest) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
            self.post("/internal/heartbeat", body).await.map(|_| ())
        })
    }
}

/// Builds and buffers this agent's envelopes.
#[derive(Debug)]
pub struct EventRecorder {
    run_id: String,
    agent_id: String,
    /// The buffer and the numbers that must follow its order: an event gets its `seq` and, when this recorder names it, its id inside
    /// the critical section that appends it, so the order of the buffer, of `seq` and of the ids is one order.
    buf: Mutex<Buffer>,
    /// Makes the id of an event this recorder names itself ([`new_ulid`] unless a test gives another).
    mint: fn() -> String,
    /// Woken by [`EventRecorder::record_urgent`]: the flusher posts without waiting for its tick.
    nudge: Notify,
}

impl EventRecorder {
    pub fn new(run_id: &str, agent_id: &str) -> Arc<Self> {
        Self::with_mint(run_id, agent_id, new_ulid)
    }

    /// A recorder whose own event ids come from `mint` (tests give a frozen clock and a descending random source).
    pub fn with_mint(run_id: &str, agent_id: &str, mint: fn() -> String) -> Arc<Self> {
        Arc::new(Self {
            run_id: run_id.into(),
            agent_id: agent_id.into(),
            buf: Mutex::new(Buffer::default()),
            mint,
            nudge: Notify::new(),
        })
    }

    /// Records an event under an id made inside the append: the ids of one recorder grow in the order of the appends, so the order
    /// of the event log (sorted by id) is the order the events happened in.
    pub fn record(&self, event_type: EventType, value: Value) {
        self.push(event_type, None, |_| value);
    }

    /// [`record`](Self::record) for an event whose payload names its own id (a question: its event id IS the request id). The id is
    /// made inside the append and handed to `build`, which makes the payload, so no event can take a place between the id and the
    /// event. Flushed at once, like [`record_urgent`](Self::record_urgent). Returns the id.
    pub fn record_urgent_minted(
        &self,
        event_type: EventType,
        build: impl FnOnce(&str) -> Value,
    ) -> String {
        let id = self.push(event_type, None, build);
        self.nudge.notify_one();
        id
    }

    /// Appends one event and returns its id: `id` when the caller made it, else one made here, inside the lock.
    fn push(
        &self,
        event_type: EventType,
        id: Option<&str>,
        build: impl FnOnce(&str) -> Value,
    ) -> String {
        let mut buf = self.buf.lock().unwrap_or_else(|p| p.into_inner());
        let event_id = id.map_or_else(|| (self.mint)(), str::to_string);
        if id.is_none() {
            debug_assert!(
                buf.minted_tail.is_empty() || event_id > buf.minted_tail,
                "event id {event_id} does not sort after the newest id this recorder made ({})",
                buf.minted_tail
            );
            buf.minted_tail.clone_from(&event_id);
        }
        let value = build(&event_id);
        let bytes = serde_json::to_vec(&value).map_or(0, |v| v.len());
        let value = if bytes > MAX_INLINE_PAYLOAD_BYTES {
            json!({ "truncated": true, "bytes": bytes })
        } else {
            value
        };
        buf.seq += 1;
        let e = Envelope {
            schema_version: SCHEMA_VERSION,
            event_id: event_id.clone(),
            run_id: self.run_id.clone(),
            task_id: None,
            agent_id: Some(self.agent_id.clone()),
            seq: buf.seq,
            ts: format_rfc3339(now_unix()),
            event_type,
            authority: Authority::Agent,
            payload: Payload::Inline { value },
        };
        buf.events.push(e);
        let excess = buf.events.len().saturating_sub(MAX_BUFFERED);
        buf.events.drain(..excess);
        event_id
    }

    /// [`record`](Self::record), then asks the flusher to post what is buffered at once instead of at its
    /// next tick. For the one event a lost microVM must not take with it: `turn.started` carries the turn's
    /// rules digest, and a turn that never completes posts nothing after it.
    pub fn record_urgent(&self, event_type: EventType, value: Value) {
        self.record(event_type, value);
        self.nudge.notify_one();
    }

    /// [`record_urgent`](Self::record_urgent) with the event id given instead of made: the id of a `needs-input` event IS the id
    /// of the question the user answers, so agentd has to know it before the event exists. `event_id` is a ULID the
    /// caller made (`crate::time::new_ulid`).
    pub fn record_urgent_with_id(&self, event_type: EventType, event_id: &str, value: Value) {
        self.push(event_type, Some(event_id), |_| value);
        self.nudge.notify_one();
    }

    pub fn drain(&self) -> Vec<Envelope> {
        std::mem::take(&mut self.buf.lock().unwrap_or_else(|p| p.into_inner()).events)
    }

    fn requeue(&self, mut events: Vec<Envelope>) {
        let mut buf = self.buf.lock().unwrap_or_else(|p| p.into_inner());
        events.append(&mut buf.events);
        let excess = events.len().saturating_sub(MAX_BUFFERED);
        events.drain(..excess);
        buf.events = events;
    }
}

/// The events not yet posted and the counters that follow their order.
#[derive(Debug, Default)]
struct Buffer {
    events: Vec<Envelope>,
    /// The `seq` of the newest event.
    seq: u64,
    /// The id of the newest event whose id this recorder made (an id a caller gave is not tracked).
    minted_tail: String,
}

#[derive(Debug)]
pub struct Flusher {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<Vec<PendingControl>>,
}

impl Flusher {
    /// Posts what is left and returns every pending control the internal API sent.
    pub async fn stop(self) -> Vec<PendingControl> {
        let _ = self.stop.send(());
        self.handle.await.unwrap_or_default()
    }
}

/// How many unread instructions a run holds for its driver; a fuller channel drops the surplus (the flusher logs it).
pub const INSTRUCTION_CHANNEL_CAPACITY: usize = 16;

/// One answer from the user (later also the analyzer) to a `needs-input` question, as the driver receives it: the internal API's
/// `Instruct` control. The driver checks it against the open window (id, expiry, action offered, model among
/// the run's routes, text rules, session caps) and acknowledges it with `input-received` or `input-ignored`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruct {
    /// The question this answers; also the control's id, so a repeated answer is a no-op.
    pub request_id: String,
    pub action: InstructAction,
    pub text: Option<String>,
    /// `try_model` only.
    pub model: Option<String>,
    pub origin: InstructOrigin,
    /// A `Cancel` came in the same batch: the driver does not act on this and acknowledges it `input-ignored` (`window-closed`);
    /// the run ends `stopped_by_user`.
    pub cancelled_in_batch: bool,
}

/// What [`TurnControls::deliver`] did with one batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeliverReport {
    /// The batch held a `Cancel`, so the cancel flag is up.
    pub cancel: bool,
    /// Instructions handed to the driver's channel.
    pub instructions: usize,
    /// Instructions dropped: the channel was full or its receiver was gone.
    pub dropped: usize,
}

/// Controls delivered while a run goes on. A `Cancel` raises the cancel signal that stops the running prompt (`ham stop`); an
/// `Instruct` goes to the instruction channel the driver takes with [`take_instructions`](Self::take_instructions).
#[derive(Debug)]
pub struct TurnControls {
    cancel: watch::Sender<bool>,
    instructions: mpsc::Sender<Instruct>,
    /// The receiving end until the driver takes it.
    instructions_rx: Mutex<Option<mpsc::Receiver<Instruct>>>,
    /// The modules of the run (`crate::direct::hooks`): every batch of controls is also shown to them. Set once, before the flusher starts.
    hooks: std::sync::OnceLock<crate::direct::hooks::Hooks>,
}

impl TurnControls {
    /// Controls plus the receiver that cancels the running `session/prompt`.
    pub fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        (Arc::new(Self::from_sender(tx)), rx)
    }

    pub fn from_sender(cancel: watch::Sender<bool>) -> Self {
        let (instructions, rx) = mpsc::channel(INSTRUCTION_CHANNEL_CAPACITY);
        Self {
            cancel,
            instructions,
            instructions_rx: Mutex::new(Some(rx)),
            hooks: std::sync::OnceLock::new(),
        }
    }

    /// Shows every later batch of controls to these modules, whatever the kind. Once: a second call changes nothing.
    pub fn install_hooks(&self, hooks: &crate::direct::hooks::Hooks) {
        if !hooks.is_empty() {
            let _ = self.hooks.set(hooks.clone());
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    /// The instruction receiver, once: the driver that waits for a person owns it. `None` the second time.
    pub fn take_instructions(&self) -> Option<mpsc::Receiver<Instruct>> {
        self.instructions_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Acts on what the internal API sent back. A `Cancel` raises the cancel signal FIRST. An `Instruct` goes to the instruction
    /// channel in order, marked `cancelled_in_batch` when the same batch held a `Cancel`; a full channel or a gone receiver drops
    /// it and the report counts it. Any other control is ignored (a direct run takes no steer and asks for no approval).
    pub fn deliver(&self, controls: &[PendingControl]) -> DeliverReport {
        // send_replace: a control can arrive before the turn subscribes, and `send` drops the
        // value when no receiver exists yet.
        let cancel = controls.iter().any(|c| matches!(c, PendingControl::Cancel));
        if cancel {
            self.cancel.send_replace(true);
        }
        let mut report = DeliverReport {
            cancel,
            ..DeliverReport::default()
        };
        for control in controls {
            let PendingControl::Instruct {
                request_id,
                action,
                text,
                model,
                origin,
            } = control
            else {
                continue;
            };
            let instruct = Instruct {
                request_id: request_id.clone(),
                action: *action,
                text: text.clone(),
                model: model.clone(),
                origin: *origin,
                cancelled_in_batch: cancel,
            };
            match self.instructions.try_send(instruct) {
                Ok(()) => report.instructions += 1,
                Err(_) => report.dropped += 1,
            }
        }
        if !controls.is_empty()
            && let Some(h) = self.hooks.get()
        {
            h.controls(controls);
        }
        report
    }
}

async fn flush(
    rec: &EventRecorder,
    sink: &dyn EventSink,
    log: &dyn LogSink,
    pending: &mut Vec<PendingControl>,
    final_flush: bool,
    controls: &TurnControls,
) {
    let events = rec.drain();
    // An idle turn still polls, so controls arrive during long tool calls; the final
    // flush posts only recorded events, so it never takes a control meant for the next turn.
    if events.is_empty() && final_flush {
        return;
    }
    // A backlog goes out in posts of at most MAX_POST_BYTES, in order, so that no number of large events makes one post too big for the
    // internal API (a Lambda function takes 6 MB). With nothing recorded there is still one post, empty: it is how a turn polls for controls.
    let mut rest = events;
    loop {
        let take = post_len(&rest);
        let tail = rest.split_off(take);
        let sent = std::mem::replace(&mut rest, tail);
        let n = sent.len();
        let batch = EventsBatchRequest {
            run_id: rec.run_id.clone(),
            agent_id: rec.agent_id.clone(),
            events: sent.clone(),
            take_controls: final_flush.then_some(false),
        };
        match sink.post_events(batch).await {
            Ok(resp) => {
                let report = controls.deliver(&resp.pending);
                if report.dropped > 0 {
                    log.line(&format!(
                        "dropped {} instruction{}: the driver's channel is full or closed",
                        report.dropped,
                        if report.dropped == 1 { "" } else { "s" }
                    ));
                }
                pending.extend(resp.pending);
            }
            Err(e) => {
                log.line(&format!("events post failed ({n} events): {e}"));
                if !final_flush {
                    let mut unsent = sent;
                    unsent.append(&mut rest);
                    rec.requeue(unsent);
                }
                return;
            }
        }
        if rest.is_empty() {
            return;
        }
    }
}

/// The most bytes of events in one post.
const MAX_POST_BYTES: usize = 4 * 1024 * 1024;

/// How many of the first `events` go in one post: as many as fit [`MAX_POST_BYTES`], at least one.
fn post_len(events: &[Envelope]) -> usize {
    let mut bytes = 0;
    for (i, e) in events.iter().enumerate() {
        bytes += serde_json::to_vec(e).map_or(0, |v| v.len());
        if bytes > MAX_POST_BYTES {
            return i.max(1);
        }
    }
    events.len()
}

/// [`spawn_flusher_with`] for a bare cancel signal.
pub fn spawn_flusher(
    rec: Arc<EventRecorder>,
    sink: Arc<dyn EventSink>,
    every: Duration,
    heartbeat_every: Duration,
    turn: u32,
    log: Arc<dyn LogSink>,
    cancel: watch::Sender<bool>,
) -> Flusher {
    let controls = Arc::new(TurnControls::from_sender(cancel));
    spawn_flusher_with(rec, sink, every, heartbeat_every, turn, log, controls)
}

/// Posts events every `every` (an empty poll when idle) and heartbeats every `heartbeat_every`,
/// feeding each response's controls to `controls`.
pub fn spawn_flusher_with(
    rec: Arc<EventRecorder>,
    sink: Arc<dyn EventSink>,
    every: Duration,
    heartbeat_every: Duration,
    turn: u32,
    log: Arc<dyn LogSink>,
    controls: Arc<TurnControls>,
) -> Flusher {
    let (stop, mut stopped) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut pending = Vec::new();
        let mut tick = tokio::time::interval(every);
        let mut beat = tokio::time::interval(heartbeat_every);
        loop {
            tokio::select! {
                _ = &mut stopped => {
                    flush(&rec, sink.as_ref(), log.as_ref(), &mut pending, true, &controls).await;
                    return pending;
                }
                _ = tick.tick() => flush(&rec, sink.as_ref(), log.as_ref(), &mut pending, false, &controls).await,
                _ = rec.nudge.notified() => flush(&rec, sink.as_ref(), log.as_ref(), &mut pending, false, &controls).await,
                _ = beat.tick() => {
                    let req = HeartbeatRequest { run_id: rec.run_id.clone(), agent_id: rec.agent_id.clone(), turn };
                    if let Err(e) = sink.heartbeat(req).await {
                        log.line(&format!("heartbeat failed: {e}"));
                    }
                }
            }
        }
    });
    Flusher { stop, handle }
}
