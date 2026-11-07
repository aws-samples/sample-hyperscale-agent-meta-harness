//! A model check of the ledger: random sequences of every operation (admit, withdraw, session reports of two incarnations, claims, the
//! three seals, outcomes, the two closes) run against the in-memory table, and after each step the invariants must hold on what
//! is stored. A refusal is a normal answer; only a broken invariant, or an error that is not a refusal, fails.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, MAX_HISTORY_STEPS, MAX_OUTSTANDING_MESSAGES, MAX_RETIRED_GENERATIONS,
    MessageCtlItem, MessageItem, MessageOutcome, MessageState, RejectReason, SealMode,
};
use harness_protocol::store::keys;
use message_ledger::mem::MemTable;
use message_ledger::table::Table;
use message_ledger::{CloseReason, LedgerError};
use proptest::prelude::*;
use std::collections::BTreeMap;
use support::*;

#[derive(Clone, Debug)]
enum Op {
    Admit {
        now: bool,
    },
    Confirm(usize),
    Withdraw(usize),
    Session {
        incarnation: u32,
        epoch: u8,
    },
    Claim {
        limit: u8,
        by_id: Option<usize>,
    },
    Seal(u8),
    Submit(usize),
    Fail(usize),
    Release(usize),
    Reject(usize, bool),
    /// An outcome that names an attempt nobody made: it must always be refused.
    StaleSubmit(usize),
    Close(bool),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => any::<bool>().prop_map(|now| Op::Admit { now }),
        1 => (0usize..8).prop_map(Op::Confirm),
        2 => (0usize..8).prop_map(Op::Withdraw),
        3 => (1u32..5, 1u8..4).prop_map(|(incarnation, epoch)| Op::Session { incarnation, epoch }),
        4 => (0u8..5, proptest::option::of(0usize..8)).prop_map(|(limit, by_id)| Op::Claim { limit, by_id }),
        2 => (0u8..3).prop_map(Op::Seal),
        3 => (0usize..8).prop_map(Op::Submit),
        1 => (0usize..8).prop_map(Op::Fail),
        1 => (0usize..8).prop_map(Op::Release),
        1 => (0usize..8, any::<bool>()).prop_map(|(i, busy)| Op::Reject(i, busy)),
        1 => (0usize..8).prop_map(Op::StaleSubmit),
        1 => any::<bool>().prop_map(Op::Close),
    ]
}

async fn stored(t: &MemTable) -> (Option<MessageCtlItem>, BTreeMap<u32, MessageItem>) {
    let ctl = t
        .get(&keys::run_pk(RUN), keys::MESSAGE_CTL)
        .await
        .unwrap()
        .map(|i| serde_json::from_value(serde_json::Value::Object(i)).unwrap());
    let msgs = t
        .query(&keys::run_pk(RUN), keys::MESSAGE_PREFIX)
        .await
        .unwrap()
        .into_iter()
        .map(|i| serde_json::from_value::<MessageItem>(serde_json::Value::Object(i)).unwrap())
        .map(|m| (m.seq, m))
        .collect();
    (ctl, msgs)
}

fn check_invariants(
    ctl: &Option<MessageCtlItem>,
    msgs: &BTreeMap<u32, MessageItem>,
    before: &BTreeMap<u32, MessageItem>,
    closed_before: bool,
) -> Result<(), String> {
    let Some(ctl) = ctl else {
        return if msgs.is_empty() {
            Ok(())
        } else {
            Err("messages without a control item".into())
        };
    };
    // the sequence numbers are 1..=n with no gap and match the control item
    let seqs: Vec<u32> = msgs.keys().copied().collect();
    let want: Vec<u32> = (1..=u32::try_from(msgs.len()).unwrap()).collect();
    if seqs != want || ctl.seq as usize != msgs.len() {
        return Err(format!(
            "sequence numbers {seqs:?}, control seq {}",
            ctl.seq
        ));
    }
    // the control item lists exactly the open messages, in the same state
    let open: Vec<(String, MessageState)> = msgs
        .values()
        .filter(|m| m.state.is_open())
        .map(|m| (m.message_id.clone(), m.state))
        .collect();
    let listed: Vec<(String, MessageState)> = ctl
        .open
        .iter()
        .map(|o| (o.message_id.clone(), o.state))
        .collect();
    if open != listed {
        return Err(format!(
            "open {open:?} but the control item lists {listed:?}"
        ));
    }
    if ctl.outstanding() > MAX_OUTSTANDING_MESSAGES {
        return Err(format!("{} outstanding", ctl.outstanding()));
    }
    let bytes: u64 = msgs.values().map(|m| m.text.len() as u64).sum();
    if bytes != ctl.bytes_total {
        return Err(format!("bytes {bytes} vs {}", ctl.bytes_total));
    }
    for m in msgs.values() {
        // the last step of the history is the state, and the history starts at pending
        let last = m.history.last().map(|s| s.state);
        if last != Some(m.state)
            || m.history.first().map(|s| s.state) != Some(MessageState::Pending)
        {
            return Err(format!(
                "message {} history {:?} state {:?}",
                m.seq, m.history, m.state
            ));
        }
        let count = |s: MessageState| m.history.iter().filter(|h| h.state == s).count();
        if count(MessageState::Delivered) > 1 {
            return Err(format!("message {} delivered twice", m.seq));
        }
        if m.state == MessageState::Delivered
            && m.history
                .iter()
                .all(|h| h.state != MessageState::Dispatching)
        {
            return Err(format!("message {} delivered without a claim", m.seq));
        }
        // a claim that was given back (released) returns the message to pending, and then it can be withdrawn
        let last_claim = m
            .history
            .iter()
            .rposition(|h| h.state == MessageState::Dispatching);
        if m.state == MessageState::Withdrawn
            && last_claim.is_some_and(|i| {
                m.history[i..]
                    .iter()
                    .all(|h| h.state != MessageState::Pending && h.state != MessageState::Held)
            })
        {
            return Err(format!(
                "message {} withdrawn after a claim that was not given back",
                m.seq
            ));
        }
        if m.state == MessageState::Delivered && m.delivered_turn.is_none() {
            return Err(format!("message {} delivered without a turn", m.seq));
        }
        if m.state == MessageState::Rejected && m.reason.is_none() {
            return Err(format!("message {} rejected without a reason", m.seq));
        }
        // a settled message stays as it was, except a delivery-unknown or held original that a confirmation supersedes
        if let Some(b) = before.get(&m.seq)
            && b.state.is_settled()
            && b.state != m.state
            && !(b.state == MessageState::DeliveryUnknown && m.state == MessageState::Superseded)
        {
            return Err(format!(
                "message {} left {:?} for {:?}",
                m.seq, b.state, m.state
            ));
        }
        if m.state == MessageState::Superseded && m.superseded_by.is_none() {
            return Err(format!("message {} superseded by nobody", m.seq));
        }
    }
    // the attempt of a message is the number of times it was claimed
    for m in msgs.values() {
        let claims = m
            .history
            .iter()
            .filter(|h| h.state == MessageState::Dispatching)
            .count();
        if m.history.len() < MAX_HISTORY_STEPS && usize::try_from(m.attempt).unwrap() != claims {
            return Err(format!(
                "message {} attempt {} after {claims} claims",
                m.seq, m.attempt
            ));
        }
        if let Some(b) = before.get(&m.seq)
            && m.attempt < b.attempt
        {
            return Err(format!("message {} attempt went back", m.seq));
        }
    }
    // a closed run holds nothing a person could still be waiting for: only a claim agentd owes an answer for can stay open
    if ctl.admission == Admission::Closed
        && ctl
            .open
            .iter()
            .any(|o| matches!(o.state, MessageState::Pending | MessageState::Held))
    {
        return Err("a closed run still lists a pending or held message".into());
    }
    // the list of replaced incarnations never holds the current one, repeats one, or grows past its bound
    let mut seen = std::collections::BTreeSet::new();
    if ctl.retired.len() > MAX_RETIRED_GENERATIONS
        || ctl
            .retired
            .iter()
            .any(|g| Some(g) == ctl.generation.as_ref())
        || !ctl.retired.iter().all(|g| seen.insert(g.clone()))
    {
        return Err(format!(
            "retired {:?} generation {:?}",
            ctl.retired, ctl.generation
        ));
    }
    // closed admission never reopens and admits nothing
    if closed_before && (ctl.admission != Admission::Closed) {
        return Err("admission reopened".into());
    }
    if closed_before && msgs.len() != before.len() {
        return Err("a message was admitted after the close".into());
    }
    // held messages were written to an earlier session than the current one (by its number: an engine may resume an earlier session id);
    // pending ones are in the current one
    for m in msgs.values() {
        match m.state {
            MessageState::Held if m.epoch_no >= ctl.epoch_no => {
                return Err(format!(
                    "message {} held in session {} of {}",
                    m.seq, m.epoch_no, ctl.epoch_no
                ));
            }
            MessageState::Pending if m.epoch_no != ctl.epoch_no || m.epoch != ctl.epoch => {
                return Err(format!(
                    "message {} pending in session {} of {}",
                    m.seq, m.epoch_no, ctl.epoch_no
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

async fn run(ops: Vec<Op>) -> Result<(), String> {
    let r = rig();
    let mut next_id = 1u32;
    let mut known: Vec<u32> = Vec::new();
    let mut cur_gen = 0u32;
    let mut cur_epoch = String::new();
    // the number agentd gives the next session of each incarnation, and the session each one reported last
    let mut numbers: BTreeMap<u32, (String, u32)> = BTreeMap::new();
    for (step, op) in ops.iter().enumerate() {
        r.clock.advance(7);
        let (_, before) = stored(&r.table).await;
        let closed_before = r.ledger.list(RUN).await.unwrap().admission == Admission::Closed;
        let pick = |i: usize| known.get(i % known.len().max(1)).copied();
        let result: Result<(), LedgerError> = match op {
            Op::Admit { now } => {
                let n = next_id;
                next_id += 1;
                let req = if *now {
                    send_now(n, "interrupt")
                } else {
                    send(n, "message")
                };
                let res = r.ledger.admit(RUN, OWNER, &req).await.map(|_| ());
                if res.is_ok() {
                    known.push(n);
                }
                res
            }
            Op::Confirm(i) => match pick(*i) {
                Some(h) => {
                    let n = next_id;
                    next_id += 1;
                    let text = before
                        .values()
                        .find(|m| m.message_id == mid(h))
                        .map(|m| m.text.clone())
                        .unwrap_or_default();
                    let mode = before
                        .values()
                        .find(|m| m.message_id == mid(h))
                        .map(|m| m.mode)
                        .unwrap_or_default();
                    let mut req = confirm(n, &text, h, &cur_epoch);
                    req.mode = mode;
                    let res = r.ledger.admit(RUN, OWNER, &req).await.map(|_| ());
                    if res.is_ok() {
                        known.push(n);
                    }
                    res
                }
                None => Ok(()),
            },
            Op::Withdraw(i) => match pick(*i) {
                Some(n) => r.ledger.withdraw(RUN, &mid(n)).await.map(|_| ()),
                None => Ok(()),
            },
            Op::Session { incarnation, epoch } => {
                let e = format!("s{epoch}");
                let slot = numbers
                    .entry(*incarnation)
                    .or_insert_with(|| (String::new(), 0));
                let number = if slot.0 == e { slot.1 } else { slot.1 + 1 };
                let res = r
                    .ledger
                    .report_session(RUN, &gen_(*incarnation), &e, number)
                    .await
                    .map(|_| ());
                if res.is_ok() {
                    *slot = (e.clone(), number);
                }
                if res.is_ok() {
                    cur_gen = *incarnation;
                    cur_epoch = e;
                }
                res
            }
            Op::Claim { limit, by_id } => {
                let id = by_id.and_then(&pick).map(mid);
                r.ledger
                    .claim(
                        RUN,
                        &gen_(cur_gen.max(1)),
                        &cur_epoch,
                        id.as_deref(),
                        *limit,
                        false,
                    )
                    .await
                    .map(|_| ())
            }
            Op::Seal(m) => {
                let mode = [SealMode::Normal, SealMode::LimitReached, SealMode::Terminal]
                    [usize::from(*m) % 3];
                r.ledger
                    .seal(RUN, &gen_(cur_gen.max(1)), &cur_epoch, mode)
                    .await
                    .map(|_| ())
            }
            Op::Submit(i) | Op::Fail(i) | Op::Release(i) | Op::Reject(i, _) => match pick(*i) {
                Some(n) => {
                    let o = match op {
                        Op::Submit(_) => MessageOutcome::Submitted { turn: 1 },
                        Op::Fail(_) => MessageOutcome::Failed { detail: "x".into() },
                        Op::Release(_) => MessageOutcome::Released,
                        Op::Reject(_, busy) => MessageOutcome::Rejected {
                            reason: if *busy {
                                RejectReason::EngineBusy
                            } else {
                                RejectReason::ContinuationLimit
                            },
                        },
                        _ => unreachable!(),
                    };
                    let stored = before.values().find(|m| m.message_id == mid(n));
                    let epoch = stored
                        .and_then(|m| m.epoch.clone())
                        .unwrap_or_else(|| cur_epoch.clone());
                    let attempt = stored.map_or(0, |m| m.attempt);
                    r.ledger
                        .outcome(RUN, &gen_(cur_gen.max(1)), &epoch, &mid(n), attempt, &o)
                        .await
                        .map(|_| ())
                }
                None => Ok(()),
            },
            Op::StaleSubmit(i) => match pick(*i) {
                Some(n) => {
                    let attempt = before
                        .values()
                        .find(|m| m.message_id == mid(n))
                        .map_or(0, |m| m.attempt)
                        + 1;
                    let res = r
                        .ledger
                        .outcome(
                            RUN,
                            &gen_(cur_gen.max(1)),
                            &cur_epoch,
                            &mid(n),
                            attempt,
                            &MessageOutcome::Submitted { turn: 1 },
                        )
                        .await;
                    match res {
                        Err(LedgerError::Refused(_)) => Ok(()),
                        other => {
                            return Err(format!(
                                "step {step} {op:?}: an attempt nobody made was accepted: {other:?}"
                            ));
                        }
                    }
                }
                None => Ok(()),
            },
            Op::Close(cancel) => r
                .ledger
                .close(
                    RUN,
                    if *cancel {
                        CloseReason::Cancel
                    } else {
                        CloseReason::Finalize
                    },
                )
                .await
                .map(|_| ()),
        };
        match result {
            Ok(()) | Err(LedgerError::Refused(_)) => {}
            Err(e) => return Err(format!("step {step} {op:?}: {e}")),
        }
        let (ctl, now) = stored(&r.table).await;
        check_invariants(&ctl, &now, &before, closed_before)
            .map_err(|e| format!("after step {step} {op:?}: {e}"))?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn every_sequence_of_operations_keeps_the_invariants(ops in proptest::collection::vec(op(), 1..40)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let res = rt.block_on(run(ops));
        prop_assert!(res.is_ok(), "{res:?}");
    }
}
