//! The fixes the design review of 2026-10-16 asked for: the claim attempt, the session
//! number, the list of replaced incarnations, the sweep of a closed run, the seal that waits for outcomes, the outcome that is fenced by the
//! incarnation and not by the session name, a release that cannot outlive a close, and the checks that fail closed when the control item and a
//! message disagree.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, ClaimResponse, MAX_RETIRED_GENERATIONS, MessageCtlItem, MessageItem, MessageOutcome,
    MessageState, RejectReason, SealMode, SealResponse,
};
use harness_protocol::store::keys;
use message_ledger::{CloseReason, LedgerError, Refusal};
use serde_json::json;
use support::*;

fn ctl_of(r: &Rig) -> MessageCtlItem {
    let i = r
        .table
        .item(&keys::run_pk(RUN), keys::MESSAGE_CTL)
        .expect("a control item");
    serde_json::from_value(serde_json::Value::Object(i)).unwrap()
}

fn item_of(r: &Rig, n: u32) -> MessageItem {
    let i = r
        .table
        .item(&keys::run_pk(RUN), &keys::message_sk(&mid(n)))
        .expect("a message item");
    serde_json::from_value(serde_json::Value::Object(i)).unwrap()
}

fn is_unavailable(e: &LedgerError) -> bool {
    matches!(e, LedgerError::Unavailable(_))
}

// ---- A: a closed run still settles what agentd held --------------------------------------------------------------------------

#[tokio::test]
async fn a_finalize_after_a_cancel_settles_the_message_agentd_never_answered() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await);
    let c = r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    assert_eq!((c.rejected, c.delivery_unknown), (0, 0));
    assert_eq!(
        r.state(1).await,
        MessageState::Dispatching,
        "a cancel leaves a claimed message to agentd"
    );
    // agentd is lost: the run finalizes
    let c = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!(c.delivery_unknown, 1, "{c:?}");
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(page.admission, Admission::Closed);
    assert_eq!(
        ctl_of(&r).closed_reason,
        Some(RejectReason::RunCancelled),
        "the first reason a run closed for stays"
    );
    assert!(ctl_of(&r).open.is_empty());
    // and a third close has nothing left to settle and writes nothing
    let writes = r.table.stats().committed;
    r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!(r.table.stats().committed, writes);
}

#[tokio::test]
async fn a_terminal_seal_after_a_cancel_settles_the_message_agentd_never_answered() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await);
    r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Terminal)
        .await
        .unwrap();
    assert_eq!(
        res,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 0,
            delivery_unknown: 1
        }
    );
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
    // a normal seal of a closed run settles nothing by itself, and the current incarnation is asked to post its outcome first (the second
    // review, tests/review2.rs); anybody else gets the closed answer
    let r2 = rig();
    r2.session(1, "s1").await;
    r2.admit(1, "in flight").await;
    claimed(r2.claim(1, "s1", 1).await);
    r2.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    let e = r2
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadTransition(_)));
    r2.ledger
        .seal(RUN, &gen_(2), "s1", SealMode::Normal)
        .await
        .unwrap();
    assert_eq!(
        r2.state(1).await,
        MessageState::Dispatching,
        "a normal seal of a closed run settles nothing; only the end of the run does"
    );
}

#[tokio::test]
async fn a_cancel_after_a_cancel_changes_nothing() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await);
    r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    let writes = r.table.stats().committed;
    r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    assert_eq!(r.table.stats().committed, writes);
    assert_eq!(r.state(1).await, MessageState::Dispatching);
}

// ---- D: the seal waits for the outcomes agentd owes --------------------------------------------------------------------------

#[tokio::test]
async fn a_normal_or_limit_seal_is_refused_while_agentd_still_owes_an_outcome() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await);
    for mode in [SealMode::Normal, SealMode::LimitReached] {
        let e = r.ledger.seal(RUN, &gen_(1), "s1", mode).await.unwrap_err();
        assert!(
            matches!(refusal(e), Refusal::BadTransition(m) if m.contains("outcome")),
            "{mode:?}"
        );
    }
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    assert_eq!(
        r.ledger.list(RUN).await.unwrap().admission,
        Admission::Open,
        "a refused seal closed nothing"
    );
    // the outcome first, then the seal
    r.outcome(1, "s1", 1, 1, &MessageOutcome::Submitted { turn: 2 })
        .await
        .unwrap();
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    assert!(matches!(
        res,
        SealResponse::Sealed {
            delivery_unknown: 0,
            ..
        }
    ));
    assert_eq!(r.state(1).await, MessageState::Delivered);
}

#[tokio::test]
async fn the_terminal_seal_does_not_wait_because_agentd_is_going_away() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "in flight").await;
    claimed(r.claim(1, "s1", 1).await);
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Terminal)
        .await
        .unwrap();
    assert!(matches!(
        res,
        SealResponse::Sealed {
            delivery_unknown: 1,
            ..
        }
    ));
}

// ---- E: which incarnation is the current one is what the server was told ----------------------------------------------------

#[tokio::test]
async fn a_replacement_whose_id_sorts_earlier_is_still_the_new_incarnation() {
    let r = rig();
    r.session(5, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(5, "s1", 1).await);
    // the replacement microVM's clock is behind: its ULID sorts before the old incarnation's
    let ack = r.session(2, "s2").await;
    assert_eq!(ack.delivery_unknown, 1);
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
    // the new incarnation can claim; the old one is fenced out of everything
    r.admit(2, "b").await;
    assert_eq!(ids(&claimed(r.claim(2, "s2", 1).await)), [mid(2)]);
    let e = r
        .ledger
        .claim(RUN, &gen_(5), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleGeneration);
}

#[tokio::test]
async fn a_replaced_incarnation_stays_fenced_even_when_its_id_sorts_later() {
    let r = rig();
    r.session(5, "s1").await;
    r.session(2, "s2").await; // 5 is replaced by 2
    let e = r
        .ledger
        .report_session(RUN, &gen_(5), "s1", 2)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleGeneration);
    assert_eq!(ctl_of(&r).generation, Some(gen_(2)));
    assert_eq!(ctl_of(&r).retired, [gen_(5)]);
}

#[tokio::test]
async fn the_list_of_replaced_incarnations_is_bounded_and_keeps_the_newest() {
    let r = rig();
    let total = u32::try_from(MAX_RETIRED_GENERATIONS).unwrap() + 8;
    for g in 1..=total {
        r.session(g, "s1").await;
    }
    let ctl = ctl_of(&r);
    assert_eq!(ctl.generation, Some(gen_(total)));
    assert_eq!(ctl.retired.len(), MAX_RETIRED_GENERATIONS);
    assert_eq!(
        ctl.retired.last(),
        Some(&gen_(total - 1)),
        "the newest replaced incarnation is the last in the list"
    );
    assert!(!ctl.retired.contains(&gen_(total)));
}

// ---- J: sessions are ordered by their number ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_late_report_of_an_older_session_changes_nothing() {
    let r = rig();
    r.session_n(1, "s1", 1).await;
    r.admit(1, "a").await;
    r.session_n(1, "s2", 2).await; // 1 is held
    r.admit(2, "b").await;
    let writes = r.table.stats().committed;
    // the retry of the first report arrives after the second one
    let ack = r.session_n(1, "s1", 1).await;
    assert_eq!((ack.epoch_no, ack.held, ack.delivery_unknown), (2, 0, 0));
    assert_eq!(r.table.stats().committed, writes, "it wrote nothing");
    let ctl = ctl_of(&r);
    assert_eq!(ctl.epoch.as_deref(), Some("s2"));
    assert_eq!(ctl.session_number, 2);
    assert_eq!(r.state(1).await, MessageState::Held);
    assert_eq!(r.state(2).await, MessageState::Pending);
}

#[tokio::test]
async fn the_same_session_number_with_another_session_id_is_refused() {
    let r = rig();
    r.session_n(1, "s1", 1).await;
    let e = r
        .ledger
        .report_session(RUN, &gen_(1), "other", 1)
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadTransition(_)));
    assert_eq!(ctl_of(&r).epoch.as_deref(), Some("s1"));
}

#[tokio::test]
async fn a_higher_number_for_the_session_that_is_already_current_moves_only_the_number() {
    // the engine resumed its own session: the conversation is the same, so nothing is held
    let r = rig();
    r.session_n(1, "s1", 1).await;
    r.admit(1, "a").await;
    let ack = r.session_n(1, "s1", 2).await;
    assert_eq!((ack.epoch_no, ack.held, ack.delivery_unknown), (1, 0, 0));
    assert_eq!(ctl_of(&r).session_number, 2);
    assert_eq!(r.state(1).await, MessageState::Pending);
    // and the older number is now late
    let writes = r.table.stats().committed;
    r.session_n(1, "s1", 1).await;
    assert_eq!(r.table.stats().committed, writes);
}

#[tokio::test]
async fn a_new_incarnation_numbers_its_sessions_from_its_own_start() {
    let r = rig();
    r.session_n(1, "s1", 1).await;
    r.session_n(1, "s2", 2).await;
    r.session_n(1, "s3", 3).await;
    r.session_n(2, "s4", 1).await;
    let ctl = ctl_of(&r);
    assert_eq!(
        (ctl.generation, ctl.session_number, ctl.epoch.as_deref()),
        (Some(gen_(2)), 1, Some("s4"))
    );
}

// ---- I: the attempt token ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_claim_of_a_message_has_its_own_attempt_number() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let first = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(first[0].attempt, 1);
    // a claim asked again before the outcome is a redelivery of the same attempt
    match r.claim(1, "s1", 1).await {
        ClaimResponse::Claimed {
            messages,
            redelivered: true,
        } => assert_eq!(messages[0].attempt, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(item_of(&r, 1).attempt, 1);
    r.outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    let second = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(second[0].attempt, 2);
    assert_eq!(item_of(&r, 1).attempt, 2);
}

#[tokio::test]
async fn the_late_retry_of_an_earlier_attempt_cannot_settle_the_current_one() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    r.outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    claimed(r.claim(1, "s1", 1).await); // attempt 2 is dispatching
    // the retry of the release of attempt 1 arrives now: it must not give attempt 2 back
    for o in [
        MessageOutcome::Released,
        MessageOutcome::Submitted { turn: 9 },
        MessageOutcome::Failed { detail: "x".into() },
    ] {
        let e = r.outcome(1, "s1", 1, 1, &o).await.unwrap_err();
        assert_eq!(refusal(e), Refusal::StaleAttempt, "{o:?}");
    }
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    // an outcome without an attempt does not settle a claim either
    let e = r
        .outcome(1, "s1", 1, 0, &MessageOutcome::Submitted { turn: 9 })
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
    // the right attempt does
    r.outcome(1, "s1", 1, 2, &MessageOutcome::Submitted { turn: 3 })
        .await
        .unwrap();
    assert_eq!(r.state(1).await, MessageState::Delivered);
    // and its own retry is harmless, while an earlier attempt's is still stale
    r.outcome(1, "s1", 1, 2, &MessageOutcome::Submitted { turn: 3 })
        .await
        .unwrap();
    let e = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Submitted { turn: 3 })
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
}

#[tokio::test]
async fn a_release_that_is_asked_again_is_harmless_while_the_message_is_not_claimed_again() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    r.outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    let writes = r.table.stats().committed;
    let again = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    assert_eq!(again.state, MessageState::Pending);
    assert_eq!(r.table.stats().committed, writes);
    // an attempt that was never made is not a retry of anything
    let e = r
        .outcome(1, "s1", 1, 5, &MessageOutcome::Released)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
}

#[tokio::test]
async fn a_release_retried_after_the_session_changed_answers_held() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    r.session(1, "s2").await;
    let first = r
        .outcome(1, "s2", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    assert_eq!(first.state, MessageState::Held);
    let again = r
        .outcome(1, "s2", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    assert_eq!(again.state, MessageState::Held);
}

#[tokio::test]
async fn agentd_rejects_a_pending_message_it_never_claimed_with_attempt_zero() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let o = r
        .outcome(
            1,
            "s1",
            1,
            0,
            &MessageOutcome::Rejected {
                reason: RejectReason::EngineBusy,
            },
        )
        .await
        .unwrap();
    assert_eq!(o.state, MessageState::Rejected);
    // the same rejection again is the same answer
    let o = r
        .outcome(
            1,
            "s1",
            1,
            0,
            &MessageOutcome::Rejected {
                reason: RejectReason::EngineBusy,
            },
        )
        .await
        .unwrap();
    assert_eq!(o.state, MessageState::Rejected);
}

// ---- H: a release cannot outlive a close -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_given_back_after_a_cancel_is_rejected_not_left_pending_for_nobody() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    let o = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap();
    assert_eq!(o.state, MessageState::Rejected);
    let v = r.view(1).await;
    assert_eq!(v.reason, Some(RejectReason::RunCancelled));
    assert!(
        ctl_of(&r).open.is_empty(),
        "nothing is left open in a closed run"
    );
    assert!(r.ledger.notices(RUN).await.unwrap().is_empty());
}

// ---- G: the checks fail closed when the control item and a message disagree --------------------------------------------------

fn set_field(r: &Rig, n: u32, field: &str, value: serde_json::Value) {
    let mut i = r
        .table
        .item(&keys::run_pk(RUN), &keys::message_sk(&mid(n)))
        .unwrap();
    i.insert(field.to_string(), value);
    r.table.put_raw(i);
}

#[tokio::test]
async fn a_claim_refuses_to_act_on_a_message_the_control_item_lists_wrongly() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    // the message item says withdrawn while the control item still lists it pending: a half-applied write the ledger never makes
    set_field(&r, 1, "state", json!("withdrawn"));
    let before = r.table.all();
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before, "a diverged claim wrote nothing");
    // by id as well
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
}

#[tokio::test]
async fn a_cancel_and_a_terminal_seal_refuse_to_act_when_a_listed_message_is_in_another_state() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    set_field(&r, 1, "state", json!("delivered"));
    let before = r.table.all();
    // a cancel trusts the list and refuses; a finalize settles from the message items instead (tests/review2.rs)
    let e = r.ledger.close(RUN, CloseReason::Cancel).await.unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    let e = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Terminal)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(r.table.all(), before);
}

#[tokio::test]
async fn a_message_listed_for_another_session_than_its_item_is_not_claimed() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    set_field(&r, 1, "epoch", json!("s0"));
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert!(is_unavailable(&e), "{e:?}");
    assert_eq!(
        item_of(&r, 1).state,
        MessageState::Pending,
        "the item was not touched"
    );
}
