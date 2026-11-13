//! The edges of the ledger that the other test files reach only by accident: the words of every refusal, the plain-table helpers, a message that
//! has been claimed and given back many times (its history stays bounded), a run read before any message exists, the real clock, and the
//! disagreements between the control item and a message that no other test builds (a settled message that is still listed, an entry
//! with another state, an entry that has no message).

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    MAX_HISTORY_STEPS, MessageOutcome, MessageState, RejectReason, SealMode,
};
use harness_protocol::store::keys;
use message_ledger::mem::MemTable;
use message_ledger::{CloseReason, Ledger, LedgerError, Refusal};
use serde_json::{Value, json};
use support::*;

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

fn unavailable(e: &LedgerError) -> bool {
    matches!(e, LedgerError::Unavailable(_))
}

#[test]
fn every_refusal_has_a_status_a_code_and_words() {
    let all = [
        (Refusal::BadInput("text".into()), 400, "bad-input"),
        (Refusal::NotFound, 404, "not-found"),
        (Refusal::RunSealed(None), 409, "run-sealed"),
        (
            Refusal::RunSealed(Some(RejectReason::RunCancelled)),
            409,
            "run-sealed",
        ),
        (
            Refusal::RunSealed(Some(RejectReason::ContinuationLimit)),
            409,
            "run-sealed",
        ),
        (Refusal::TooManyPending, 409, "too-many-pending"),
        (Refusal::RunLimit("limit".into()), 409, "run-limit"),
        (Refusal::RateLimited("run"), 429, "rate-limited"),
        (Refusal::RateLimited("owner"), 429, "rate-limited"),
        (Refusal::IdempotencyConflict, 409, "idempotency-conflict"),
        (Refusal::NotHeld(MessageState::Pending), 409, "not-held"),
        (Refusal::EpochChanged, 409, "epoch-changed"),
        (Refusal::ConfirmMismatch, 409, "confirm-mismatch"),
        (Refusal::AlreadyDispatching, 409, "already-dispatching"),
        (
            Refusal::NotWithdrawable(MessageState::Delivered),
            409,
            "not-withdrawable",
        ),
        (Refusal::StaleGeneration, 409, "stale-generation"),
        (Refusal::StaleEpoch, 409, "stale-epoch"),
        (Refusal::NoSession, 409, "no-session"),
        (Refusal::BadTransition("why".into()), 409, "bad-transition"),
        (Refusal::StaleAttempt, 409, "stale-attempt"),
    ];
    for (r, status, code) in all {
        assert_eq!((r.status(), r.code()), (status, code), "{r:?}");
        assert!(!r.message().is_empty(), "{r:?}");
        let shown = r.to_string();
        assert!(shown.starts_with(code), "{shown}");
    }
}

#[test]
fn the_words_of_a_sealed_run_say_how_it_closed() {
    let cancelled = Refusal::RunSealed(Some(RejectReason::RunCancelled)).message();
    let limit = Refusal::RunSealed(Some(RejectReason::ContinuationLimit)).message();
    let plain = Refusal::RunSealed(None).message();
    assert!(cancelled.contains("stopped"), "{cancelled}");
    assert!(limit.contains("limit"), "{limit}");
    assert!(plain.contains("ending"), "{plain}");
    assert!(Refusal::RateLimited("owner").message().contains("from you"));
}

#[tokio::test]
async fn the_real_clock_admits_and_the_table_can_be_read_back() {
    let table = MemTable::new();
    let ledger = Ledger::new(table.clone());
    let a = ledger.admit(RUN, OWNER, &send(1, "hello")).await.unwrap();
    assert!(a.message.accepted_at.ends_with('Z'));
    assert_eq!(ledger.table().stats().committed, 1);
    assert!(format!("{:?}", ledger.table()).contains("MemTable"));
    assert!(format!("{ledger:?}").contains("Ledger"));
}

#[tokio::test]
async fn a_run_with_no_message_has_nothing_to_hand_out() {
    let r = rig();
    assert!(r.ledger.notices(RUN).await.unwrap().is_empty());
    let page = r.ledger.list(RUN).await.unwrap();
    assert!(page.messages.is_empty());
    assert_eq!(page.watermark, 0);
}

#[tokio::test]
async fn a_message_given_back_again_and_again_keeps_a_bounded_history() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    for attempt in 1..=u32::try_from(MAX_HISTORY_STEPS).unwrap() {
        claimed(r.claim(1, "s1", 1).await);
        r.outcome(1, "s1", 1, attempt, &MessageOutcome::Released)
            .await
            .unwrap();
    }
    let v = r.view(1).await;
    assert!(v.history.len() <= MAX_HISTORY_STEPS, "{}", v.history.len());
    assert_eq!(v.history.first().unwrap().state, MessageState::Pending);
    assert_eq!(v.history.last().unwrap().state, MessageState::Pending);
    assert_eq!(r.state(1).await, MessageState::Pending);
}

// ---- the control item and a message disagree --------------------------------------------------------------------------------

#[tokio::test]
async fn a_withdrawal_refuses_a_message_the_control_item_lists_in_another_state() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    edit_ctl(&r, |c| c["open"][0]["state"] = json!("held"));
    let before = r.table.all();
    let e = r.ledger.withdraw(RUN, &mid(1)).await.unwrap_err();
    assert!(unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before);
}

#[tokio::test]
async fn an_outcome_refuses_a_settled_message_the_control_item_still_lists() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    set_field(&r, 1, "state", json!("delivered"));
    let e = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Submitted { turn: 1 })
        .await
        .unwrap_err();
    assert!(unavailable(&e), "{e:?}");
}

#[tokio::test]
async fn a_close_and_a_claim_refuse_an_entry_that_has_no_message() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    edit_ctl(&r, |c| {
        let mut ghost = c["open"][0].clone();
        ghost["messageId"] = json!(mid(77));
        ghost["seq"] = json!(2);
        c["open"].as_array_mut().unwrap().push(ghost);
    });
    let before = r.table.all();
    // a cancel reads every listed message
    let e = r.ledger.close(RUN, CloseReason::Cancel).await.unwrap_err();
    assert!(unavailable(&e), "{e:?}");
    // a claim of the ghost by id
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", Some(&mid(77)), 1, false)
        .await
        .unwrap_err();
    assert!(unavailable(&e), "{e:?}");
    // an unnamed claim takes the oldest first, so the ghost is never reached before the real one; ask for a batch
    let e = r.claim_err(1, "s1", 3).await;
    assert!(unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before, "nothing was written");
}

#[tokio::test]
async fn a_claim_asked_again_refuses_a_dispatching_entry_that_has_no_message() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    edit_ctl(&r, |c| c["open"][0]["messageId"] = json!(mid(77)));
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert!(unavailable(&e), "{e:?}");
}

#[tokio::test]
async fn a_confirmation_refuses_a_held_original_the_control_item_does_not_list() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "carry on").await;
    r.session(1, "s2").await; // held
    edit_ctl(&r, |c| c["open"] = json!([]));
    let e = r
        .ledger
        .admit(RUN, OWNER, &confirm(2, "carry on", 1, "s2"))
        .await
        .unwrap_err();
    assert!(unavailable(&e), "{e:?}");
}

#[tokio::test]
async fn a_report_of_a_new_session_refuses_a_listed_message_with_another_state() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    set_field(&r, 1, "state", json!("withdrawn"));
    let e = r
        .ledger
        .report_session(RUN, &gen_(1), "s2", 2)
        .await
        .unwrap_err();
    assert!(unavailable(&e), "{e:?}");
    // and the terminal seal reads the same list
    let e = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Terminal)
        .await
        .unwrap_err();
    assert!(unavailable(&e), "{e:?}");
}
