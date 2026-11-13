//! The world of the whole-run message scenarios (package IM2): the fake AWS and the in-process agentd of `direct_support`, the fake Cline
//! engine (`FAKE_ACP_CLINE=1`), the module under test with its server model, the hand-out pump and the checks every scenario makes.
#![allow(dead_code, clippy::unwrap_used)]

use crate::common;
use crate::direct_support::{self, Direct, Opts, PROMPT};
use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks, StartupCtx};
use agentd::direct::messages::http::{HttpMessageStore, Transport};
use agentd::direct::messages::{
    ClaimOutcome, FailReason, MessageStore, Notice, Refusal, SealMode, SealOutcome,
};
use agentd::direct::messages::{MemoryStore, Message, MessagesHook, Mode, Settings};
use agentd::events::TurnControls;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::messages::{MessageMode, MessageNotice};
use harness_protocol::runtime::InvocationResponse;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// A module that keeps the run's controls, so a test can deliver the user's cancel the way the events response does.
#[derive(Default)]
pub struct Canceller {
    controls: Mutex<Option<Arc<TurnControls>>>,
}

impl Canceller {
    pub fn cancel(&self) {
        let c = self.controls.lock().unwrap().clone().unwrap();
        c.deliver(&[PendingControl::Cancel]);
    }
}

impl DirectHook for Canceller {
    fn name(&self) -> &'static str {
        "canceller"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.controls.lock().unwrap() = Some(ctx.env.controls.clone());
            Ok(())
        })
    }
}

pub struct World {
    pub d: Direct,
    pub store: Arc<MemoryStore>,
    pub hook: Arc<MessagesHook>,
    pub canceller: Arc<Canceller>,
    pub pids: PathBuf,
    pump: Mutex<Option<JoinHandle<()>>>,
}

pub fn cfg() -> Option<direct_support::CfgEdit> {
    Some(Box::new(|c: &mut agentd::config::Config| {
        c.direct.provider_retries = 3;
        c.direct.engine_stop_resumes = 1;
        c.direct.provider_retry_pause = Duration::from_millis(10);
        c.direct.input_margin = Duration::ZERO;
        c.direct.input_min_window = Duration::from_secs(1);
    }))
}

pub fn env(extra: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    let mut v = vec![("FAKE_ACP_CLINE", "1".to_string())];
    v.extend(extra.iter().map(|(k, x)| (*k, (*x).to_string())));
    v
}

impl World {
    pub async fn start(
        prompt: &str,
        extra_env: &[(&'static str, &str)],
        settings: Settings,
        pids: &Path,
    ) -> Self {
        Self::start_via(prompt, extra_env, settings, pids, false).await
    }

    /// As [`World::start`], with the module talking to the server model through the four routes of the internal API: the module's
    /// `HttpMessageStore`, a [`Plane`] that answers them from the model, and the hand-out as `PendingControl::Message` notices.
    pub async fn start_over_the_wire(
        prompt: &str,
        extra_env: &[(&'static str, &str)],
        settings: Settings,
        pids: &Path,
    ) -> Self {
        Self::start_via(prompt, extra_env, settings, pids, true).await
    }

    async fn start_via(
        prompt: &str,
        extra_env: &[(&'static str, &str)],
        settings: Settings,
        pids: &Path,
        wire: bool,
    ) -> Self {
        let d = Direct::start(Opts {
            prompt: prompt.to_string(),
            engine_env: env(extra_env),
            edit_cfg: cfg(),
            timeout_s: 120,
            ..Opts::default()
        })
        .await;
        let store = MemoryStore::new();
        let seen: Arc<dyn MessageStore> = if wire {
            Arc::new(HttpMessageStore::new(
                Arc::new(Plane::new(store.clone())),
                crate::direct_support::RUN,
                "direct",
                "01M5A0000000000000000000GE",
            ))
        } else {
            store.clone()
        };
        let hook = MessagesHook::new(seen, settings);
        Self {
            d,
            store,
            hook,
            canceller: Arc::new(Canceller::default()),
            pids: pids.to_path_buf(),
            pump: Mutex::new(None),
        }
    }

    pub fn run(&self) -> JoinHandle<InvocationResponse> {
        let deps = Deps {
            hooks: Some(Hooks::new(vec![
                self.canceller.clone() as Arc<dyn DirectHook>,
                self.hook.clone() as Arc<dyn DirectHook>,
            ])),
            ..Deps::default()
        };
        let (app, req) = (self.d.app.clone(), self.d.req.clone());
        tokio::spawn(async move { agentd::direct::run_with(&app, req, &deps).await })
    }

    /// The events response, every 100 ms: whatever is pending is handed out.
    pub fn pump(&self) {
        let (hook, store) = (self.hook.clone(), self.store.clone());
        *self.pump.lock().unwrap() = Some(tokio::spawn(async move {
            loop {
                // an events response: a `message` notice for each pending message of the current session, no text
                hook.on_controls(&notice_controls(&store.handout()));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }));
    }

    pub fn log(&self) -> String {
        self.d.fake_engine_log()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.log().lines().filter(|l| l.starts_with(prefix)).count()
    }

    pub async fn until(&self, what: &str, secs: u64, f: impl Fn(&Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !f(self) {
            assert!(
                Instant::now() < end,
                "waiting for {what}\nengine log:\n{}\nstore calls: {:?}",
                self.log(),
                self.store.calls()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn epoch(&self) -> String {
        self.until("the epoch", 20, |w| w.store.current_epoch().is_some())
            .await;
        self.store.current_epoch().unwrap()
    }

    pub async fn send(&self, id: &str, text: &str, mode: Mode) {
        let epoch = self.epoch().await;
        self.store
            .accept(Message {
                id: id.into(),
                text: text.into(),
                mode,
                epoch,
                seq: 0,
            })
            .unwrap();
    }

    /// Nothing overlapped: the engine never noted an overlap, and its prompts opened and ended strictly in turn.
    pub fn assert_serial(&self) {
        let log = self.log();
        assert!(
            !log.contains("overlap"),
            "a prompt overlapped another:\n{log}"
        );
        let mut open = false;
        for l in log.lines() {
            if l.starts_with("prompt-open") {
                assert!(!open, "a prompt opened while another was open:\n{log}");
                open = true;
            } else if l.starts_with("prompt-end") {
                open = false;
            }
        }
    }

    pub fn pids_of(&self, name: &str) -> Vec<u32> {
        std::fs::read_to_string(self.pids.join(name))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    pub fn cleanup(&self) {
        if let Some(p) = self.pump.lock().unwrap().take() {
            p.abort();
        }
        // a failed test must not leave a sleeping tool behind
        let tag = self.hook.tag();
        let want = format!("AGENTD_ENGINE_TAG={tag}");
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for e in dir.filter_map(Result::ok) {
                let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                if std::fs::read(e.path().join("environ"))
                    .is_ok_and(|b| b.split(|x| *x == 0).any(|x| x == want.as_bytes()))
                {
                    let _ = std::process::Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
            }
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .is_some_and(|c| c != 'Z' && c != 'X'),
        Err(_) => false,
    }
}

pub async fn all_gone(pids: &[u32], secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if pids.iter().all(|p| !alive(*p)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pids.iter().all(|p| !alive(*p))
}

/// The module's settings for a run that lasts seconds: the 300 s the product keeps for delivery would reject every message of a 120 s run.
pub fn quick() -> Settings {
    let mut s = Settings::for_tests();
    s.min_left = Duration::ZERO;
    s
}

/// A directory for the pid files of the tools a scenario starts; the prompt names the file, so it exists before the run does.
pub fn pid_dir() -> PathBuf {
    common::scratch("pids")
}

pub fn pidfile(dir: &Path, name: &str) -> String {
    dir.join(name).display().to_string()
}

pub fn directive(json: &str) -> String {
    // The task always changes a file, so a finished run reads "Cline reported done" and not "no changes".
    let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
    v.as_object_mut()
        .unwrap()
        .entry("write")
        .or_insert(serde_json::json!({"path": "work.txt", "text": "changed"}));
    format!("{PROMPT}@fake{v}")
}

/// The notices of an events response for these pending messages.
pub fn notice_controls(pending: &[Notice]) -> Vec<PendingControl> {
    pending
        .iter()
        .map(|n| {
            PendingControl::Message(MessageNotice {
                message_id: n.id.clone(),
                seq: u32::try_from(n.seq).unwrap_or(0),
                mode: match n.mode {
                    Mode::Next => MessageMode::Next,
                    Mode::Now => MessageMode::Now,
                },
                epoch: Some(n.epoch.clone()),
                bytes: u32::try_from(n.bytes).unwrap_or(0),
                accepted_at: "2026-10-16T14:00:00Z".to_string(),
            })
        })
        .collect()
}

/// The control plane's four internal routes, answered from the server model: what worker-api does, in the words of protocol 1.38.0.
/// The module's `HttpMessageStore` talks to it exactly as it talks to the real one.
pub struct Plane {
    store: Arc<MemoryStore>,
    /// Every request the module made, with its path.
    pub seen: Mutex<Vec<(String, Value)>>,
    /// The incarnations of agentd the server knows (protocol 1.38.0): the current one and the ones it replaced.
    generations: Mutex<Generations>,
    /// The session the server holds and its number within the run (`epochNo`).
    session: Mutex<(Option<String>, u32)>,
}

#[derive(Default)]
struct Generations {
    current: Option<String>,
    retired: Vec<String>,
}

/// The skew a replacement incarnation may have behind the current one (`GENERATION_CLOCK_SKEW_MS`, protocol 1.38.0).
pub const GENERATION_CLOCK_SKEW_MS: u64 = 60_000;

/// The millisecond time of a ULID: its first ten characters, Crockford base32.
pub fn ulid_ms(ulid: &str) -> u64 {
    const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    ulid.chars().take(10).fold(0u64, |acc, c| {
        acc * 32 + ALPHABET.find(c.to_ascii_uppercase()).unwrap_or(0) as u64
    })
}

/// A ULID with the given millisecond time and a fixed random part (for tests that need an incarnation of a given age).
pub fn ulid_at(ms: u64, tail: &str) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut head = [b'0'; 10];
    let mut v = ms;
    for slot in head.iter_mut().rev() {
        *slot = ALPHABET[(v % 32) as usize];
        v /= 32;
    }
    format!("{}{tail}", String::from_utf8(head.to_vec()).unwrap())
}

impl Plane {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            store,
            seen: Mutex::new(Vec::new()),
            generations: Mutex::new(Generations::default()),
            session: Mutex::new((None, 0)),
        }
    }

    /// The number of the session the server holds now.
    fn current_no(&self) -> u32 {
        self.session.lock().unwrap().1
    }

    /// The number of the session the server holds after a report: it grows when a session id the server has not seen is reported.
    fn session_no(&self, epoch: &str) -> u32 {
        let mut s = self.session.lock().unwrap();
        if s.0.as_deref() != Some(epoch) {
            s.0 = Some(epoch.to_string());
            s.1 += 1;
        }
        s.1
    }

    /// How many times the message was claimed: the attempt of its current (or last) claim.
    fn attempts(&self, id: &str) -> u32 {
        u32::try_from(
            self.store
                .history(id)
                .iter()
                .filter(|s| **s == agentd::direct::messages::State::Dispatching)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    /// What the server does with the incarnation a call names (protocol 1.38.0): the first session report adopts it; an incarnation
    /// the server has not seen replaces the current one unless its ULID is older by more than the skew; a replaced one is stale on
    /// every route; any other route needs a session reported first.
    fn check_generation(&self, path: &str, generation: &str) -> Option<(u16, Vec<u8>)> {
        let mut g = self.generations.lock().unwrap();
        let stale = || {
            answer(
                409,
                json!({"code": "stale-generation", "message": "this agentd was replaced"}),
            )
            .ok()
        };
        if g.retired.iter().any(|r| r == generation) {
            return stale();
        }
        match g.current.clone() {
            Some(cur) if cur == generation => None,
            Some(cur) if path == "/internal/message-session" => {
                if ulid_ms(generation) + GENERATION_CLOCK_SKEW_MS < ulid_ms(&cur) {
                    return stale();
                }
                g.retired.push(cur);
                g.current = Some(generation.to_string());
                // the replaced incarnation's dispatching messages become delivery-unknown
                self.store.worker_lost();
                None
            }
            Some(_) => stale(),
            None if path == "/internal/message-session" => {
                g.current = Some(generation.to_string());
                None
            }
            None => answer(
                409,
                json!({"code": "no-session", "message": "report a session first"}),
            )
            .ok(),
        }
    }
}

fn answer(status: u16, v: Value) -> Result<(u16, Vec<u8>), String> {
    Ok((status, v.to_string().into_bytes()))
}

impl Transport for Plane {
    fn post<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<(u16, Vec<u8>), String>> {
        Box::pin(async move {
            let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            self.seen
                .lock()
                .unwrap()
                .push((path.to_string(), v.clone()));
            let epoch = v["epoch"].as_str().unwrap_or_default().to_string();
            let id = v["messageId"].as_str().unwrap_or_default().to_string();
            if let Some(refused) =
                self.check_generation(path, v["generation"].as_str().unwrap_or_default())
            {
                return Ok(refused);
            }
            match path {
                "/internal/message-session" => {
                    if v["number"].as_u64().unwrap_or(0) == 0 {
                        return answer(
                            400,
                            json!({"code": "bad-request", "message": "number is at least 1"}),
                        );
                    }
                    self.store
                        .report_epoch(&epoch, 0)
                        .await
                        .map_err(|e| e.to_string())?;
                    answer(
                        200,
                        json!({"epochNo": self.session_no(&epoch), "held": 0, "deliveryUnknown": 0, "admission": "open"}),
                    )
                }
                "/internal/message-claim" if id.is_empty() => {
                    // a claim without an id: what this incarnation holds in dispatching, or else the next pending message
                    match self.store.held(&epoch).await {
                        Ok(list) if list.is_empty() => {
                            answer(200, json!({"outcome": "nothing", "admission": "open"}))
                        }
                        Ok(list) => answer(
                            200,
                            json!({"outcome": "claimed",
                                "redelivered": list.iter().any(|h| h.redelivered),
                                "messages": list.iter().map(|h| json!({
                                    "messageId": h.message.id, "seq": 1, "mode": h.message.mode.as_str(),
                                    "epochNo": if h.current_session { self.current_no() } else { 0 },
                                    "attempt": self.attempts(&h.message.id),
                                    "text": h.message.text, "acceptedAt": "2026-10-16T14:00:00Z"
                                })).collect::<Vec<_>>()}),
                        ),
                        Err(e) => {
                            answer(500, json!({"code": "internal", "message": e.to_string()}))
                        }
                    }
                }
                "/internal/message-claim" => {
                    let was_dispatching =
                        self.store.state(&id) == Some(agentd::direct::messages::State::Dispatching);
                    match self.store.claim(&id, &epoch).await {
                        Ok(ClaimOutcome::Claimed(m)) => answer(
                            200,
                            json!({"outcome": "claimed", "redelivered": was_dispatching, "messages": [{
                                "messageId": m.id, "seq": 1, "mode": m.mode.as_str(), "epochNo": self.current_no(),
                                "attempt": self.attempts(&m.id),
                                "text": m.text, "acceptedAt": "2026-10-16T14:00:00Z"
                            }]}),
                        ),
                        Ok(ClaimOutcome::Refused(Refusal::NotPending(s))) => {
                            answer(200, json!({"outcome": "not-pending", "state": s}))
                        }
                        Ok(ClaimOutcome::Refused(Refusal::Cancelled)) => answer(
                            200,
                            json!({"outcome": "refused", "reason": "cancel-queued"}),
                        ),
                        Ok(ClaimOutcome::Refused(Refusal::Sealed)) => answer(
                            200,
                            json!({"outcome": "refused", "reason": "admission-closed"}),
                        ),
                        Ok(ClaimOutcome::Refused(Refusal::EpochMoved)) => answer(
                            409,
                            json!({"code": "stale-epoch", "message": "the session moved on"}),
                        ),
                        Ok(ClaimOutcome::Refused(Refusal::StaleGeneration)) => answer(
                            409,
                            json!({"code": "stale-generation", "message": "replaced"}),
                        ),
                        Ok(ClaimOutcome::Refused(Refusal::Unknown)) => {
                            answer(200, json!({"outcome": "not-pending", "state": "unknown"}))
                        }
                        Err(e) => {
                            answer(500, json!({"code": "internal", "message": e.to_string()}))
                        }
                    }
                }
                "/internal/message-seal" => {
                    let mode = match v["mode"].as_str() {
                        Some("limit-reached") => SealMode::LimitReached,
                        Some("terminal") => SealMode::Terminal,
                        _ => SealMode::Normal,
                    };
                    match self.store.seal(&epoch, mode).await {
                        Ok(SealOutcome::Pending(n)) => answer(
                            200,
                            json!({"outcome": "pending", "messages": n.iter().map(|n| json!({
                                "messageId": n.id, "seq": 1, "mode": n.mode.as_str(), "epoch": n.epoch,
                                "bytes": n.bytes, "acceptedAt": "2026-10-16T14:00:00Z"
                            })).collect::<Vec<_>>()}),
                        ),
                        Ok(SealOutcome::Sealed { watermark }) => answer(
                            200,
                            json!({"outcome": "sealed", "watermark": watermark, "rejected": 0, "deliveryUnknown": 0}),
                        ),
                        Err(e) if e.detail.contains("bad-transition") => {
                            answer(409, json!({"code": "bad-transition", "message": e.detail}))
                        }
                        Err(e) => {
                            answer(500, json!({"code": "internal", "message": e.to_string()}))
                        }
                    }
                }
                "/internal/message-outcome" => {
                    let o = &v["outcome"];
                    let current = self.attempts(&id);
                    let named = v["attempt"].as_u64().unwrap_or(u64::MAX);
                    // the attempt fences an outcome; a rejection of a pending message takes 0 or the attempt of its last claim
                    let ok = if o["kind"].as_str() == Some("rejected") {
                        named == 0 || named == u64::from(current)
                    } else {
                        named == u64::from(current)
                    };
                    if !ok {
                        return answer(
                            409,
                            json!({"code": "stale-attempt", "message": "that attempt is over"}),
                        );
                    }
                    let r = match o["kind"].as_str() {
                        Some("submitted") => {
                            self.store
                                .submitted(
                                    &id,
                                    u32::try_from(o["turn"].as_u64().unwrap_or(0)).unwrap_or(0),
                                )
                                .await
                        }
                        Some("failed") => self.store.failed(&id, FailReason::NotSent).await,
                        Some("released") => self.store.release(&id).await,
                        Some("rejected") => {
                            let why = match o["reason"].as_str() {
                                Some("engine-busy") => {
                                    agentd::direct::messages::RejectReason::EngineBusy
                                }
                                _ => agentd::direct::messages::RejectReason::ContinuationLimit,
                            };
                            self.store.reject(&id, why).await
                        }
                        _ => {
                            return answer(
                                400,
                                json!({"code": "bad-request", "message": "unknown outcome"}),
                            );
                        }
                    };
                    match r {
                        Ok(()) => answer(200, json!({"messageId": id, "state": "delivered"})),
                        Err(e) => answer(
                            409,
                            json!({"code": "bad-transition", "message": e.to_string()}),
                        ),
                    }
                }
                other => answer(404, json!({"code": "no-route", "message": other})),
            }
        })
    }
}
