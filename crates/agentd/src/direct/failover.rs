//! The second slice of provider failover: after the resumes of the first slice on the model the user chose, agentd
//! switches the SAME engine session to the next route, then to the next model, and when every route has failed it waits and begins
//! again at the preferred one, instead of failing the run. This file holds the decisions only ([`Ladder`]): no engine, no clock, no
//! network, so every rule is a plain unit test. The loop that carries them out is [`crate::direct::retry::prompt_with_retry`].
//!
//! * The preferred route keeps the first slice's budget (3 resumes for a transient failure, 1 for a permanent one). A fallback route
//!   gets 1 resume for a transient failure and none for a permanent one: the switch itself was the new try.
//! * A route the provider refuses (model not found, access denied for the route) is left at once and never visited again in the
//!   session. So is a route whose very first submission after a switch ends in a permanent error: the history or the settings do not
//!   suit it.
//! * The next route is the next one in the request's order that this pass has not used. The preferred route is tried once more per pass
//!   (a return trial: one submission, no resumes) when it failed at least the dwell ago; the pass then goes on where it was.
//! * A switch takes effect only between two prompts (the engine applies `session/set_config_option` to the next prompt, not the one in
//!   flight), so every switch and every return happens at a failure.
//! * When every usable route has been used in a pass, [`Next::AllDown`] says how long to wait; the loop then probes the routes and
//!   [`Ladder::begin_pass`] starts the next pass on a healthy one (or on the preferred route when it cannot probe).
//! * A failure the work causes (content filter, context window, output limit) stops the ladder: another route is no answer to it.
//! * A stop the engine chose itself never reaches the ladder's decisions: it switches nothing,
//!   excludes nothing and counts toward nothing ([`Ladder::engine_stopped`] only clears the mark of a fresh route).
//! * The bound on prompts that achieve nothing: [`FRUITLESS_PROMPTS_MAX`] provider-failed prompts in a row
//!   with no completed tool call (any duration: a 6-minute timeout counts like a quick refusal; agentd counts prompts, not the engine's own
//!   requests) put the ladder on hold too.
//! * On hold a failure gets no resume: the route is asked once per healthy probe, so a provider whose probes pass while its prompts fail
//!   is not fed the whole history over and over.
//! * The sanity bound on switching: a switch is fruitless when the first attempt after it fails within [`FRUITLESS_WITHIN`] with no
//!   completed tool call. As many fruitless switches as there are usable routes (at most [`FRUITLESS_MAX`]) put the ladder on hold:
//!   it stops switching, waits and probes the route the user chose, and switches only to a route that answers. A completed tool call
//!   ends the hold. The hold is not a stop: the run waits until its deadline or the user's cancel.

use crate::acp::AcpError;
use crate::direct::engine_stop::StageFacts;
use crate::direct::retry::{RetryClass, RetryPolicy};
use harness_protocol::direct::{
    DirectFailoverNote, DirectModelSpec, DirectSwitch, DirectSwitchKind,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A switch is fruitless when the first attempt after it fails within this long with no completed tool call.
pub const FRUITLESS_WITHIN: Duration = Duration::from_secs(60);
/// The most fruitless switches that put the ladder on hold, however many routes there are.
pub const FRUITLESS_MAX: usize = 9;
/// Provider-failed prompts in a row with no completed tool call that put the ladder on hold (the recovery ladder's run-wide bound).
pub const FRUITLESS_PROMPTS_MAX: u32 = 12;
/// After this many seconds with every route down the run raises its outage notice once and goes on waiting (the recovery ladder's R5).
pub const OUTAGE_NOTICE_AFTER_S: u64 = 1_800;

/// Prefixes of cross-Region inference profile ids (`dispatcher::policy::GEO_PREFIXES`).
const GEO_PREFIXES: [&str; 8] = ["us", "us-gov", "eu", "apac", "jp", "au", "ca", "global"];

/// One route the engine can run on: a model id (an inference profile) in a Region, and the limits the model has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub id: String,
    pub region: String,
    pub context_tokens: u32,
    pub max_output_tokens: u32,
}

impl From<&DirectModelSpec> for Route {
    fn from(m: &DirectModelSpec) -> Self {
        Self {
            id: m.id.clone(),
            region: m.region.clone(),
            context_tokens: m.context_tokens,
            max_output_tokens: m.max_output_tokens,
        }
    }
}

/// The model an id names without its profile type (`us.`, `global.`, ...) or Mantle prefix.
fn base_model(id: &str) -> &str {
    let id = id.strip_prefix("aws-mantle/").unwrap_or(id);
    match id.split_once('.') {
        Some((p, rest)) if GEO_PREFIXES.contains(&p) => rest,
        _ => id,
    }
}

/// True when both ids are the same model (only the profile type differs).
pub fn same_model(a: &str, b: &str) -> bool {
    base_model(a) == base_model(b)
}

/// The smallest output limit and the smallest window of the routes: what the engine's provider settings carry for the whole run, so no
/// switch sends a limit a route does not allow. The provider settings are one pair for the run; the engine does not change them on a
/// switch (measured 2026-10-15).
pub fn effective_limits(routes: &[Route]) -> (u32, u32) {
    let out = routes
        .iter()
        .map(|r| r.max_output_tokens)
        .min()
        .unwrap_or(0);
    let ctx = routes.iter().map(|r| r.context_tokens).min().unwrap_or(0);
    (out, ctx)
}

/// The numbers of the ladder. The waits follow the signed route ladder of the old line (`engine/policy/ladder/v3.json`: wait base 30 s,
/// factor 1.5, cap 300 s; `returnMinDwellS` 120).
#[derive(Clone, Debug)]
pub struct LadderPolicy {
    /// Resumes and waits on the preferred route: the first slice's policy.
    pub preferred: RetryPolicy,
    /// Resumes on a fallback route after a transient failure.
    pub fallback_transient_retries: u32,
    /// Resumes on a fallback route after a permanent failure.
    pub fallback_permanent_retries: u32,
    /// How long after the preferred route failed it may be tried again.
    pub return_dwell: Duration,
    pub wait_base: Duration,
    pub wait_factor: f64,
    pub wait_cap: Duration,
}

impl LadderPolicy {
    pub fn new(preferred: RetryPolicy) -> Self {
        Self {
            preferred,
            fallback_transient_retries: 1,
            fallback_permanent_retries: 0,
            return_dwell: Duration::from_secs(120),
            wait_base: Duration::from_secs(30),
            wait_factor: 1.5,
            wait_cap: Duration::from_secs(300),
        }
    }
}

/// Why the ladder gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopWhy {
    /// Another route is no answer to this failure (the work or the context causes it).
    Never,
    /// A run with one route has spent its resumes.
    Exhausted,
    /// Every route was refused or excluded.
    NoRouteRemains,
}

/// What to do after a failed submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Send the resume sentence on the same route after `wait`.
    Retry {
        wait: Duration,
    },
    /// Switch the engine to route `to`, then send the resume sentence. The loop confirms with [`Ladder::switched`] or reports
    /// [`Ladder::refused`].
    Switch {
        to: usize,
        kind: DirectSwitchKind,
    },
    /// Every usable route has been used in this pass: wait `wait`, probe, then [`Ladder::begin_pass`]. `pass` counts the waits so far.
    AllDown {
        wait: Duration,
        pass: u32,
    },
    Stop(StopWhy),
}

#[derive(Debug)]
struct Slot {
    route: Route,
    /// Refused or unsuitable: never visited again in this session.
    excluded: bool,
    /// Used in the current pass.
    visited: bool,
    last_failed: Option<Instant>,
}

#[derive(Debug)]
pub struct Ladder {
    slots: Vec<Slot>,
    policy: LadderPolicy,
    /// The route the engine is on.
    cur: usize,
    /// Resumes sent on `cur` since the engine moved to it.
    retries_used: u32,
    /// No submission has failed on `cur` since the engine moved to it.
    fresh: bool,
    /// `cur` is the preferred route again after a fallback: one submission, no resumes.
    return_trial: bool,
    /// The preferred route has had its return trial in this pass.
    pref_reentered: bool,
    /// Resumes the latest failure's class allowed on the route (for the "try 2 of 4" words).
    last_allowed: u32,
    /// Waits with every route down so far.
    passes: u32,
    /// Index into the wait schedule of the next [`Ladder::next_wait`].
    wait_k: u32,
    last_class: &'static str,
    /// Fruitless switches since a tool call last completed.
    fruitless: u32,
    /// Provider-failed prompts since a tool call last completed.
    fruitless_prompts: u32,
    /// The attempt that ends next is the first since the engine moved to its route.
    awaiting_first: bool,
    /// Switching is on hold: the fruitless bound was reached.
    hold: bool,
}

fn class_word(c: RetryClass) -> &'static str {
    match c {
        RetryClass::EngineStop => "engine_stop",
        RetryClass::HistoryUnusable => "history_unusable",
        RetryClass::Transient | RetryClass::Never => "transient",
        RetryClass::Permanent => "permanent",
        RetryClass::Unavailable => "unavailable",
    }
}

impl Ladder {
    /// A ladder that starts on route `current`: where the engine really is when a stage begins after earlier switches.
    /// The route counts as used in the pass, it is not "fresh" (the engine has been there), and `routes[0]`, the
    /// user's choice, is an ordinary candidate that has not failed in this ladder. `0` and an index out of range mean [`Ladder::new`].
    pub fn new_at(routes: Vec<Route>, policy: LadderPolicy, current: usize) -> Self {
        let mut l = Self::new(routes, policy);
        if current != 0 && current < l.slots.len() {
            l.slots[0].visited = false;
            l.slots[current].visited = true;
            l.cur = current;
        }
        l
    }

    /// `routes[0]` is the route the user chose and the engine starts on.
    pub fn new(routes: Vec<Route>, policy: LadderPolicy) -> Self {
        let mut slots: Vec<Slot> = routes
            .into_iter()
            .map(|route| Slot {
                route,
                excluded: false,
                visited: false,
                last_failed: None,
            })
            .collect();
        if let Some(first) = slots.first_mut() {
            first.visited = true;
        }
        Self {
            slots,
            policy,
            cur: 0,
            retries_used: 0,
            fresh: false,
            return_trial: false,
            pref_reentered: false,
            last_allowed: 0,
            passes: 0,
            wait_k: 0,
            last_class: "transient",
            fruitless: 0,
            fruitless_prompts: 0,
            awaiting_first: false,
            hold: false,
        }
    }

    pub fn current(&self) -> usize {
        self.cur
    }

    pub fn route(&self, i: usize) -> &Route {
        &self.slots[i].route
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn excluded(&self, i: usize) -> bool {
        self.slots.get(i).is_none_or(|s| s.excluded)
    }

    /// Waits with every route down so far.
    pub fn pass(&self) -> u32 {
        self.passes
    }

    /// The resumes sent on the current route so far and the most its latest failure's class allows: after a [`Next::Retry`], the
    /// numbers of "try 2 of 4".
    pub fn retry_progress(&self) -> (u32, u32) {
        (self.retries_used, self.last_allowed)
    }

    /// The class of the latest failure: `transient`, `permanent` or `unavailable`.
    pub fn last_class(&self) -> &'static str {
        self.last_class
    }

    /// Routes that can still be used: not excluded.
    pub fn usable(&self) -> Vec<usize> {
        (0..self.slots.len())
            .filter(|i| !self.slots[*i].excluded)
            .collect()
    }

    /// The seconds to wait before the next probe round: 30 s, then half again as much each time, at most 300 s.
    pub fn next_wait(&mut self) -> Duration {
        let k = i32::try_from(self.wait_k).unwrap_or(i32::MAX);
        self.wait_k = self.wait_k.saturating_add(1);
        let secs = self.policy.wait_base.as_secs_f64() * self.policy.wait_factor.powi(k);
        Duration::from_secs_f64(secs).min(self.policy.wait_cap)
    }

    /// A submission ran for `d` before it failed. A route that worked for as long as the longest wait is healthy again: the next outage
    /// starts the wait schedule over.
    pub fn healthy_for(&mut self, d: Duration) {
        if d >= self.policy.wait_cap {
            self.wait_k = 0;
        }
    }

    /// Fruitless switches since a tool call last completed.
    pub fn fruitless(&self) -> u32 {
        self.fruitless
    }

    /// Provider-failed prompts since a tool call last completed.
    pub fn fruitless_prompts(&self) -> u32 {
        self.fruitless_prompts
    }

    /// Switching is on hold (see the module notes): the ladder waits and probes instead of switching.
    pub fn holding(&self) -> bool {
        self.hold
    }

    /// Puts switching on hold at once: the run has made as many switches as the summary can list.
    pub fn force_hold(&mut self) {
        self.hold = true;
    }

    /// The routes a probe round asks: on hold only the route the user chose (when it is usable), else every usable route.
    pub fn probe_targets(&self) -> Vec<usize> {
        if self.hold && self.slots.first().is_some_and(|s| !s.excluded) {
            vec![0]
        } else {
            self.usable()
        }
    }

    /// The engine ended the prompt itself (`RetryClass::EngineStop`). The route answered, so it is not "fresh" any more (a later
    /// refusal is not the first submission's), and the stop is not a switch's first attempt: nothing else moves.
    pub fn engine_stopped(&mut self) {
        self.fresh = false;
        self.awaiting_first = false;
    }

    /// A submission ended in a provider failure after `ran`, having completed `completed_calls` tool calls. A completed call ends the
    /// counts and the hold; with none, the prompt counts toward [`FRUITLESS_PROMPTS_MAX`], and the first attempt after a switch that fails
    /// within [`FRUITLESS_WITHIN`] counts one fruitless switch.
    pub fn attempt_ended(&mut self, completed_calls: u64, ran: Duration) {
        let first = std::mem::take(&mut self.awaiting_first);
        if completed_calls > 0 {
            self.fruitless = 0;
            self.fruitless_prompts = 0;
            self.hold = false;
            return;
        }
        self.fruitless_prompts = self.fruitless_prompts.saturating_add(1);
        if self.fruitless_prompts >= FRUITLESS_PROMPTS_MAX {
            self.hold = true;
        }
        if first && ran <= FRUITLESS_WITHIN {
            self.fruitless = self.fruitless.saturating_add(1);
            let limit = self.usable().len().clamp(1, FRUITLESS_MAX);
            if usize::try_from(self.fruitless).unwrap_or(usize::MAX) >= limit {
                self.hold = true;
            }
        }
    }

    /// A submission on the current route ended in a failure of `class`.
    pub fn failed(&mut self, class: RetryClass, err: &AcpError, now: Instant) -> Next {
        self.last_class = class_word(class);
        // An engine stop and an unusable history are no failure of the route: nothing to retry here, nothing to switch to.
        if matches!(
            class,
            RetryClass::Never | RetryClass::EngineStop | RetryClass::HistoryUnusable
        ) {
            return Next::Stop(StopWhy::Never);
        }
        let fresh = std::mem::replace(&mut self.fresh, false);
        let cur = self.cur;
        self.slots[cur].last_failed = Some(now);
        if self.slots.len() == 1 {
            let allowed = self.policy.preferred.allowed(class);
            self.last_allowed = allowed;
            if self.retries_used < allowed {
                self.retries_used += 1;
                return Next::Retry {
                    wait: self.policy.preferred.pause_before(self.retries_used, err),
                };
            }
            return Next::Stop(StopWhy::Exhausted);
        }
        // A refused route, and a route whose first submission after a switch is refused as invalid, are left for good.
        if class == RetryClass::Unavailable || (class == RetryClass::Permanent && fresh) {
            self.slots[cur].excluded = true;
        }
        let allowed = if self.slots[cur].excluded || self.return_trial || self.hold {
            0
        } else if cur == 0 {
            self.policy.preferred.allowed(class)
        } else if class == RetryClass::Permanent {
            self.policy.fallback_permanent_retries
        } else {
            self.policy.fallback_transient_retries
        };
        self.last_allowed = allowed;
        if self.retries_used < allowed {
            self.retries_used += 1;
            return Next::Retry {
                wait: self.policy.preferred.pause_before(self.retries_used, err),
            };
        }
        self.pick_next(now)
    }

    /// The engine is on route `to` now.
    pub fn switched(&mut self, to: usize) {
        let returning = to == 0 && self.cur != 0;
        self.cur = to;
        self.slots[to].visited = true;
        self.retries_used = 0;
        self.fresh = true;
        self.awaiting_first = true;
        self.return_trial = returning;
    }

    /// The engine refused route `route` (or it cannot be selected): it is out for the session, and the next one is chosen.
    pub fn refused(&mut self, route: usize, now: Instant) -> Next {
        self.slots[route].excluded = true;
        self.pick_next(now)
    }

    /// The small test request of route `route`, the target of a switch, was not answered: the switch is not made and no prompt is sent.
    /// The route is used up for this pass (not excluded) and the next target is chosen. Returns that decision.
    pub fn unanswered(&mut self, route: usize, now: Instant) -> Next {
        self.slots[route].visited = true;
        self.slots[route].last_failed = Some(now);
        self.pick_next(now)
    }

    /// A route is out for the session (a probe found it refused).
    pub fn exclude(&mut self, route: usize) {
        if let Some(s) = self.slots.get_mut(route) {
            s.excluded = true;
        }
    }

    /// A new pass begins on `start` (a route that answered a probe, or the preferred one): every route is unused again and the budgets
    /// are fresh. Returns the switch the engine needs when it is not on `start`.
    pub fn begin_pass(&mut self, start: usize, _now: Instant) -> Option<(usize, DirectSwitchKind)> {
        for s in &mut self.slots {
            s.visited = false;
        }
        self.slots[start].visited = true;
        self.pref_reentered = false;
        self.retries_used = 0;
        self.return_trial = false;
        if start == self.cur {
            self.fresh = false;
            None
        } else {
            Some((start, self.kind_to(start)))
        }
    }

    fn kind_to(&self, to: usize) -> DirectSwitchKind {
        if to == 0 && self.cur != 0 {
            DirectSwitchKind::Return
        } else if same_model(&self.slots[self.cur].route.id, &self.slots[to].route.id) {
            DirectSwitchKind::Route
        } else {
            DirectSwitchKind::Model
        }
    }

    fn pick_next(&mut self, now: Instant) -> Next {
        let cur = self.cur;
        self.slots[cur].visited = true;
        if self.slots.iter().all(|s| s.excluded) {
            return Next::Stop(StopWhy::NoRouteRemains);
        }
        if self.hold {
            // The bound on fruitless switches was reached: no route is switched to; the run waits and probes.
            self.passes += 1;
            let wait = self.next_wait();
            return Next::AllDown {
                wait,
                pass: self.passes,
            };
        }
        // The preferred route once more per pass, when it failed at least the dwell ago.
        if cur != 0
            && !self.pref_reentered
            && !self.slots[0].excluded
            && self.slots[0]
                .last_failed
                .is_some_and(|t| now.saturating_duration_since(t) >= self.policy.return_dwell)
        {
            self.pref_reentered = true;
            self.slots[0].visited = true;
            return Next::Switch {
                to: 0,
                kind: DirectSwitchKind::Return,
            };
        }
        let n = self.slots.len();
        for step in 1..n {
            let i = (cur + step) % n;
            if !self.slots[i].excluded && !self.slots[i].visited {
                return Next::Switch {
                    to: i,
                    kind: self.kind_to(i),
                };
            }
        }
        self.passes += 1;
        let wait = self.next_wait();
        Next::AllDown {
            wait,
            pass: self.passes,
        }
    }
}

// ---- what the loop is given and what it hands back -------------------------------------------------------------------------

/// The answer of a small test request to one route while every route is down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// The route answered: it is worth a real resume.
    Healthy,
    /// The route did not answer (throttled, unavailable, timed out): try again after the next wait.
    Down(String),
    /// The provider refuses the route itself (model not found, access denied): it is out for the session.
    Refused(String),
}

/// A cheap test request to a route (a one-message Converse call with a few output tokens): what agentd sends while every route is down,
/// instead of a resume that carries the whole conversation. A success proves only that the route answers a small request.
pub trait RouteProbe: Send + Sync + std::fmt::Debug {
    fn probe<'a>(
        &'a self,
        route: &'a Route,
    ) -> Pin<Box<dyn Future<Output = ProbeAnswer> + Send + 'a>>;
}

/// The routes of a run and the settings of the ladder over them.
#[derive(Clone, Debug)]
pub struct FailoverPlan {
    /// `routes[0]` is the route the user chose; the engine starts on it.
    pub routes: Vec<Route>,
    /// Resumes on a fallback route after a transient failure.
    pub fallback_retries: u32,
    pub return_dwell: Duration,
    pub wait_base: Duration,
    pub wait_cap: Duration,
    /// How long the engine gets to confirm a switch.
    pub switch_timeout: Duration,
    /// The longest a wait is slept in one piece; agentd marks the engine's silence clock between pieces.
    pub wait_slice: Duration,
    /// After this long with every route down the run raises its outage notice once and goes on waiting
    /// ([`crate::direct::collector::DirectCollector::record_outage_notice`]).
    pub notice_after: Duration,
    /// The route the engine is on when this plan's first prompt is sent; `None` means `routes[0]`, the user's choice.
    pub start: Option<usize>,
    /// End the call at the cap of [`FRUITLESS_PROMPTS_MAX`] fruitless prompts instead of holding and waiting for the deadline: set for a
    /// run that can ask its user what to do. Off, the ladder holds as it always did.
    pub stop_at_cap: bool,
    pub probe: Option<Arc<dyn RouteProbe>>,
}

impl FailoverPlan {
    pub fn new(routes: Vec<Route>) -> Self {
        Self {
            routes,
            fallback_retries: 1,
            return_dwell: Duration::from_secs(120),
            wait_base: Duration::from_secs(30),
            wait_cap: Duration::from_secs(300),
            switch_timeout: Duration::from_secs(30),
            wait_slice: Duration::from_secs(30),
            notice_after: Duration::from_secs(OUTAGE_NOTICE_AFTER_S),
            start: None,
            stop_at_cap: false,
            probe: None,
        }
    }

    pub fn with_probe(mut self, probe: Arc<dyn RouteProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// The plan with its start seeded from the model the engine is on (`FailoverRecord.final_model`): the route with that id, else
    /// `None` (the user's choice).
    pub fn starting_on(mut self, model_id: Option<&str>) -> Self {
        self.start = model_id.and_then(|m| self.routes.iter().position(|r| r.id == m));
        self
    }

    /// The ladder over these routes, with the user's resume policy on the preferred one.
    pub fn ladder(&self, preferred: RetryPolicy) -> Ladder {
        let mut policy = LadderPolicy::new(preferred);
        policy.fallback_transient_retries = self.fallback_retries;
        policy.return_dwell = self.return_dwell;
        policy.wait_base = self.wait_base;
        policy.wait_cap = self.wait_cap;
        Ladder::new_at(self.routes.clone(), policy, self.start.unwrap_or(0))
    }

    pub fn effective_limits(&self) -> (u32, u32) {
        effective_limits(&self.routes)
    }
}

/// What the loop learned: handed to the summary.
#[derive(Debug, Default)]
pub struct FailoverRecord {
    pub switches: Vec<DirectSwitch>,
    /// The model the engine ended on, once it switched.
    pub final_model: Option<String>,
    pub passes: u32,
    pub probes: u32,
    pub waited_s: u64,
    /// When the current stretch with every route down began; `None` while a pass is running.
    pub outage_since: Option<tokio::time::Instant>,
    /// What the stage in progress learned; the caller starts a stage with [`FailoverRecord::begin_stage`].
    pub stage: StageFacts,
    /// Which early warnings of the session's disks are on: one notice per crossing, for the whole run.
    pub disk: crate::direct::disk::DiskWatch,
}

impl FailoverRecord {
    /// A new stage begins: its facts start over (the switches, the passes and the other totals of the run go on).
    pub fn begin_stage(&mut self) {
        self.stage = StageFacts::default();
    }

    /// Seconds the run has been in a stretch with every route down, when it ended in one.
    pub fn outage_s(&self) -> Option<u64> {
        self.outage_since.map(|t| t.elapsed().as_secs())
    }

    /// The note of `summary.json` for a run that had these routes.
    pub fn note(&self, plan: &FailoverPlan) -> DirectFailoverNote {
        let (out, ctx) = plan.effective_limits();
        DirectFailoverNote {
            routes: u32::try_from(plan.routes.len()).unwrap_or(u32::MAX),
            effective_max_output_tokens: out,
            effective_context_tokens: ctx,
            passes: self.passes,
            probes: self.probes,
            waited_s: self.waited_s,
        }
    }
}
