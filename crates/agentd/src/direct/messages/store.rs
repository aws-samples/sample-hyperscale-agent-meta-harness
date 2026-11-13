//! The store of messages as agentd sees it: the calls it makes to the control plane and the server model the tests run them against.
//!
//! [`MessageStore`] is the one seam between the module and the world. The production implementation is [`crate::direct::messages::http`],
//! which signs the calls to the private internal API with the run's session credentials; [`MemoryStore`] is the in-memory server model
//! every test of the module runs against, so the module is proven against the rules the real server must keep, and
//! the real server (`crates/worker-api`, `crates/control-api`) is proven against the same rules by its own tests.
//!
//! The model keeps the safety claims of the decision record by construction: every transition is one conditional step under one lock, so
//! a withdrawal and a claim cannot both win; a claim moves a message to `dispatching`, which only the claiming worker generation can
//! settle; a seal that finds nothing pending closes admission in the same step; a close never rejects a claimed or delivered message.

use crate::BoxFuture;
use crate::direct::messages::types::{
    ClaimOutcome, FailReason, Held, Message, Notice, Refusal, RejectReason, SealMode, SealOutcome,
    State, StoreError,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// The calls agentd makes about messages. Every call is idempotent where the wire is (a claim by the worker that holds it, an outcome
/// already posted), so a call whose answer was lost can be repeated.
pub trait MessageStore: Send + Sync {
    /// agentd's ACP session changed: the server commits it before any later claim and holds the pending
    /// messages written against another session.
    fn report_epoch<'a>(
        &'a self,
        epoch: &'a str,
        number: u32,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// The dispatch claim: one conditional step from pending to dispatching, fenced by the worker generation and the epoch.
    fn claim<'a>(
        &'a self,
        id: &'a str,
        epoch: &'a str,
    ) -> BoxFuture<'a, Result<ClaimOutcome, StoreError>>;

    /// A claim without a message id (protocol 1.38.0): every message this agentd incarnation holds in `dispatching`, `redelivered` and
    /// with the attempt it had, also after the session changed; when it holds none, the next pending message is claimed. The module asks
    /// after a claim whose answer may have been lost and before it seals, to learn what it holds.
    fn held<'a>(&'a self, epoch: &'a str) -> BoxFuture<'a, Result<Vec<Held>, StoreError>>;

    /// The prompt carrying the message was sent in turn `turn`: dispatching becomes delivered.
    fn submitted<'a>(&'a self, id: &'a str, turn: u32) -> BoxFuture<'a, Result<(), StoreError>>;

    /// The claimed message never reached the engine: dispatching becomes failed.
    fn failed<'a>(
        &'a self,
        id: &'a str,
        reason: FailReason,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// An interrupt the engine did not acknowledge: the claim is given back, the message goes on as a next-turn message.
    fn release<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), StoreError>>;

    /// A pending message agentd will not send: the continuation allowance is spent or no time is left.
    fn reject<'a>(
        &'a self,
        id: &'a str,
        reason: RejectReason,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// The one atomic "anything pending? if not, the run is done", and the terminal close of a run that ends whatever happens.
    fn seal<'a>(
        &'a self,
        epoch: &'a str,
        mode: SealMode,
    ) -> BoxFuture<'a, Result<SealOutcome, StoreError>>;

    /// The store has learned that it cannot serve this run: the plane has no message routes, or this agentd was fenced (a stale
    /// generation or session). The module stays out of the run from then on and calls nothing more.
    fn unavailable(&self) -> bool {
        false
    }
}

/// The calls a test can make fail or slow down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    ReportEpoch,
    Claim,
    Submitted,
    Failed,
    Release,
    Reject,
    Seal,
}

impl Op {
    fn word(self) -> &'static str {
        match self {
            Self::ReportEpoch => "epoch",
            Self::Claim => "claim",
            Self::Submitted => "submitted",
            Self::Failed => "failed",
            Self::Release => "release",
            Self::Reject => "reject",
            Self::Seal => "seal",
        }
    }
}

/// Why a withdrawal was refused (the 409s of the withdraw route).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WithdrawRefused {
    NotFound,
    /// The claim won the race: the message may already run.
    AlreadyDispatching,
    NotWithdrawable(State),
}

/// Why a confirmation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmRefused {
    NotFound,
    /// The held message is no longer held (withdrawn, rejected, already superseded).
    NotHeld(State),
    /// The session the user was shown is not the current one, or the copy is written against another session.
    EpochMoved,
    Sealed,
}

#[derive(Debug)]
struct Item {
    msg: Message,
    state: State,
    history: Vec<State>,
    claimed_by: Option<u32>,
}

impl Item {
    fn set(&mut self, s: State) {
        self.history.push(s.clone());
        self.state = s;
    }
}

#[derive(Debug, Default)]
struct Faults {
    fail: HashMap<Op, u32>,
    lose: HashMap<Op, u32>,
    delay: HashMap<Op, Duration>,
    delay_after: HashMap<Op, Duration>,
}

/// What an outcome call may read of the server besides the message: whether the user's cancel is queued and which session is current.
#[derive(Debug)]
struct Ctx {
    cancel_queued: bool,
    epoch: Option<String>,
}

#[derive(Debug)]
struct Server {
    items: Vec<Item>,
    epoch: Option<String>,
    admission_open: bool,
    generation: u32,
    cancel_queued: bool,
    accepted: u64,
    calls: Vec<String>,
    faults: Faults,
}

/// Who is calling: a worker generation. [`MemoryStore`] itself calls as whichever generation is current.
#[derive(Clone, Copy, Debug)]
enum Caller {
    Current,
    Generation(u32),
}

/// The in-memory server model. Cheap to share; every method takes `&self`.
#[derive(Debug)]
pub struct MemoryStore {
    server: Mutex<Server>,
}

impl MemoryStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            server: Mutex::new(Server {
                items: Vec::new(),
                epoch: None,
                admission_open: true,
                generation: 1,
                cancel_queued: false,
                accepted: 0,
                calls: Vec::new(),
                faults: Faults::default(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Server> {
        self.server.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- what the clients of the server do --------------------------------------------------------------------------------

    /// `POST /v1/runs/{id}/messages`: durably accepts a message. A message written against another session than the current one is
    /// held at once. After the seal nothing is stored.
    pub fn accept(&self, msg: Message) -> Result<(), RejectReason> {
        let mut s = self.lock();
        if !s.admission_open {
            return Err(RejectReason::RunSealed);
        }
        if s.items.iter().any(|i| i.msg.id == msg.id) {
            return Ok(());
        }
        s.accepted += 1;
        let current = s.epoch.clone();
        let mut item = Item {
            msg,
            state: State::Accepted,
            history: vec![State::Accepted],
            claimed_by: None,
        };
        match current {
            Some(e) if e != item.msg.epoch => item.set(State::Held),
            _ => item.set(State::Pending),
        }
        s.items.push(item);
        Ok(())
    }

    /// `POST /v1/runs/{id}/messages/{id}/withdraw`: competes with the claim for the same item; the first conditional update wins.
    pub fn withdraw(&self, id: &str) -> Result<(), WithdrawRefused> {
        let mut s = self.lock();
        let Some(item) = s.items.iter_mut().find(|i| i.msg.id == id) else {
            return Err(WithdrawRefused::NotFound);
        };
        match item.state {
            State::Pending | State::Held => {
                item.set(State::Withdrawn);
                Ok(())
            }
            State::Dispatching => Err(WithdrawRefused::AlreadyDispatching),
            ref other => Err(WithdrawRefused::NotWithdrawable(other.clone())),
        }
    }

    /// What the control plane puts in an events response: the pending messages of the current session, in server order. Changes
    /// nothing; it can repeat.
    pub fn handout(&self) -> Vec<Notice> {
        let s = self.lock();
        s.items
            .iter()
            .filter(|i| i.state == State::Pending && Some(&i.msg.epoch) == s.epoch.as_ref())
            .map(|i| i.msg.notice())
            .collect()
    }

    /// A confirmation of a held message: the copy is a new message, the original becomes superseded; the update fails when the
    /// session the user was shown has moved on or the original is no longer held.
    pub fn confirm(
        &self,
        held_id: &str,
        shown_epoch: &str,
        copy: Message,
    ) -> Result<(), ConfirmRefused> {
        let mut s = self.lock();
        if !s.admission_open {
            return Err(ConfirmRefused::Sealed);
        }
        if s.epoch.as_deref() != Some(shown_epoch) || copy.epoch != shown_epoch {
            return Err(ConfirmRefused::EpochMoved);
        }
        let Some(original) = s.items.iter_mut().find(|i| i.msg.id == held_id) else {
            return Err(ConfirmRefused::NotFound);
        };
        if original.state != State::Held {
            return Err(ConfirmRefused::NotHeld(original.state.clone()));
        }
        original.set(State::Superseded);
        let mut item = Item {
            msg: copy,
            state: State::Accepted,
            history: vec![State::Accepted],
            claimed_by: None,
        };
        item.set(State::Pending);
        s.items.push(item);
        s.accepted += 1;
        Ok(())
    }

    /// The control plane learns the worker was lost: the messages it was sending become delivery-unknown and are never sent again,
    /// and the next worker is another generation.
    pub fn worker_lost(&self) {
        let mut s = self.lock();
        for i in s.items.iter_mut().filter(|i| i.state == State::Dispatching) {
            i.set(State::DeliveryUnknown);
        }
        s.generation += 1;
    }

    /// The user's cancel is queued: claims are refused and the messages end rejected for it.
    pub fn queue_cancel(&self) {
        self.lock().cancel_queued = true;
    }

    /// A view of the store as worker generation `generation` sees it (a replaced worker keeps calling with its old one).
    pub fn view(self: &Arc<Self>, generation: u32) -> Arc<dyn MessageStore> {
        Arc::new(View {
            store: self.clone(),
            generation,
        })
    }

    // ---- what a test reads ----------------------------------------------------------------------------------------------

    pub fn state(&self, id: &str) -> Option<State> {
        self.lock()
            .items
            .iter()
            .find(|i| i.msg.id == id)
            .map(|i| i.state.clone())
    }

    /// Every state the message passed through, in order, the current one last.
    pub fn history(&self, id: &str) -> Vec<State> {
        self.lock()
            .items
            .iter()
            .find(|i| i.msg.id == id)
            .map(|i| i.history.clone())
            .unwrap_or_default()
    }

    /// The ids and states of every message, in server order.
    pub fn states(&self) -> Vec<(String, State)> {
        self.lock()
            .items
            .iter()
            .map(|i| (i.msg.id.clone(), i.state.clone()))
            .collect()
    }

    pub fn admission_open(&self) -> bool {
        self.lock().admission_open
    }

    pub fn current_epoch(&self) -> Option<String> {
        self.lock().epoch.clone()
    }

    pub fn generation(&self) -> u32 {
        self.lock().generation
    }

    /// One line per call agentd made, in order (`claim m1 s1`, `submitted m1 2`, `seal s1 normal`, ...), failed ones included.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    // ---- faults ---------------------------------------------------------------------------------------------------------

    /// The next `times` calls of `op` fail with a retryable error and change nothing.
    pub fn fail(&self, op: Op, times: u32) {
        self.lock().faults.fail.insert(op, times);
    }

    /// The next `times` calls of `op` are applied and still report a retryable error: the response was lost.
    pub fn lose_response(&self, op: Op, times: u32) {
        self.lock().faults.lose.insert(op, times);
    }

    /// Every call of `op` waits this long before it is applied.
    pub fn delay(&self, op: Op, pause: Duration) {
        self.lock().faults.delay.insert(op, pause);
    }

    /// Every call of `op` is applied at once and its answer waits this long: the window in which a withdrawal meets a claim that has
    /// already won.
    pub fn delay_after(&self, op: Op, pause: Duration) {
        self.lock().faults.delay_after.insert(op, pause);
    }

    fn pause_after(&self, op: Op) -> Option<Duration> {
        self.lock().faults.delay_after.get(&op).copied()
    }

    // ---- the calls of agentd ----------------------------------------------------------------------------------------------

    fn caller_generation(s: &Server, caller: Caller) -> u32 {
        match caller {
            Caller::Current => s.generation,
            Caller::Generation(g) => g,
        }
    }

    /// Records the call, applies a fault; `Ok(true)` when the call must be applied and its answer lost.
    fn enter(&self, op: Op, line: String) -> Result<bool, StoreError> {
        let mut s = self.lock();
        s.calls.push(line);
        if let Some(n) = s.faults.fail.get_mut(&op)
            && *n > 0
        {
            *n -= 1;
            return Err(StoreError::retryable(format!(
                "injected failure of {}",
                op.word()
            )));
        }
        if let Some(n) = s.faults.lose.get_mut(&op)
            && *n > 0
        {
            *n -= 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn pause(&self, op: Op) -> Option<Duration> {
        self.lock().faults.delay.get(&op).copied()
    }

    fn lost(op: Op) -> StoreError {
        StoreError::retryable(format!("the response of {} was lost", op.word()))
    }

    async fn do_report(&self, caller: Caller, epoch: &str, number: u32) -> Result<(), StoreError> {
        if let Some(d) = self.pause(Op::ReportEpoch) {
            tokio::time::sleep(d).await;
        }
        let lose = self.enter(Op::ReportEpoch, format!("epoch {epoch} {number}"))?;
        {
            let mut s = self.lock();
            if Self::caller_generation(&s, caller) != s.generation {
                return Err(StoreError::fatal("stale worker generation"));
            }
            if s.epoch.as_deref() != Some(epoch) {
                s.epoch = Some(epoch.to_string());
                for i in s
                    .items
                    .iter_mut()
                    .filter(|i| i.state == State::Pending && i.msg.epoch != epoch)
                {
                    i.set(State::Held);
                }
            }
        }
        if lose {
            return Err(Self::lost(Op::ReportEpoch));
        }
        Ok(())
    }

    async fn do_claim(
        &self,
        caller: Caller,
        id: &str,
        epoch: &str,
    ) -> Result<ClaimOutcome, StoreError> {
        if let Some(d) = self.pause(Op::Claim) {
            tokio::time::sleep(d).await;
        }
        let lose = self.enter(Op::Claim, format!("claim {id} {epoch}"))?;
        let outcome = {
            let mut s = self.lock();
            let gen_now = s.generation;
            let gen_caller = Self::caller_generation(&s, caller);
            let cancel = s.cancel_queued;
            let server_epoch = s.epoch.clone();
            let open = s.admission_open;
            match s.items.iter_mut().find(|i| i.msg.id == id) {
                _ if gen_caller != gen_now => ClaimOutcome::Refused(Refusal::StaleGeneration),
                None => ClaimOutcome::Refused(Refusal::Unknown),
                Some(item) => {
                    if cancel {
                        if matches!(item.state, State::Pending | State::Held) {
                            item.set(State::Rejected(RejectReason::RunCancelled));
                        }
                        ClaimOutcome::Refused(Refusal::Cancelled)
                    } else if item.state == State::Dispatching
                        && item.claimed_by == Some(gen_caller)
                    {
                        // The same worker asks again (its first answer was lost): it is the same claim.
                        ClaimOutcome::Claimed(item.msg.clone())
                    } else if item.state != State::Pending {
                        ClaimOutcome::Refused(Refusal::NotPending(item.state.name().to_string()))
                    } else if item.msg.epoch != epoch || server_epoch.as_deref() != Some(epoch) {
                        ClaimOutcome::Refused(Refusal::EpochMoved)
                    } else if !open {
                        ClaimOutcome::Refused(Refusal::Sealed)
                    } else {
                        item.set(State::Dispatching);
                        item.claimed_by = Some(gen_caller);
                        ClaimOutcome::Claimed(item.msg.clone())
                    }
                }
            }
        };
        if let Some(d) = self.pause_after(Op::Claim) {
            tokio::time::sleep(d).await;
        }
        if lose {
            return Err(Self::lost(Op::Claim));
        }
        Ok(outcome)
    }

    /// The common shape of the outcome calls: the item must exist and be in a state the call may leave.
    fn settle(
        &self,
        caller: Caller,
        op: Op,
        line: String,
        id: &str,
        apply: impl FnOnce(&mut Item, &Ctx) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let lose = self.enter(op, line)?;
        {
            let mut s = self.lock();
            if Self::caller_generation(&s, caller) != s.generation {
                return Err(StoreError::fatal("stale worker generation"));
            }
            // What the closure may read of the server: whether the user's cancel is queued and which session is current.
            let view = Ctx {
                cancel_queued: s.cancel_queued,
                epoch: s.epoch.clone(),
            };
            let Some(item) = s.items.iter_mut().find(|i| i.msg.id == id) else {
                return Err(StoreError::fatal(format!("unknown message {id}")));
            };
            apply(item, &view)?;
        }
        if lose {
            return Err(Self::lost(op));
        }
        Ok(())
    }

    async fn do_held(&self, caller: Caller, epoch: &str) -> Result<Vec<Held>, StoreError> {
        if let Some(d) = self.pause(Op::Claim) {
            tokio::time::sleep(d).await;
        }
        let lose = self.enter(Op::Claim, format!("held {epoch}"))?;
        let out = {
            let mut s = self.lock();
            let gen_caller = Self::caller_generation(&s, caller);
            if gen_caller != s.generation {
                return Err(StoreError::fatal(
                    "stale-generation: this worker was replaced",
                ));
            }
            let held: Vec<Held> = s
                .items
                .iter()
                .filter(|i| i.state == State::Dispatching && i.claimed_by == Some(gen_caller))
                .map(|i| Held {
                    message: i.msg.clone(),
                    redelivered: true,
                    current_session: i.msg.epoch == epoch,
                })
                .collect();
            let server_epoch = s.epoch.clone();
            let closed = s.cancel_queued || !s.admission_open;
            if !held.is_empty() {
                held
            } else if closed {
                Vec::new()
            } else if let Some(item) = s.items.iter_mut().find(|i| {
                i.state == State::Pending
                    && i.msg.epoch == epoch
                    && server_epoch.as_deref() == Some(epoch)
            }) {
                // nothing held: the question claims the next pending message, as a claim without an id does
                item.set(State::Dispatching);
                item.claimed_by = Some(gen_caller);
                vec![Held {
                    message: item.msg.clone(),
                    redelivered: false,
                    current_session: true,
                }]
            } else {
                Vec::new()
            }
        };
        if let Some(d) = self.pause_after(Op::Claim) {
            tokio::time::sleep(d).await;
        }
        if lose {
            return Err(Self::lost(Op::Claim));
        }
        Ok(out)
    }

    async fn do_seal(
        &self,
        caller: Caller,
        epoch: &str,
        mode: SealMode,
    ) -> Result<SealOutcome, StoreError> {
        if let Some(d) = self.pause(Op::Seal) {
            tokio::time::sleep(d).await;
        }
        let lose = self.enter(Op::Seal, format!("seal {epoch} {}", mode.as_str()))?;
        let outcome = {
            let mut s = self.lock();
            if Self::caller_generation(&s, caller) != s.generation {
                return Err(StoreError::fatal("stale worker generation"));
            }
            // A normal or limit seal while a message is still dispatching is a bad transition and changes nothing; the terminal seal
            // does not wait (a dispatching message becomes delivery-unknown).
            let dispatching: Vec<String> = s
                .items
                .iter()
                .filter(|i| i.state == State::Dispatching)
                .map(|i| i.msg.id.clone())
                .collect();
            if mode != SealMode::Terminal && !dispatching.is_empty() {
                return Err(StoreError::fatal(format!(
                    "bad-transition: messages still dispatching ({}); post their outcomes first",
                    dispatching.join(", ")
                )));
            }
            let eligible: Vec<Notice> = s
                .items
                .iter()
                .filter(|i| i.state == State::Pending && i.msg.epoch == epoch)
                .map(|i| i.msg.notice())
                .collect();
            if mode == SealMode::Normal && !eligible.is_empty() {
                SealOutcome::Pending(eligible)
            } else {
                s.admission_open = false;
                let cancelled = s.cancel_queued;
                for i in s.items.iter_mut() {
                    match i.state {
                        // The run is over: a message that was being sent has an outcome nobody will learn, and it is never sent again.
                        State::Dispatching if mode == SealMode::Terminal => {
                            i.set(State::DeliveryUnknown);
                        }
                        State::Pending
                            if mode == SealMode::LimitReached && i.msg.epoch == epoch =>
                        {
                            i.set(State::Rejected(RejectReason::ContinuationLimit));
                        }
                        State::Pending | State::Held => i.set(State::Rejected(if cancelled {
                            RejectReason::RunCancelled
                        } else {
                            RejectReason::RunSealed
                        })),
                        _ => {}
                    }
                }
                SealOutcome::Sealed {
                    watermark: s.accepted,
                }
            }
        };
        if lose {
            return Err(Self::lost(Op::Seal));
        }
        Ok(outcome)
    }

    fn call<'a>(&'a self, caller: Caller, call: Call<'a>) -> BoxFuture<'a, CallResult> {
        Box::pin(async move {
            match call {
                Call::Report(e, n) => self.do_report(caller, e, n).await.map(CallDone::Unit),
                Call::Claim(id, e) => self.do_claim(caller, id, e).await.map(CallDone::Claim),
                Call::Submitted(id, turn) => self
                    .settle(
                        caller,
                        Op::Submitted,
                        format!("submitted {id} {turn}"),
                        id,
                        |i, _| match i.state {
                            State::Dispatching => {
                                i.set(State::Delivered { turn });
                                Ok(())
                            }
                            State::Delivered { turn: t } if t == turn => Ok(()),
                            ref other => Err(StoreError::fatal(format!(
                                "cannot acknowledge a message that is {}",
                                other.name()
                            ))),
                        },
                    )
                    .map(CallDone::Unit),
                Call::Failed(id, why) => self
                    .settle(
                        caller,
                        Op::Failed,
                        format!("failed {id} {}", why.as_str()),
                        id,
                        |i, _| match i.state {
                            State::Dispatching => {
                                i.set(State::Failed(why));
                                Ok(())
                            }
                            State::Failed(w) if w == why => Ok(()),
                            ref other => Err(StoreError::fatal(format!(
                                "cannot fail a message that is {}",
                                other.name()
                            ))),
                        },
                    )
                    .map(CallDone::Unit),
                Call::Release(id) => self
                    .settle(
                        caller,
                        Op::Release,
                        format!("release {id}"),
                        id,
                        |i, server| match i.state {
                            State::Dispatching => {
                                i.claimed_by = None;
                                // A claim given back after the user's cancel ends rejected for it; one given back in another session
                                // than the current one is held for the user to confirm; otherwise it is pending again.
                                if server.cancel_queued {
                                    i.set(State::Rejected(RejectReason::RunCancelled));
                                } else if server.epoch.as_deref() != Some(i.msg.epoch.as_str()) {
                                    i.set(State::Held);
                                } else {
                                    i.set(State::Pending);
                                }
                                Ok(())
                            }
                            // The same release again (its answer was lost, or the cancel already settled it): nothing more to do.
                            State::Pending
                            | State::Held
                            | State::Rejected(RejectReason::RunCancelled)
                                if i.claimed_by.is_none()
                                    && i.history.contains(&State::Dispatching) =>
                            {
                                Ok(())
                            }
                            ref other => Err(StoreError::fatal(format!(
                                "cannot release a message that is {}",
                                other.name()
                            ))),
                        },
                    )
                    .map(CallDone::Unit),
                Call::Reject(id, why) => self
                    .settle(
                        caller,
                        Op::Reject,
                        format!("reject {id} {}", why.as_str()),
                        id,
                        |i, _| {
                            if !matches!(
                                why,
                                RejectReason::EngineBusy | RejectReason::ContinuationLimit
                            ) {
                                return Err(StoreError::fatal(format!(
                                    "an agent cannot reject for {}",
                                    why.as_str()
                                )));
                            }
                            match i.state {
                                State::Pending | State::Held => {
                                    i.set(State::Rejected(why));
                                    Ok(())
                                }
                                ref other => Err(StoreError::fatal(format!(
                                    "cannot reject a message that is {}",
                                    other.name()
                                ))),
                            }
                        },
                    )
                    .map(CallDone::Unit),
                Call::Seal(e, mode) => self.do_seal(caller, e, mode).await.map(CallDone::Seal),
            }
        })
    }
}

enum Call<'a> {
    Report(&'a str, u32),
    Claim(&'a str, &'a str),
    Submitted(&'a str, u32),
    Failed(&'a str, FailReason),
    Release(&'a str),
    Reject(&'a str, RejectReason),
    Seal(&'a str, SealMode),
}

enum CallDone {
    Unit(()),
    Claim(ClaimOutcome),
    Seal(SealOutcome),
}

type CallResult = Result<CallDone, StoreError>;

fn unit(r: CallResult) -> Result<(), StoreError> {
    r.map(|_| ())
}

fn claim_of(r: CallResult) -> Result<ClaimOutcome, StoreError> {
    match r? {
        CallDone::Claim(c) => Ok(c),
        _ => Err(StoreError::fatal("not a claim")),
    }
}

fn seal_of(r: CallResult) -> Result<SealOutcome, StoreError> {
    match r? {
        CallDone::Seal(c) => Ok(c),
        _ => Err(StoreError::fatal("not a seal")),
    }
}

/// What the blanket implementation of [`MessageStore`] needs: the model to call and the generation that calls it.
trait Resolve: Send + Sync {
    fn store(&self) -> &MemoryStore;
    fn caller(&self) -> Caller;
}

impl Resolve for MemoryStore {
    fn store(&self) -> &MemoryStore {
        self
    }

    fn caller(&self) -> Caller {
        Caller::Current
    }
}

/// A [`MemoryStore`] as one worker generation sees it.
#[derive(Debug)]
struct View {
    store: Arc<MemoryStore>,
    generation: u32,
}

impl Resolve for View {
    fn store(&self) -> &MemoryStore {
        &self.store
    }

    fn caller(&self) -> Caller {
        Caller::Generation(self.generation)
    }
}

impl<T: Resolve> MessageStore for T {
    fn report_epoch<'a>(
        &'a self,
        epoch: &'a str,
        number: u32,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            unit(
                self.store()
                    .call(self.caller(), Call::Report(epoch, number))
                    .await,
            )
        })
    }

    fn claim<'a>(
        &'a self,
        id: &'a str,
        epoch: &'a str,
    ) -> BoxFuture<'a, Result<ClaimOutcome, StoreError>> {
        Box::pin(async move {
            claim_of(
                self.store()
                    .call(self.caller(), Call::Claim(id, epoch))
                    .await,
            )
        })
    }

    fn held<'a>(&'a self, epoch: &'a str) -> BoxFuture<'a, Result<Vec<Held>, StoreError>> {
        Box::pin(async move { self.store().do_held(self.caller(), epoch).await })
    }

    fn unavailable(&self) -> bool {
        // a worker generation the control plane replaced is fenced for good (HTTP 409 stale-generation)
        match self.caller() {
            Caller::Current => false,
            Caller::Generation(g) => g != self.store().generation(),
        }
    }

    fn submitted<'a>(&'a self, id: &'a str, turn: u32) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            unit(
                self.store()
                    .call(self.caller(), Call::Submitted(id, turn))
                    .await,
            )
        })
    }

    fn failed<'a>(
        &'a self,
        id: &'a str,
        reason: FailReason,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            unit(
                self.store()
                    .call(self.caller(), Call::Failed(id, reason))
                    .await,
            )
        })
    }

    fn release<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move { unit(self.store().call(self.caller(), Call::Release(id)).await) })
    }

    fn reject<'a>(
        &'a self,
        id: &'a str,
        reason: RejectReason,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            unit(
                self.store()
                    .call(self.caller(), Call::Reject(id, reason))
                    .await,
            )
        })
    }

    fn seal<'a>(
        &'a self,
        epoch: &'a str,
        mode: SealMode,
    ) -> BoxFuture<'a, Result<SealOutcome, StoreError>> {
        Box::pin(async move {
            seal_of(
                self.store()
                    .call(self.caller(), Call::Seal(epoch, mode))
                    .await,
            )
        })
    }
}
