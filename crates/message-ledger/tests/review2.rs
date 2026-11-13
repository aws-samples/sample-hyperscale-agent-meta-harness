//! The fixes of the second design review of 2026-10-16: a seal of a cancelled run
//! that still waits for the outcomes agentd owes, a claim replay that survives a session change, an unseen incarnation that started long
//! before the current one, a release and a rejection that are answered exactly, the checks that a control item and a message agree (and
//! that a read which raced a transition is retried, not reported as corruption), and a finalize that settles a run whose control item lost
//! track of a message.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, ClaimResponse, MessageCtlItem, MessageOutcome, MessageState, RejectReason, SealMode,
    SealResponse,
};
use harness_protocol::store::keys;
use message_ledger::mem::MemTable;
use message_ledger::table::{Item, Table, TableError, WriteOp};
use message_ledger::{CloseReason, Ledger, LedgerError, ManualClock, Refusal};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use support::*;

fn ctl_of(r: &Rig) -> MessageCtlItem {
    let i = r
        .table
        .item(&keys::run_pk(RUN), keys::MESSAGE_CTL)
        .expect("a control item");
    serde_json::from_value(Value::Object(i)).unwrap()
}

/// Rewrites the control item behind the ledger's back, the way an out-of-band writer or a half-applied write would.
fn edit_ctl(r: &Rig, f: impl FnOnce(&mut Value)) {
    let i = r
        .table
        .item(&keys::run_pk(RUN), keys::MESSAGE_CTL)
        .expect("a control item");
    let mut v = Value::Object(i);
    f(&mut v);
    let Value::Object(i) = v else { unreachable!() };
    r.table.put_raw(i);
}

fn set_field(r: &Rig, n: u32, field: &str, value: Value) {
    let mut i = r
        .table
        .item(&keys::run_pk(RUN), &keys::message_sk(&mid(n)))
        .unwrap();
    i.insert(field.to_string(), value);
    r.table.put_raw(i);
}

fn is_unavailable(e: &LedgerError) -> bool {
    matches!(e, LedgerError::Unavailable(_))
}

fn block<F>(f: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    // the hook runs inside a table call that is already on the runtime: a new current-thread runtime on its own thread
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(f)
        })
        .join()
        .unwrap()
    })
}

// ---- the seal of a cancelled run ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_normal_seal_of_a_cancelled_run_waits_for_the_outcome_agentd_still_owes() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await);
    r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    for mode in [SealMode::Normal, SealMode::LimitReached] {
        let e = r.ledger.seal(RUN, &gen_(1), "s1", mode).await.unwrap_err();
        assert!(
            matches!(refusal(e), Refusal::BadTransition(m) if m.contains(&mid(1))),
            "{mode:?}: the refusal names the message agentd still owes"
        );
    }
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    // a caller that is not the current incarnation is not asked to wait for anything
    let res = r
        .ledger
        .seal(RUN, &gen_(2), "s1", SealMode::Normal)
        .await
        .unwrap();
    assert!(matches!(
        res,
        SealResponse::Sealed {
            delivery_unknown: 0,
            ..
        }
    ));
    // the outcome first, then the seal
    r.outcome(1, "s1", 1, 1, &MessageOutcome::Submitted { turn: 2 })
        .await
        .unwrap();
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    assert!(matches!(res, SealResponse::Sealed { .. }));
    assert_eq!(r.state(1).await, MessageState::Delivered);
}

// ---- a claim asked again after the session changed ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_asked_again_after_the_session_changed_still_gets_what_the_incarnation_holds() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await); // the answer is lost
    r.session(1, "s2").await; // the same incarnation started another session
    match r.claim(1, "s2", 1).await {
        ClaimResponse::Claimed {
            messages,
            redelivered: true,
        } => {
            assert_eq!(ids(&messages), [mid(1)]);
            assert_eq!(messages[0].attempt, 1, "the same attempt");
            assert_eq!(messages[0].epoch_no, 1, "the session it was written to");
        }
        other => panic!("agentd was not told it holds the message: {other:?}"),
    }
    // agentd can now settle it and seal
    r.outcome(1, "s2", 1, 1, &MessageOutcome::Submitted { turn: 2 })
        .await
        .unwrap();
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s2", SealMode::Normal)
        .await
        .unwrap();
    assert!(matches!(res, SealResponse::Sealed { .. }));
}

// ---- an unseen incarnation that started long before the current one ---------------------------------------------------------

#[tokio::test]
async fn an_unseen_incarnation_that_started_long_before_the_current_one_does_not_replace_it() {
    let r = rig();
    let now_ms = u64::try_from(T0).unwrap() * 1000;
    let current = gen_at(now_ms, 1);
    r.ledger
        .report_session(RUN, &current, "s1", 1)
        .await
        .unwrap();
    // the delayed first report of an incarnation that was started, and replaced, long ago
    let late_first_report = gen_at(now_ms - 120_000, 2);
    let e = r
        .ledger
        .report_session(RUN, &late_first_report, "s9", 1)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleGeneration);
    let ctl = ctl_of(&r);
    assert_eq!(ctl.generation.as_deref(), Some(current.as_str()));
    assert_eq!(ctl.epoch.as_deref(), Some("s1"));
    assert!(ctl.retired.is_empty(), "nothing was replaced");
    // the current incarnation is undisturbed
    r.admit(1, "a").await;
    assert!(matches!(
        r.ledger
            .claim(RUN, &current, "s1", None, 1, false)
            .await
            .unwrap(),
        ClaimResponse::Claimed { .. }
    ));
}

#[tokio::test]
async fn a_replacement_whose_clock_is_a_little_behind_is_still_the_new_incarnation() {
    let r = rig();
    let now_ms = u64::try_from(T0).unwrap() * 1000;
    let current = gen_at(now_ms, 1);
    r.ledger
        .report_session(RUN, &current, "s1", 1)
        .await
        .unwrap();
    let replacement = gen_at(now_ms - 30_000, 2);
    r.ledger
        .report_session(RUN, &replacement, "s2", 1)
        .await
        .unwrap();
    let ctl = ctl_of(&r);
    assert_eq!(ctl.generation.as_deref(), Some(replacement.as_str()));
    assert_eq!(ctl.retired, [current]);
}

// ---- a release and a rejection are answered exactly -------------------------------------------------------------------------

#[tokio::test]
async fn a_release_that_ended_rejected_in_a_closed_run_is_answered_again_without_a_write() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    let first = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    assert_eq!(first.state, MessageState::Rejected);
    // the answer was lost; agentd asks again
    let writes = r.table.stats().committed;
    let again = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    assert_eq!(again.state, MessageState::Rejected);
    assert_eq!(r.table.stats().committed, writes);
    // an attempt that was never made is still stale
    let e = r
        .outcome(1, "s1", 1, 5, &MessageOutcome::Released)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
}

#[tokio::test]
async fn a_late_rejection_of_an_earlier_attempt_cannot_reject_the_message_as_it_is_now() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    for attempt in 1..=2 {
        claimed(r.claim(1, "s1", 1).await);
        r.outcome(1, "s1", 1, attempt, &MessageOutcome::Released)
            .await
            .unwrap();
    }
    assert_eq!(r.state(1).await, MessageState::Pending);
    let busy = MessageOutcome::Rejected {
        reason: RejectReason::EngineBusy,
    };
    let e = r.outcome(1, "s1", 1, 1, &busy).await.unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
    assert_eq!(r.state(1).await, MessageState::Pending);
    // agentd that never tracked the claims rejects with attempt 0; the current attempt is accepted too
    let o = r.outcome(1, "s1", 1, 0, &busy).await.unwrap();
    assert_eq!(o.state, MessageState::Rejected);
    let o = r.outcome(1, "s1", 1, 2, &busy).await.unwrap();
    assert_eq!(
        o.state,
        MessageState::Rejected,
        "asked again: the same answer"
    );
    let e = r.outcome(1, "s1", 1, 3, &busy).await.unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
}

// ---- the control item and a message agree -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_named_claim_refuses_a_message_listed_for_another_session() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    edit_ctl(&r, |c| c["open"][0]["epoch"] = json!("s0"));
    let before = r.table.all();
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before);
}

#[tokio::test]
async fn a_close_checks_the_session_a_message_is_listed_for() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    edit_ctl(&r, |c| c["open"][0]["epoch"] = json!("s0"));
    let before = r.table.all();
    let e = r.ledger.close(RUN, CloseReason::Cancel).await.unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before);
}

#[tokio::test]
async fn a_claim_asked_again_fails_closed_when_the_message_is_not_what_the_control_item_lists() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    set_field(&r, 1, "state", json!("delivered"));
    let before = r.table.all();
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before);
}

#[tokio::test]
async fn an_outcome_and_a_withdrawal_refuse_a_message_that_is_open_but_not_listed() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "claimed").await;
    r.admit(2, "pending").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
            .await
            .unwrap(),
    );
    edit_ctl(&r, |c| c["open"] = json!([]));
    let before = r.table.all();
    let e = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    let e = r.ledger.withdraw(RUN, &mid(2)).await.unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before, "nothing was written");
}

/// A table that lets a rival request run just before a message item is read: between the read of the control item and the read of the
/// message, which is how an ordinary transition looks like a disagreement.
type Hook = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
struct Racy {
    inner: MemTable,
    hook: Arc<Mutex<Option<Hook>>>,
}

impl Table for Racy {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, TableError> {
        if sk.starts_with(keys::MESSAGE_PREFIX) && sk != keys::MESSAGE_CTL {
            let hook = self.hook.lock().unwrap().take();
            if let Some(h) = hook {
                h();
            }
        }
        self.inner.get(pk, sk).await
    }
    async fn query(&self, pk: &str, sk_prefix: &str) -> Result<Vec<Item>, TableError> {
        self.inner.query(pk, sk_prefix).await
    }
    async fn transact(&self, ops: Vec<WriteOp>) -> Result<(), TableError> {
        self.inner.transact(ops).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_that_raced_a_transition_is_retried_not_reported_as_corruption() {
    let table = MemTable::new();
    let clock = ManualClock::at(T0);
    let rival = Arc::new(Ledger::with_clock(table.clone(), clock.clone()));
    rival.report_session(RUN, &gen_(1), "s1", 1).await.unwrap();
    rival.admit(RUN, OWNER, &send(1, "a")).await.unwrap();
    let hook: Arc<Mutex<Option<Hook>>> = Arc::new(Mutex::new(None));
    let ledger = Ledger::with_clock(
        Racy {
            inner: table.clone(),
            hook: hook.clone(),
        },
        clock,
    );
    // the terminal seal reads the control item (message 1 pending), then the rival claims it, then the seal reads the message item
    let r2 = rival.clone();
    *hook.lock().unwrap() = Some(Box::new(move || {
        block(async move { r2.claim(RUN, &gen_(1), "s1", None, 1, false).await.unwrap() });
    }));
    let res = ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Terminal)
        .await
        .expect("a race is retried, it is not a fault of the store");
    assert_eq!(
        res,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 0,
            delivery_unknown: 1
        }
    );
    let v = rival.get_message(RUN, &mid(1)).await.unwrap().unwrap();
    assert_eq!(v.state, MessageState::DeliveryUnknown);
}

// ---- a finalize settles a run whose control item lost track of a message -----------------------------------------------------

#[tokio::test]
async fn finalize_settles_the_messages_the_control_item_lost_track_of() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "claimed").await;
    r.admit(2, "pending").await;
    r.admit(3, "pending too").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
            .await
            .unwrap(),
    );
    edit_ctl(&r, |c| c["open"] = json!([]));
    let c = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert!(c.repaired, "{c:?}");
    assert_eq!((c.rejected, c.delivery_unknown), (2, 1));
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
    for n in [2, 3] {
        let v = r.view(n).await;
        assert_eq!(
            (v.state, v.reason),
            (MessageState::Rejected, Some(RejectReason::RunSealed))
        );
    }
    let ctl = ctl_of(&r);
    assert!(ctl.open.is_empty());
    assert_eq!(ctl.admission, Admission::Closed);
    // and a second finalize has nothing to do
    let writes = r.table.stats().committed;
    let again = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert!(!again.repaired);
    assert_eq!(r.table.stats().committed, writes);
}

#[tokio::test]
async fn finalize_drops_the_listed_entries_whose_message_is_settled_or_missing() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    set_field(&r, 1, "state", json!("delivered")); // the item is settled, the list still says pending
    edit_ctl(&r, |c| {
        let mut ghost = c["open"][1].clone();
        ghost["messageId"] = json!(mid(77));
        c["open"].as_array_mut().unwrap().push(ghost); // a listed message that has no item
    });
    let c = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert!(c.repaired);
    assert_eq!(c.rejected, 1, "only message 2 was really open");
    assert_eq!(
        r.state(1).await,
        MessageState::Delivered,
        "the item is the truth"
    );
    assert_eq!(r.state(2).await, MessageState::Rejected);
    assert!(ctl_of(&r).open.is_empty());
}

#[tokio::test]
async fn a_cancel_does_not_repair_it_refuses_so_the_fault_is_seen() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    set_field(&r, 1, "state", json!("delivered")); // the list still says pending
    let e = r.ledger.close(RUN, CloseReason::Cancel).await.unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
}
