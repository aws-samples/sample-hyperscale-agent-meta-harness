//! The ledger: every change to a message of the user, as a pure rule applied to what was read and written back in one transaction.
//!
//! Every operation has the same shape. It reads the control item `MSGCTL` of the run (and the message items it will change), applies a rule
//! to copies, and writes the copies in one transaction that carries two conditions: the control item still has the version that was read,
//! and each message item still has the version and state that were read. When another request wrote in between, a condition fails, nothing
//! is written, and the operation starts again from the read. So a withdrawal and a claim of the same message cannot both succeed (the one
//! that writes second finds the version changed and reads the message in its new state), and neither can a late message and the seal: the
//! seal closes admission in the same write that checked nothing was pending, and an admission writes the control item too.

use crate::error::{LedgerError, Refusal};
use crate::table::{Cond, Item, PK, SK, Table, TableError, WriteOp, diff};
use harness_protocol::common::is_ulid;
use harness_protocol::messages::{
    Admission, ClaimRefusal, ClaimResponse, ClaimedMessage, GENERATION_CLOCK_SKEW_MS,
    MAX_CLAIM_BATCH, MAX_DETAIL_CHARS, MAX_HISTORY_STEPS, MAX_OUTSTANDING_MESSAGES,
    MAX_OWNER_ADMISSIONS_PER_MINUTE, MAX_RETIRED_GENERATIONS, MAX_RUN_ADMISSIONS_PER_MINUTE,
    MAX_RUN_MESSAGE_BYTES, MAX_RUN_MESSAGES, MessageCtlItem, MessageItem, MessageMode,
    MessageNotice, MessageOutcome, MessageState, MessageStep, MessageView, MessagesPage,
    OWNER_RATE_TTL_S, OpenMessage, OutcomeResponse, OwnerRateItem, RejectReason, SealMode,
    SealResponse, SendMessageRequest, SendOutcome, SessionAck, validate_send,
};
use harness_protocol::store::{Key, Keyed, keys};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

/// How many times an operation starts over when another request wrote in between.
const ATTEMPTS: usize = 8;
/// Longest session id (the engine's own id) the ledger stores.
const MAX_EPOCH_CHARS: usize = 256;

pub trait Clock: Send + Sync {
    /// Unix seconds.
    fn unix(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// A clock a test moves.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    pub fn at(unix: i64) -> Self {
        Self(Arc::new(AtomicI64::new(unix)))
    }
    pub fn set(&self, unix: i64) {
        self.0.store(unix, Ordering::SeqCst);
    }
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// One reading of the clock, in the two forms the ledger needs.
#[derive(Clone, Debug)]
pub struct Now {
    pub unix: i64,
    pub rfc3339: String,
}

impl Now {
    pub fn at(unix: i64) -> Self {
        Self {
            unix,
            rfc3339: rfc3339(unix),
        }
    }

    /// The clock minute (Unix minutes).
    pub fn minute(&self) -> u64 {
        u64::try_from(self.unix.max(0)).unwrap_or(0) / 60
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds (civil-from-days, Howard Hinnant).
pub fn rfc3339(unix: i64) -> String {
    let secs = unix.max(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// What a send stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    pub outcome: SendOutcome,
    pub message: MessageView,
}

/// Why admission is closed from outside agentd.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The run is finishing: pending and held messages end `rejected` (`run-sealed`), a dispatching one becomes `delivery-unknown`.
    Finalize,
    /// The user stopped the run: pending and held messages end `rejected` (`run-cancelled`); a dispatching one is left to agentd.
    Cancel,
}

/// What a close did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Closed {
    pub rejected: u32,
    pub delivery_unknown: u32,
    pub watermark: u32,
    /// The control item listed other messages as open than the message items say were open, and the close settled them from the items
    /// (only a finalize does that).
    pub repaired: bool,
}

/// Why reading a listed message did not give the state the control item lists.
#[derive(Debug)]
enum Differs {
    /// The control item changed meanwhile: an ordinary transition happened between the two reads, so read again.
    Raced,
    /// Nothing changed meanwhile, so the control item and the message disagree for good.
    Diverged(String),
}

impl Differs {
    /// `Ok` when the caller should read again, the fault when the disagreement is real.
    fn retry(self) -> Result<(), LedgerError> {
        match self {
            Differs::Raced => Ok(()),
            Differs::Diverged(what) => Err(corrupt(what)),
        }
    }
}

/// How a control item lists `m`, if it does not list it the way `m` is: an open message is listed in its state and session, a settled one
/// is not listed.
fn disagreement(ctl: &MessageCtlItem, m: &MessageItem) -> Option<String> {
    match (m.state.is_open(), ctl.find(&m.message_id)) {
        (true, Some(o)) if o.state == m.state && o.epoch == m.epoch => None,
        (true, Some(o)) => Some(format!(
            "the control item lists message {} as {} in {:?} and the message is {} in {:?}",
            m.message_id,
            o.state.word(),
            o.epoch,
            m.state.word(),
            m.epoch
        )),
        (true, None) => Some(format!(
            "message {} is {} and the control item does not list it",
            m.message_id,
            m.state.word()
        )),
        (false, Some(o)) => Some(format!(
            "the control item lists message {} as {} and the message is {}",
            m.message_id,
            o.state.word(),
            m.state.word()
        )),
        (false, None) => None,
    }
}

/// The time of a ULID in Unix milliseconds (its first 10 characters, Crockford base 32).
fn ulid_ms(s: &str) -> Option<u64> {
    let mut v: u64 = 0;
    for c in s.bytes().take(10) {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'A'..=b'H' => c - b'A' + 10,
            b'J'..=b'K' => c - b'J' + 18,
            b'M'..=b'N' => c - b'M' + 20,
            b'P'..=b'T' => c - b'P' + 22,
            b'V'..=b'Z' => c - b'V' + 27,
            _ => return None,
        };
        v = v.checked_mul(32)?.checked_add(u64::from(d))?;
    }
    Some(v)
}

fn open_entry(m: &MessageItem) -> OpenMessage {
    OpenMessage {
        message_id: m.message_id.clone(),
        seq: m.seq,
        mode: m.mode,
        state: m.state,
        epoch: m.epoch.clone(),
        bytes: u32::try_from(m.text.len()).unwrap_or(u32::MAX),
        accepted_at: m.accepted_at.clone(),
    }
}

#[derive(Debug)]
pub struct Ledger<T, C = SystemClock> {
    table: T,
    clock: C,
}

impl<T: Table> Ledger<T, SystemClock> {
    pub fn new(table: T) -> Self {
        Self {
            table,
            clock: SystemClock,
        }
    }
}

fn corrupt(what: impl std::fmt::Display) -> LedgerError {
    LedgerError::Unavailable(format!("a stored message item cannot be read: {what}"))
}

fn to_item<K: Serialize + Keyed>(k: &K) -> Result<Item, LedgerError> {
    let Key { pk, sk } = k.key();
    let Value::Object(mut m) = serde_json::to_value(k).map_err(corrupt)? else {
        return Err(corrupt("an item is not an object"));
    };
    m.insert(PK.into(), Value::String(pk));
    m.insert(SK.into(), Value::String(sk));
    Ok(m)
}

fn from_item<K: DeserializeOwned>(i: Item) -> Result<K, LedgerError> {
    serde_json::from_value(Value::Object(i)).map_err(corrupt)
}

fn state_value(s: MessageState) -> Value {
    json!(s.word())
}

fn enter(
    m: &mut MessageItem,
    ctl: &mut MessageCtlItem,
    to: MessageState,
    reason: Option<RejectReason>,
    turn: Option<u32>,
    now: &Now,
) {
    m.state = to;
    if to == MessageState::Rejected {
        m.reason = reason;
    }
    if to == MessageState::Delivered {
        m.delivered_turn = turn;
    }
    m.updated_at = now.rfc3339.clone();
    m.history.push(MessageStep {
        state: to,
        at: now.rfc3339.clone(),
        reason: if to == MessageState::Rejected {
            reason
        } else {
            None
        },
        turn,
    });
    while m.history.len() > MAX_HISTORY_STEPS {
        m.history.remove(1);
    }
    if to.is_open() {
        if let Some(o) = ctl.open.iter_mut().find(|o| o.message_id == m.message_id) {
            o.state = to;
        }
    } else {
        ctl.open.retain(|o| o.message_id != m.message_id);
    }
}

/// agentd's incarnation and session as the ledger checks them for a claim or a seal.
fn fence(
    ctl: &MessageCtlItem,
    existed: bool,
    generation: &str,
    epoch: &str,
) -> Result<(), Refusal> {
    if !existed || ctl.generation.is_none() {
        return Err(Refusal::NoSession);
    }
    if ctl.generation.as_deref() != Some(generation) {
        return Err(Refusal::StaleGeneration);
    }
    if ctl.epoch.as_deref() != Some(epoch) {
        return Err(Refusal::StaleEpoch);
    }
    Ok(())
}

fn check_ids(generation: &str, epoch: &str) -> Result<(), Refusal> {
    if !is_ulid(generation) {
        return Err(Refusal::BadInput(
            "generation must be a ULID made when agentd started".into(),
        ));
    }
    if epoch.trim().is_empty() || epoch.chars().count() > MAX_EPOCH_CHARS {
        return Err(Refusal::BadInput(format!(
            "epoch is the engine's session id (1 to {MAX_EPOCH_CHARS} characters)"
        )));
    }
    Ok(())
}

/// What a transaction is made of.
struct Change {
    ctl_old: MessageCtlItem,
    existed: bool,
    ctl: MessageCtlItem,
    /// Message items that change: the item as read and as it will be written.
    msgs: Vec<(MessageItem, MessageItem)>,
    /// Further operations: a new message, the owner's counter.
    extra: Vec<WriteOp>,
}

enum Committed {
    Done,
    Retry,
}

impl<T: Table, C: Clock> Ledger<T, C> {
    pub fn with_clock(table: T, clock: C) -> Self {
        Self { table, clock }
    }

    pub fn table(&self) -> &T {
        &self.table
    }

    fn now(&self) -> Now {
        Now::at(self.clock.unix())
    }

    async fn load_ctl(&self, run_id: &str) -> Result<(MessageCtlItem, bool), LedgerError> {
        match self
            .table
            .get(&keys::run_pk(run_id), keys::MESSAGE_CTL)
            .await?
        {
            Some(i) => Ok((from_item(i)?, true)),
            None => Ok((MessageCtlItem::new(run_id), false)),
        }
    }

    async fn load_message(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageItem>, LedgerError> {
        self.table
            .get(&keys::run_pk(run_id), &keys::message_sk(message_id))
            .await?
            .map(from_item)
            .transpose()
    }

    /// `Raced` when the control item is no longer the version that was read, `Diverged` when it is: a message that differs from its entry
    /// while nothing changed is not an ordinary transition.
    async fn differs(
        &self,
        run_id: &str,
        ctl_read: &MessageCtlItem,
        what: String,
    ) -> Result<Differs, LedgerError> {
        let (now, _) = self.load_ctl(run_id).await?;
        Ok(if now.version == ctl_read.version {
            Differs::Diverged(what)
        } else {
            Differs::Raced
        })
    }

    /// The items of the messages the control item lists as open, each in the state and session it is listed in.
    async fn load_open(
        &self,
        run_id: &str,
        ctl: &MessageCtlItem,
    ) -> Result<Result<Vec<MessageItem>, Differs>, LedgerError> {
        let mut out = Vec::with_capacity(ctl.open.len());
        for o in &ctl.open {
            let Some(m) = self.load_message(run_id, &o.message_id).await? else {
                let what = format!(
                    "the control item lists message {} and it does not exist",
                    o.message_id
                );
                return Ok(Err(self.differs(run_id, ctl, what).await?));
            };
            // The control item and the message are written in one transaction, so they agree. When they do not, something else wrote
            // one of them; acting on either would be a guess.
            if m.state != o.state || m.epoch != o.epoch {
                let what = format!(
                    "the control item lists message {} as {} in {:?} and the message is {} in {:?}",
                    o.message_id,
                    o.state.word(),
                    o.epoch,
                    m.state.word(),
                    m.epoch
                );
                return Ok(Err(self.differs(run_id, ctl, what).await?));
            }
            out.push(m);
        }
        Ok(Ok(out))
    }

    /// The open messages as the message items say, and the control item with its list made to match: what a finalize settles from, because
    /// it has to end the run whatever the list says. One consistent query of the run's messages (at most 100 items).
    async fn repair_open(
        &self,
        run_id: &str,
        ctl_old: &MessageCtlItem,
    ) -> Result<(MessageCtlItem, Vec<MessageItem>), LedgerError> {
        let items = self
            .table
            .query(&keys::run_pk(run_id), keys::MESSAGE_PREFIX)
            .await?;
        let mut open: Vec<MessageItem> = items
            .into_iter()
            .map(from_item::<MessageItem>)
            .collect::<Result<_, _>>()?;
        open.retain(|m| m.state.is_open());
        open.sort_by_key(|m| m.seq);
        let mut ctl = ctl_old.clone();
        ctl.open = open.iter().map(open_entry).collect();
        Ok((ctl, open))
    }

    /// Writes the change in one transaction. `Retry` means another request wrote in between: read again.
    async fn commit(&self, ch: Change, round: usize) -> Result<Committed, LedgerError> {
        let mut ops = Vec::with_capacity(2 + ch.msgs.len() + ch.extra.len());
        let mut ctl = ch.ctl;
        ctl.version = ch.ctl_old.version + 1;
        let new_item = to_item(&ctl)?;
        let ctl_key = ctl.key();
        if ch.existed {
            let (set, remove) = diff(&to_item(&ch.ctl_old)?, &new_item);
            ops.push(WriteOp::Update {
                pk: ctl_key.pk,
                sk: ctl_key.sk,
                set,
                remove,
                cond: Some(Cond::And(vec![
                    Cond::Exists(PK),
                    Cond::Eq("version", json!(ch.ctl_old.version)),
                ])),
            });
        } else {
            // The first write of the control item is an update that creates it, guarded so it cannot replace one: worker-api and
            // run-session hold `dynamodb:UpdateItem` and not `PutItem` on this partition.
            let (set, remove) = diff(&Item::new(), &new_item);
            ops.push(WriteOp::Update {
                pk: ctl_key.pk,
                sk: ctl_key.sk,
                set,
                remove,
                cond: Some(Cond::NotExists(PK)),
            });
        }
        for (old, new) in ch.msgs {
            let mut new = new;
            new.version = old.version + 1;
            let (set, remove) = diff(&to_item(&old)?, &to_item(&new)?);
            let k = new.key();
            ops.push(WriteOp::Update {
                pk: k.pk,
                sk: k.sk,
                set,
                remove,
                cond: Some(Cond::And(vec![
                    Cond::Exists(PK),
                    Cond::Eq("version", json!(old.version)),
                    Cond::Eq("state", state_value(old.state)),
                ])),
            });
        }
        ops.extend(ch.extra);
        match self.table.transact(ops).await {
            Ok(()) => Ok(Committed::Done),
            Err(TableError::ConditionFailed { .. }) => Ok(Committed::Retry),
            Err(TableError::Contended(_)) => {
                let ms = 20u64 * (round as u64 + 1);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(Committed::Retry)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn contended() -> LedgerError {
        LedgerError::Unavailable(
            "too many requests changed the run's messages at once; retry".to_string(),
        )
    }

    // ---- reads ------------------------------------------------------------------------------------------------------

    /// One message of the run.
    pub async fn get_message(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageView>, LedgerError> {
        Ok(self
            .load_message(run_id, message_id)
            .await?
            .map(|m| m.view()))
    }

    /// Every message of the run in the order it was admitted, with the session and the admission state.
    pub async fn list(&self, run_id: &str) -> Result<MessagesPage, LedgerError> {
        let (ctl, _) = self.load_ctl(run_id).await?;
        let items = self
            .table
            .query(&keys::run_pk(run_id), keys::MESSAGE_PREFIX)
            .await?;
        let mut messages: Vec<MessageItem> =
            items.into_iter().map(from_item).collect::<Result<_, _>>()?;
        messages.sort_by_key(|m| m.seq);
        Ok(MessagesPage {
            run_id: run_id.to_string(),
            messages: messages.iter().map(MessageItem::view).collect(),
            admission: ctl.admission,
            epoch: ctl.epoch.clone(),
            epoch_no: ctl.epoch_no,
            watermark: ctl.seq,
        })
    }

    /// The notices agentd is handed: the pending messages of the current session, in the order claims take them. One read of one item.
    pub async fn notices(&self, run_id: &str) -> Result<Vec<MessageNotice>, LedgerError> {
        let (ctl, existed) = self.load_ctl(run_id).await?;
        if !existed {
            return Ok(Vec::new());
        }
        Ok(eligible(&ctl)
            .into_iter()
            .map(OpenMessage::notice)
            .collect())
    }

    // ---- the owner's calls ------------------------------------------------------------------------------------------

    /// Admits a message: one transaction writes the message, the control item (a new count, the new `seq`, the rate window) and the owner's
    /// counter, and, for a confirmation, supersedes the held original. An id that was admitted before returns the stored message when the
    /// payload is the same and is refused when it is not.
    pub async fn admit(
        &self,
        run_id: &str,
        principal: &str,
        req: &SendMessageRequest,
    ) -> Result<Admitted, LedgerError> {
        validate_send(req).map_err(|e| Refusal::BadInput(e.to_string()))?;
        let sha = req.payload_sha256();
        for round in 0..ATTEMPTS {
            let now = self.now();
            if let Some(m) = self.load_message(run_id, &req.message_id).await? {
                if m.principal_arn != principal {
                    return Err(Refusal::NotFound.into());
                }
                if m.payload_sha256 != sha {
                    return Err(Refusal::IdempotencyConflict.into());
                }
                return Ok(Admitted {
                    outcome: SendOutcome::AlreadyAccepted,
                    message: m.view(),
                });
            }
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            if ctl_old.admission == Admission::Closed {
                return Err(Refusal::RunSealed(ctl_old.closed_reason).into());
            }
            let mut ctl = ctl_old.clone();
            // A confirmation replaces a held (or unknown) message, once, and only for the session the user was shown.
            let mut original: Option<(MessageItem, MessageItem)> = None;
            if let Some(c) = &req.confirms {
                let Some(orig) = self.load_message(run_id, &c.message_id).await? else {
                    return Err(Refusal::NotFound.into());
                };
                if orig.principal_arn != principal {
                    return Err(Refusal::NotFound.into());
                }
                if let Some(what) = disagreement(&ctl_old, &orig) {
                    self.differs(run_id, &ctl_old, what).await?.retry()?;
                    continue;
                }
                if !matches!(
                    orig.state,
                    MessageState::Held | MessageState::DeliveryUnknown
                ) {
                    return Err(Refusal::NotHeld(orig.state).into());
                }
                if ctl.epoch.as_deref() != Some(c.epoch.as_str()) {
                    return Err(Refusal::EpochChanged.into());
                }
                if orig.text != req.text || orig.mode != req.mode {
                    return Err(Refusal::ConfirmMismatch.into());
                }
                let mut next = orig.clone();
                next.superseded_by = Some(req.message_id.clone());
                enter(
                    &mut next,
                    &mut ctl,
                    MessageState::Superseded,
                    None,
                    None,
                    &now,
                );
                original = Some((orig, next));
            }
            // The budget counts the bytes the text takes inside JSON, which is what the list of the run answers with.
            let bytes = harness_protocol::direct::json_len(&req.text);
            if ctl.seq >= MAX_RUN_MESSAGES {
                return Err(Refusal::RunLimit(format!(
                    "this run has taken {MAX_RUN_MESSAGES} messages; start a follow-up run"
                ))
                .into());
            }
            if ctl.bytes_total + bytes as u64 > MAX_RUN_MESSAGE_BYTES {
                return Err(Refusal::RunLimit(format!(
                    "this run has taken {} KiB of messages; start a follow-up run",
                    MAX_RUN_MESSAGE_BYTES / 1024
                ))
                .into());
            }
            if ctl.outstanding() + 1 > MAX_OUTSTANDING_MESSAGES {
                return Err(Refusal::TooManyPending.into());
            }
            let minute = now.minute();
            let run_count = if ctl.rate_minute == minute {
                ctl.rate_count
            } else {
                0
            };
            if run_count + 1 > MAX_RUN_ADMISSIONS_PER_MINUTE {
                return Err(Refusal::RateLimited("run").into());
            }
            let owner_old = self
                .table
                .get(
                    &keys::owner_rate_pk(principal),
                    &keys::owner_rate_sk(minute),
                )
                .await?
                .map(from_item::<OwnerRateItem>)
                .transpose()?;
            let owner_count = owner_old.as_ref().map_or(0, |r| r.count);
            if owner_count + 1 > MAX_OWNER_ADMISSIONS_PER_MINUTE {
                return Err(Refusal::RateLimited("owner").into());
            }
            let seq = ctl.seq + 1;
            let item = MessageItem {
                run_id: run_id.to_string(),
                message_id: req.message_id.clone(),
                seq,
                principal_arn: principal.to_string(),
                text: req.text.clone(),
                mode: req.mode,
                epoch: ctl.epoch.clone(),
                epoch_no: ctl.epoch_no,
                state: MessageState::Pending,
                reason: None,
                detail: None,
                delivered_turn: None,
                confirms: req.confirms.as_ref().map(|c| c.message_id.clone()),
                superseded_by: None,
                claimed_by: None,
                attempt: 0,
                payload_sha256: sha.clone(),
                accepted_at: now.rfc3339.clone(),
                updated_at: now.rfc3339.clone(),
                history: vec![MessageStep {
                    state: MessageState::Pending,
                    at: now.rfc3339.clone(),
                    reason: None,
                    turn: None,
                }],
                version: 1,
            };
            ctl.seq = seq;
            ctl.bytes_total += bytes as u64;
            ctl.rate_minute = minute;
            ctl.rate_count = run_count + 1;
            ctl.open.push(OpenMessage {
                message_id: item.message_id.clone(),
                seq,
                mode: item.mode,
                state: MessageState::Pending,
                epoch: item.epoch.clone(),
                bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
                accepted_at: now.rfc3339.clone(),
            });
            let rate = OwnerRateItem {
                principal_arn: principal.to_string(),
                minute,
                count: owner_count + 1,
                ttl: (minute + 1) * 60 + OWNER_RATE_TTL_S,
            };
            let rate_cond = if owner_old.is_some() {
                Cond::Eq("count", json!(owner_count))
            } else {
                Cond::NotExists(PK)
            };
            let extra = vec![
                WriteOp::Put {
                    item: to_item(&item)?,
                    cond: Some(Cond::NotExists(PK)),
                },
                WriteOp::Put {
                    item: to_item(&rate)?,
                    cond: Some(rate_cond),
                },
            ];
            let mut msgs = Vec::new();
            if let Some(pair) = original {
                msgs.push(pair);
            }
            let view = item.view();
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs,
                extra,
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(Admitted {
                    outcome: SendOutcome::Accepted,
                    message: view,
                });
            }
        }
        Err(Self::contended())
    }

    /// Withdraws a message nobody has claimed (pending or held). A withdrawn message is withdrawn again without a write.
    pub async fn withdraw(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<MessageView, LedgerError> {
        for round in 0..ATTEMPTS {
            let now = self.now();
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            let Some(old) = self.load_message(run_id, message_id).await? else {
                return Err(Refusal::NotFound.into());
            };
            if let Some(what) = disagreement(&ctl_old, &old) {
                self.differs(run_id, &ctl_old, what).await?.retry()?;
                continue;
            }
            match old.state {
                MessageState::Withdrawn => return Ok(old.view()),
                s if s.can_withdraw() => {}
                MessageState::Dispatching => return Err(Refusal::AlreadyDispatching.into()),
                s => return Err(Refusal::NotWithdrawable(s).into()),
            }
            let mut ctl = ctl_old.clone();
            let mut new = old.clone();
            enter(
                &mut new,
                &mut ctl,
                MessageState::Withdrawn,
                None,
                None,
                &now,
            );
            let view = new.view();
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs: vec![(old, new)],
                extra: Vec::new(),
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(view);
            }
        }
        Err(Self::contended())
    }

    // ---- agentd's calls ----------------------------------------------------------------------------------------------

    /// agentd reports its incarnation and its session. The server commits both before any later claim.
    ///
    /// `number` is the ordinal of the session within the incarnation (1 for its first). A report with a lower number than the last one of
    /// the same incarnation is the late retry of an older report and changes nothing; the same number with another session id is refused.
    /// An incarnation the server has not seen replaces the current one, unless its ULID is older than the current one's by more than
    /// [`GENERATION_CLOCK_SKEW_MS`] (the delayed first report of one that was replaced already); one it has retired is refused.
    pub async fn report_session(
        &self,
        run_id: &str,
        generation: &str,
        epoch: &str,
        number: u32,
    ) -> Result<SessionAck, LedgerError> {
        check_ids(generation, epoch)?;
        if number == 0 {
            return Err(Refusal::BadInput(
                "number counts the sessions of an agentd incarnation from 1".into(),
            )
            .into());
        }
        for round in 0..ATTEMPTS {
            let now = self.now();
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            let same = ctl_old.generation.as_deref() == Some(generation);
            if !same && ctl_old.retired.iter().any(|g| g == generation) {
                return Err(Refusal::StaleGeneration.into());
            }
            // An incarnation the server has not seen replaces the current one, unless it started long before it: that is the delayed
            // first report of one that was replaced already, and it must not take the run back.
            if !same
                && let (Some(current), Some(new)) = (
                    ctl_old.generation.as_deref().and_then(ulid_ms),
                    ulid_ms(generation),
                )
                && current.saturating_sub(new) > GENERATION_CLOCK_SKEW_MS
            {
                return Err(Refusal::StaleGeneration.into());
            }
            let ack = |ctl: &MessageCtlItem, held, unknown| SessionAck {
                epoch_no: ctl.epoch_no,
                held,
                delivery_unknown: unknown,
                admission: ctl.admission,
            };
            if same {
                if number < ctl_old.session_number {
                    return Ok(ack(&ctl_old, 0, 0));
                }
                if number == ctl_old.session_number {
                    if ctl_old.epoch.as_deref() == Some(epoch) {
                        return Ok(ack(&ctl_old, 0, 0));
                    }
                    return Err(Refusal::BadTransition(format!(
                        "session number {number} was already reported for another session"
                    ))
                    .into());
                }
            }
            let replaced = ctl_old.generation.is_some() && !same;
            let epoch_changed = ctl_old.epoch.as_deref() != Some(epoch);
            let mut ctl = ctl_old.clone();
            let first_session = ctl.epoch.is_none();
            let open = if replaced || epoch_changed {
                match self.load_open(run_id, &ctl_old).await? {
                    Ok(items) => items,
                    Err(d) => {
                        d.retry()?;
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
            let (mut held, mut unknown) = (0u32, 0u32);
            let mut msgs = Vec::new();
            if replaced && let Some(old_generation) = ctl_old.generation.clone() {
                ctl.retired.push(old_generation);
                while ctl.retired.len() > MAX_RETIRED_GENERATIONS {
                    ctl.retired.remove(0);
                }
            }
            ctl.generation = Some(generation.to_string());
            ctl.session_number = number;
            if epoch_changed {
                ctl.epoch = Some(epoch.to_string());
                ctl.epoch_no += 1;
            }
            for old in open {
                let mut new = old.clone();
                let mut changed = false;
                if replaced && old.state == MessageState::Dispatching {
                    enter(
                        &mut new,
                        &mut ctl,
                        MessageState::DeliveryUnknown,
                        None,
                        None,
                        &now,
                    );
                    unknown += 1;
                    changed = true;
                } else if epoch_changed && old.state == MessageState::Pending {
                    if first_session && old.epoch.is_none() {
                        // The first session is not a change of session: a message sent before it was reported belongs to it.
                        new.epoch = Some(epoch.to_string());
                        new.epoch_no = ctl.epoch_no;
                        new.updated_at = now.rfc3339.clone();
                        if let Some(o) =
                            ctl.open.iter_mut().find(|o| o.message_id == old.message_id)
                        {
                            o.epoch = Some(epoch.to_string());
                        }
                    } else {
                        enter(&mut new, &mut ctl, MessageState::Held, None, None, &now);
                        held += 1;
                    }
                    changed = true;
                }
                if changed {
                    msgs.push((old, new));
                }
            }
            let result = ack(&ctl, held, unknown);
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs,
                extra: Vec::new(),
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(result);
            }
        }
        Err(Self::contended())
    }

    /// agentd claims pending messages before it sends them: each moves from pending to dispatching in the same write that checked it was
    /// pending in the current session, for the current agentd. `cancel_queued` is true while a run-level Cancel waits for agentd: it wins,
    /// and the open messages end `rejected` (`run-cancelled`).
    pub async fn claim(
        &self,
        run_id: &str,
        generation: &str,
        epoch: &str,
        message_id: Option<&str>,
        limit: u8,
        cancel_queued: bool,
    ) -> Result<ClaimResponse, LedgerError> {
        check_ids(generation, epoch)?;
        let take = usize::from(limit.clamp(1, MAX_CLAIM_BATCH));
        'rounds: for round in 0..ATTEMPTS {
            let now = self.now();
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            fence(&ctl_old, existed, generation, epoch)?;
            if cancel_queued {
                if ctl_old.admission == Admission::Open {
                    let open = match self.load_open(run_id, &ctl_old).await? {
                        Ok(items) => items,
                        Err(d) => {
                            d.retry()?;
                            continue 'rounds;
                        }
                    };
                    let (ctl, msgs, _) =
                        apply_close(&ctl_old, open, RejectReason::RunCancelled, false, &now);
                    let change = Change {
                        ctl_old,
                        existed,
                        ctl,
                        msgs,
                        extra: Vec::new(),
                    };
                    if let Committed::Retry = self.commit(change, round).await? {
                        continue;
                    }
                }
                return Ok(ClaimResponse::Refused {
                    reason: ClaimRefusal::CancelQueued,
                });
            }
            // A claim whose answer was lost is asked again: the messages this generation holds in `dispatching` come back, unchanged.
            let held = match self
                .redeliver(run_id, &ctl_old, generation, message_id)
                .await?
            {
                Ok(held) => held,
                Err(d) => {
                    d.retry()?;
                    continue 'rounds;
                }
            };
            if !held.is_empty() {
                return Ok(ClaimResponse::Claimed {
                    messages: held,
                    redelivered: true,
                });
            }
            let picks: Vec<OpenMessage> = match message_id {
                Some(id) => match ctl_old.find(id) {
                    Some(o) if o.state == MessageState::Pending && o.epoch == ctl_old.epoch => {
                        vec![o.clone()]
                    }
                    Some(o) if o.state == MessageState::Pending => {
                        let what = format!(
                            "the control item lists message {id} as pending in {:?} and the session is {:?}",
                            o.epoch, ctl_old.epoch
                        );
                        self.differs(run_id, &ctl_old, what).await?.retry()?;
                        continue 'rounds;
                    }
                    Some(o) => return Ok(ClaimResponse::NotPending { state: o.state }),
                    None => {
                        return match self.load_message(run_id, id).await? {
                            Some(m) => Ok(ClaimResponse::NotPending { state: m.state }),
                            None => Err(Refusal::NotFound.into()),
                        };
                    }
                },
                None => {
                    if ctl_old.admission == Admission::Closed {
                        return Ok(ClaimResponse::Refused {
                            reason: ClaimRefusal::AdmissionClosed,
                        });
                    }
                    eligible(&ctl_old).into_iter().take(take).cloned().collect()
                }
            };
            if message_id.is_some() && ctl_old.admission == Admission::Closed {
                return Ok(ClaimResponse::Refused {
                    reason: ClaimRefusal::AdmissionClosed,
                });
            }
            if picks.is_empty() {
                return Ok(ClaimResponse::Nothing {
                    admission: ctl_old.admission,
                });
            }
            let mut ctl = ctl_old.clone();
            let mut msgs = Vec::new();
            let mut claimed = Vec::new();
            for o in &picks {
                let Some(old) = self.load_message(run_id, &o.message_id).await? else {
                    let what = format!("message {} is listed and missing", o.message_id);
                    self.differs(run_id, &ctl_old, what).await?.retry()?;
                    continue 'rounds;
                };
                // The control item says this message is pending in this session; the message must say the same.
                if old.state != MessageState::Pending || old.epoch != o.epoch {
                    let what = format!(
                        "the control item lists message {} as pending in {:?} and the message is {} in {:?}",
                        o.message_id,
                        o.epoch,
                        old.state.word(),
                        old.epoch
                    );
                    self.differs(run_id, &ctl_old, what).await?.retry()?;
                    continue 'rounds;
                }
                let mut new = old.clone();
                new.claimed_by = Some(generation.to_string());
                new.attempt = old.attempt + 1;
                enter(
                    &mut new,
                    &mut ctl,
                    MessageState::Dispatching,
                    None,
                    None,
                    &now,
                );
                claimed.push(ClaimedMessage {
                    message_id: new.message_id.clone(),
                    seq: new.seq,
                    mode: new.mode,
                    epoch_no: new.epoch_no,
                    attempt: new.attempt,
                    text: new.text.clone(),
                    accepted_at: new.accepted_at.clone(),
                });
                msgs.push((old, new));
            }
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs,
                extra: Vec::new(),
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(ClaimResponse::Claimed {
                    messages: claimed,
                    redelivered: false,
                });
            }
        }
        Err(Self::contended())
    }

    /// The messages `generation` already holds in `dispatching` (all of them, or the one named): what a claim whose answer was lost must be
    /// given again, also when the session changed since. Nothing is written. A listed message that is not what its entry says is a
    /// disagreement, not a message to skip.
    async fn redeliver(
        &self,
        run_id: &str,
        ctl: &MessageCtlItem,
        generation: &str,
        message_id: Option<&str>,
    ) -> Result<Result<Vec<ClaimedMessage>, Differs>, LedgerError> {
        let mut out = Vec::new();
        for o in ctl
            .open
            .iter()
            .filter(|o| o.state == MessageState::Dispatching)
            .filter(|o| message_id.is_none_or(|id| id == o.message_id))
        {
            let Some(m) = self.load_message(run_id, &o.message_id).await? else {
                let what = format!("message {} is listed and missing", o.message_id);
                return Ok(Err(self.differs(run_id, ctl, what).await?));
            };
            if m.state != MessageState::Dispatching
                || m.claimed_by.as_deref() != Some(generation)
                || m.epoch != o.epoch
            {
                let what = format!(
                    "the control item lists message {} as dispatching and the message is {} (claimed by {:?})",
                    o.message_id,
                    m.state.word(),
                    m.claimed_by
                );
                return Ok(Err(self.differs(run_id, ctl, what).await?));
            }
            out.push(ClaimedMessage {
                message_id: m.message_id,
                seq: m.seq,
                mode: m.mode,
                epoch_no: m.epoch_no,
                attempt: m.attempt,
                text: m.text,
                accepted_at: m.accepted_at,
            });
        }
        Ok(Ok(out))
    }

    /// agentd's one atomic question before it declares the run done. See [`SealMode`].
    ///
    /// A `normal` or `limit-reached` seal is refused while agentd still owes the outcome of a message it claimed: it posts the outcomes
    /// first. The `terminal` seal does not wait, because agentd is going away; what it still holds becomes `delivery-unknown`. On a run that
    /// is closed already, a seal is the closed answer, and the terminal seal of the current incarnation also settles what was left
    /// `dispatching` (a cancel leaves it to agentd).
    pub async fn seal(
        &self,
        run_id: &str,
        generation: &str,
        epoch: &str,
        mode: SealMode,
    ) -> Result<SealResponse, LedgerError> {
        check_ids(generation, epoch)?;
        for round in 0..ATTEMPTS {
            let now = self.now();
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            if existed && ctl_old.admission == Admission::Closed {
                // Only the current incarnation holds anything to settle; anybody else gets the closed answer.
                if fence(&ctl_old, existed, generation, epoch).is_ok() {
                    if mode != SealMode::Terminal {
                        if let Some(owed) = owed_outcomes(&ctl_old) {
                            return Err(Refusal::BadTransition(owed).into());
                        }
                    } else if !ctl_old.open.is_empty() {
                        let open = match self.load_open(run_id, &ctl_old).await? {
                            Ok(items) => items,
                            Err(d) => {
                                d.retry()?;
                                continue;
                            }
                        };
                        let (ctl, msgs, closed) =
                            apply_close(&ctl_old, open, RejectReason::RunSealed, true, &now);
                        if !msgs.is_empty() {
                            let watermark = ctl_old.seq;
                            let change = Change {
                                ctl_old,
                                existed,
                                ctl,
                                msgs,
                                extra: Vec::new(),
                            };
                            if let Committed::Done = self.commit(change, round).await? {
                                return Ok(SealResponse::Sealed {
                                    watermark,
                                    rejected: closed.rejected,
                                    delivery_unknown: closed.delivery_unknown,
                                });
                            }
                            continue;
                        }
                    }
                }
                return Ok(SealResponse::Sealed {
                    watermark: ctl_old.seq,
                    rejected: 0,
                    delivery_unknown: 0,
                });
            }
            fence(&ctl_old, existed, generation, epoch)?;
            if mode != SealMode::Terminal
                && let Some(owed) = owed_outcomes(&ctl_old)
            {
                return Err(Refusal::BadTransition(owed).into());
            }
            if mode == SealMode::Normal {
                let pending: Vec<MessageNotice> = eligible(&ctl_old)
                    .into_iter()
                    .map(OpenMessage::notice)
                    .collect();
                if !pending.is_empty() {
                    return Ok(SealResponse::Pending { messages: pending });
                }
            }
            let reason = match mode {
                SealMode::LimitReached => RejectReason::ContinuationLimit,
                _ => RejectReason::RunSealed,
            };
            let open = match self.load_open(run_id, &ctl_old).await? {
                Ok(items) => items,
                Err(d) => {
                    d.retry()?;
                    continue;
                }
            };
            let (ctl, msgs, closed) = apply_close(&ctl_old, open, reason, true, &now);
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs,
                extra: Vec::new(),
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(SealResponse::Sealed {
                    watermark: closed.watermark,
                    rejected: closed.rejected,
                    delivery_unknown: closed.delivery_unknown,
                });
            }
        }
        Err(Self::contended())
    }

    /// What agentd did with a message it claimed (or will not take).
    ///
    /// An outcome is fenced by the incarnation and by the claim: `attempt` is the number of the claim it answers, so the late retry of an
    /// earlier attempt cannot settle a later one. `epoch` is informative; the session may have changed between the claim and the outcome,
    /// and the outcome of a message agentd really sent must still be recorded. `attempt` 0 is for `rejected` of a message agentd never
    /// claimed.
    pub async fn outcome(
        &self,
        run_id: &str,
        generation: &str,
        epoch: &str,
        message_id: &str,
        attempt: u32,
        outcome: &MessageOutcome,
    ) -> Result<OutcomeResponse, LedgerError> {
        check_ids(generation, epoch)?;
        if let MessageOutcome::Rejected { reason } = outcome
            && !reason.agent_may_set()
        {
            return Err(Refusal::BadTransition(format!(
                "agentd cannot reject a message as {}",
                reason.word()
            ))
            .into());
        }
        for round in 0..ATTEMPTS {
            let now = self.now();
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            if !existed || ctl_old.generation.is_none() {
                return Err(Refusal::NoSession.into());
            }
            if ctl_old.generation.as_deref() != Some(generation) {
                return Err(Refusal::StaleGeneration.into());
            }
            let Some(old) = self.load_message(run_id, message_id).await? else {
                return Err(Refusal::NotFound.into());
            };
            if let Some(what) = disagreement(&ctl_old, &old) {
                self.differs(run_id, &ctl_old, what).await?.retry()?;
                continue;
            }
            let mut ctl = ctl_old.clone();
            let mut new = old.clone();
            let own_claim = old.claimed_by.as_deref() == Some(generation);
            let bad = |what: &str| {
                Refusal::BadTransition(format!(
                    "a message that is {} cannot be {what}",
                    old.state.word()
                ))
            };
            let answered = |old: &MessageItem| OutcomeResponse {
                message_id: old.message_id.clone(),
                state: old.state,
            };
            // The attempt a claimed message is settled by, and the attempts a message that is settled already can be asked again by.
            let current = attempt == old.attempt;
            let earlier_or_current = (1..=old.attempt).contains(&attempt);
            match outcome {
                MessageOutcome::Submitted { turn } => match old.state {
                    MessageState::Dispatching if own_claim => {
                        if !current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        enter(
                            &mut new,
                            &mut ctl,
                            MessageState::Delivered,
                            None,
                            Some(*turn),
                            &now,
                        );
                    }
                    MessageState::Delivered => {
                        if !current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        if old.delivered_turn == Some(*turn) {
                            return Ok(answered(&old));
                        }
                        return Err(bad("reported as submitted").into());
                    }
                    _ => return Err(bad("reported as submitted").into()),
                },
                MessageOutcome::Failed { detail } => match old.state {
                    MessageState::Dispatching if own_claim => {
                        if !current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        new.detail = Some(detail.chars().take(MAX_DETAIL_CHARS).collect());
                        enter(&mut new, &mut ctl, MessageState::Failed, None, None, &now);
                    }
                    MessageState::Failed => {
                        if !current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        return Ok(answered(&old));
                    }
                    _ => return Err(bad("reported as failed").into()),
                },
                MessageOutcome::Released => match old.state {
                    MessageState::Dispatching if own_claim => {
                        if !current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        if ctl.admission == Admission::Closed {
                            // The run was stopped while agentd held this message: nobody would claim it again, so it ends here.
                            let why = ctl.closed_reason.unwrap_or(RejectReason::RunCancelled);
                            enter(
                                &mut new,
                                &mut ctl,
                                MessageState::Rejected,
                                Some(why),
                                None,
                                &now,
                            );
                        } else {
                            new.mode = MessageMode::Next;
                            new.claimed_by = None;
                            // The session may have changed since the claim: then the message is held for the user to confirm, as any
                            // message written to an older session is.
                            let to = if ctl.epoch_no == old.epoch_no {
                                MessageState::Pending
                            } else {
                                MessageState::Held
                            };
                            enter(&mut new, &mut ctl, to, None, None, &now);
                            if let Some(o) =
                                ctl.open.iter_mut().find(|o| o.message_id == new.message_id)
                            {
                                o.mode = MessageMode::Next;
                            }
                        }
                    }
                    // the release was applied and its answer was lost: asking again is harmless while the claim it gave back is not
                    // claimed again
                    MessageState::Pending | MessageState::Held => {
                        if !earlier_or_current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        return Ok(answered(&old));
                    }
                    // the release ended the message `rejected` because the run was closed meanwhile, and its answer was lost
                    MessageState::Rejected
                        if own_claim
                            && ctl.admission == Admission::Closed
                            && old.reason.is_some()
                            && old.reason == ctl.closed_reason =>
                    {
                        if !earlier_or_current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        return Ok(answered(&old));
                    }
                    MessageState::Dispatching => return Err(Refusal::StaleAttempt.into()),
                    _ => return Err(bad("released").into()),
                },
                MessageOutcome::Rejected { reason } => match old.state {
                    MessageState::Pending => {
                        // attempt 0 rejects the message as it is; any other attempt must be the claim the message was last given back from
                        if attempt != 0 && attempt != old.attempt {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        enter(
                            &mut new,
                            &mut ctl,
                            MessageState::Rejected,
                            Some(*reason),
                            None,
                            &now,
                        );
                    }
                    MessageState::Dispatching if own_claim => {
                        if !current {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        enter(
                            &mut new,
                            &mut ctl,
                            MessageState::Rejected,
                            Some(*reason),
                            None,
                            &now,
                        );
                    }
                    MessageState::Rejected if old.reason == Some(*reason) => {
                        if attempt > old.attempt {
                            return Err(Refusal::StaleAttempt.into());
                        }
                        return Ok(answered(&old));
                    }
                    _ => return Err(bad("rejected").into()),
                },
            }
            let state = new.state;
            let id = new.message_id.clone();
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs: vec![(old, new)],
                extra: Vec::new(),
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(OutcomeResponse {
                    message_id: id,
                    state,
                });
            }
        }
        Err(Self::contended())
    }

    /// Closes admission from outside agentd: at finalize (agentd is gone) or when the user stops the run. It creates the control item when
    /// there is none, so a message admitted afterwards finds admission closed. Safe to repeat. On a run that is closed already it still
    /// settles what is open: a finalize turns the message a cancel left `dispatching` into `delivery-unknown`.
    pub async fn close(&self, run_id: &str, reason: CloseReason) -> Result<Closed, LedgerError> {
        for round in 0..ATTEMPTS {
            let now = self.now();
            let (ctl_old, existed) = self.load_ctl(run_id).await?;
            let was_closed = existed && ctl_old.admission == Admission::Closed;
            let mut already = Closed {
                watermark: ctl_old.seq,
                ..Closed::default()
            };
            let (why, unknown) = match reason {
                CloseReason::Finalize => (RejectReason::RunSealed, true),
                CloseReason::Cancel => (RejectReason::RunCancelled, false),
            };
            // A finalize has to end the run whatever the list says, so it settles from the message items; a cancel trusts the list and
            // refuses when a listed message is not what its entry says.
            let (base, open) = if reason == CloseReason::Finalize {
                let (fixed, open) = self.repair_open(run_id, &ctl_old).await?;
                already.repaired = !same_entries(&ctl_old.open, &fixed.open);
                (fixed, open)
            } else {
                if was_closed && ctl_old.open.is_empty() {
                    return Ok(already);
                }
                match self.load_open(run_id, &ctl_old).await? {
                    Ok(open) => (ctl_old.clone(), open),
                    Err(d) => {
                        d.retry()?;
                        continue;
                    }
                }
            };
            let (ctl, msgs, mut closed) = apply_close(&base, open, why, unknown, &now);
            closed.repaired = already.repaired;
            if was_closed && msgs.is_empty() && !already.repaired {
                // closed already and nothing to settle (a cancel after a cancel, a finalize after a finalize)
                return Ok(already);
            }
            let change = Change {
                ctl_old,
                existed,
                ctl,
                msgs,
                extra: Vec::new(),
            };
            if let Committed::Done = self.commit(change, round).await? {
                return Ok(closed);
            }
        }
        Err(Self::contended())
    }
}

/// The rule of a close: pending and held messages end `rejected` with `reason`; a dispatching message becomes `delivery-unknown` when
/// `dispatching_unknown` and stays as it is otherwise; admission closes. `open` are the message items the control item lists.
fn apply_close(
    ctl_old: &MessageCtlItem,
    open: Vec<MessageItem>,
    reason: RejectReason,
    dispatching_unknown: bool,
    now: &Now,
) -> (MessageCtlItem, Vec<(MessageItem, MessageItem)>, Closed) {
    let mut ctl = ctl_old.clone();
    let mut closed = Closed::default();
    let mut msgs = Vec::new();
    for old in open {
        let mut new = old.clone();
        match old.state {
            MessageState::Pending | MessageState::Held => {
                enter(
                    &mut new,
                    &mut ctl,
                    MessageState::Rejected,
                    Some(reason),
                    None,
                    now,
                );
                closed.rejected += 1;
            }
            MessageState::Dispatching if dispatching_unknown => {
                enter(
                    &mut new,
                    &mut ctl,
                    MessageState::DeliveryUnknown,
                    None,
                    None,
                    now,
                );
                closed.delivery_unknown += 1;
            }
            _ => continue,
        }
        msgs.push((old, new));
    }
    if ctl_old.admission != Admission::Closed {
        ctl.admission = Admission::Closed;
        ctl.closed_reason = Some(reason);
        ctl.closed_at = Some(now.rfc3339.clone());
    }
    closed.watermark = ctl.seq;
    (ctl, msgs, closed)
}

/// Do two lists name the same messages in the same states and sessions?
fn same_entries(a: &[OpenMessage], b: &[OpenMessage]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.message_id == y.message_id && x.state == y.state && x.epoch == y.epoch)
}

/// What the current incarnation still owes, as a sentence for a refusal: the messages it claimed and has not answered.
fn owed_outcomes(ctl: &MessageCtlItem) -> Option<String> {
    let owed: Vec<&str> = ctl
        .open
        .iter()
        .filter(|o| o.state == MessageState::Dispatching)
        .map(|o| o.message_id.as_str())
        .collect();
    if owed.is_empty() {
        None
    } else {
        Some(format!(
            "agentd still owes the outcome of {} message(s) it claimed ({}); post the outcomes before sealing",
            owed.len(),
            owed.join(", ")
        ))
    }
}

/// The pending messages of the current session in the order a claim takes them: interrupts first, then the rest, each in server order.
fn eligible(ctl: &MessageCtlItem) -> Vec<&OpenMessage> {
    let mut v: Vec<&OpenMessage> = ctl
        .pending()
        .filter(|m| m.epoch.is_some() && m.epoch == ctl.epoch)
        .collect();
    v.sort_by_key(|m| (m.mode != MessageMode::Now, m.seq));
    v
}
