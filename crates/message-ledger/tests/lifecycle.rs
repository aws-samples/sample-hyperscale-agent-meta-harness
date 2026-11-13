//! The states of a message: withdrawal, the session report, the claim, the outcome, the seal and the close.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, ClaimRefusal, ClaimResponse, MAX_CLAIM_BATCH, MAX_DETAIL_CHARS, MessageMode,
    MessageOutcome, MessageState, RejectReason, SealMode, SealResponse,
};
use message_ledger::{CloseReason, Refusal};
use support::*;

// ---- withdraw ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pending_or_held_message_can_be_withdrawn_and_a_repeat_is_harmless() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    r.session(1, "s2").await; // both held
    r.admit(3, "c").await;
    for n in [1, 3] {
        let v = r.ledger.withdraw(RUN, &mid(n)).await.unwrap();
        assert_eq!(v.state, MessageState::Withdrawn);
        let writes = r.table.stats().committed;
        assert_eq!(
            r.ledger.withdraw(RUN, &mid(n)).await.unwrap().state,
            MessageState::Withdrawn
        );
        assert_eq!(
            r.table.stats().committed,
            writes,
            "a second withdrawal writes nothing"
        );
    }
    assert_eq!(r.state(2).await, MessageState::Held);
    assert_eq!(
        r.ledger.list(RUN).await.unwrap().messages.len(),
        3,
        "a withdrawn message stays on the list"
    );
    // a withdrawn message is never offered to agentd
    assert!(r.ledger.notices(RUN).await.unwrap().is_empty());
    assert!(matches!(
        r.claim(1, "s2", 3).await,
        ClaimResponse::Nothing { .. }
    ));
}

#[tokio::test]
async fn a_claimed_or_settled_message_cannot_be_withdrawn() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    claimed(r.claim(1, "s1", 1).await);
    let e = r.ledger.withdraw(RUN, &mid(1)).await.unwrap_err();
    assert_eq!(refusal(e), Refusal::AlreadyDispatching);
    r.ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 2 },
        )
        .await
        .unwrap();
    let e = r.ledger.withdraw(RUN, &mid(1)).await.unwrap_err();
    assert_eq!(
        refusal(e),
        Refusal::NotWithdrawable(MessageState::Delivered)
    );
    let e = r.ledger.withdraw(RUN, &mid(99)).await.unwrap_err();
    assert_eq!(refusal(e), Refusal::NotFound);
    assert_eq!(
        r.state(1).await,
        MessageState::Delivered,
        "a refused withdrawal changed nothing"
    );
}

// ---- the session report -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_new_session_holds_what_was_pending_in_the_old_one() {
    let r = rig();
    assert_eq!(r.session(1, "s1").await.epoch_no, 1);
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    let ack = r.session(1, "s2").await;
    assert_eq!((ack.epoch_no, ack.held, ack.delivery_unknown), (2, 2, 0));
    let v = r.view(1).await;
    assert_eq!(v.state, MessageState::Held);
    assert_eq!(
        (v.epoch.as_deref(), v.epoch_no),
        (Some("s1"), 1),
        "it keeps the session it was written to"
    );
    // a message written now belongs to the new session, and only it is offered
    r.admit(3, "c").await;
    let notices = r.ledger.notices(RUN).await.unwrap();
    assert_eq!(
        notices
            .iter()
            .map(|n| n.message_id.clone())
            .collect::<Vec<_>>(),
        [mid(3)]
    );
    assert_eq!(notices[0].epoch.as_deref(), Some("s2"));
}

#[tokio::test]
async fn the_same_report_again_changes_nothing() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let writes = r.table.stats().committed;
    let ack = r.session(1, "s1").await;
    assert_eq!((ack.epoch_no, ack.held, ack.delivery_unknown), (1, 0, 0));
    assert_eq!(r.table.stats().committed, writes);
    assert_eq!(r.state(1).await, MessageState::Pending);
}

#[tokio::test]
async fn a_newer_incarnation_turns_what_the_old_one_was_dispatching_into_delivery_unknown() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    claimed(r.claim(1, "s1", 1).await);
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    // the replacement microVM starts a new incarnation and a new session
    let ack = r.session(2, "s9").await;
    assert_eq!((ack.held, ack.delivery_unknown), (1, 1));
    let v = r.view(1).await;
    assert_eq!(v.state, MessageState::DeliveryUnknown);
    assert_eq!(r.state(2).await, MessageState::Held);
    // the old incarnation is fenced out of every later call
    for res in [
        r.ledger
            .claim(RUN, &gen_(1), "s1", None, 1, false)
            .await
            .map(|_| ()),
        r.ledger
            .seal(RUN, &gen_(1), "s1", SealMode::Normal)
            .await
            .map(|_| ()),
        r.ledger
            .outcome(
                RUN,
                &gen_(1),
                "s1",
                &mid(1),
                1,
                &MessageOutcome::Submitted { turn: 1 },
            )
            .await
            .map(|_| ()),
        r.ledger
            .report_session(RUN, &gen_(1), "s1", 1)
            .await
            .map(|_| ()),
    ] {
        assert_eq!(refusal(res.unwrap_err()), Refusal::StaleGeneration);
    }
    assert_eq!(
        r.state(1).await,
        MessageState::DeliveryUnknown,
        "a late outcome of the old incarnation changed nothing"
    );
}

#[tokio::test]
async fn a_new_incarnation_that_resumes_the_same_session_keeps_pending_messages_pending() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let ack = r.session(2, "s1").await;
    assert_eq!((ack.epoch_no, ack.held, ack.delivery_unknown), (1, 0, 0));
    assert_eq!(r.state(1).await, MessageState::Pending);
    assert!(matches!(
        r.ledger
            .claim(RUN, &gen_(2), "s1", None, 1, false)
            .await
            .unwrap(),
        ClaimResponse::Claimed { .. }
    ));
}

#[tokio::test]
async fn a_report_needs_a_ulid_generation_and_a_session_id() {
    let r = rig();
    let e = r
        .ledger
        .report_session(RUN, "not-a-ulid", "s1", 1)
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadInput(_)));
    let e = r
        .ledger
        .report_session(RUN, &gen_(1), " ", 1)
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadInput(_)));
    let e = r
        .ledger
        .report_session(RUN, &gen_(1), &"e".repeat(257), 1)
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadInput(_)));
    // sessions are numbered from 1
    let e = r
        .ledger
        .report_session(RUN, &gen_(1), "s1", 0)
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadInput(_)));
}

// ---- the claim --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_takes_interrupts_first_then_the_oldest_and_at_most_a_batch() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "first").await;
    r.ledger
        .admit(RUN, OWNER, &send_now(2, "interrupt"))
        .await
        .unwrap();
    r.admit(3, "third").await;
    let got = claimed(r.claim(1, "s1", 2).await);
    assert_eq!(
        ids(&got),
        [mid(2), mid(1)],
        "the interrupt first, then the oldest"
    );
    assert_eq!(got[0].text, "interrupt");
    assert_eq!(got[0].mode, MessageMode::Now);
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    assert_eq!(r.state(3).await, MessageState::Pending);
    // the batch is clamped to 1..=3 and a claim never takes what is not pending
    r.session(1, "s1").await;
    r.outcome_all_submitted(&[1, 2]).await;
    let rest = claimed(r.claim(1, "s1", 0).await);
    assert_eq!(ids(&rest), [mid(3)]);
    assert_eq!(MAX_CLAIM_BATCH, 3);
}

impl Rig {
    async fn outcome_all_submitted(&self, ns: &[u32]) {
        for (t, n) in ns.iter().enumerate() {
            self.ledger
                .outcome(
                    RUN,
                    &gen_(1),
                    "s1",
                    &mid(*n),
                    1,
                    &MessageOutcome::Submitted { turn: t as u32 + 1 },
                )
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn a_claim_by_id_takes_that_message_only() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    let got = claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(2)), 3, false)
            .await
            .unwrap(),
    );
    assert_eq!(ids(&got), [mid(2)]);
    assert_eq!(r.state(1).await, MessageState::Pending);
    // a withdrawn, unknown or settled message is reported, not claimed
    r.ledger.withdraw(RUN, &mid(1)).await.unwrap();
    assert_eq!(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
            .await
            .unwrap(),
        ClaimResponse::NotPending {
            state: MessageState::Withdrawn
        }
    );
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", Some(&mid(77)), 1, false)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NotFound);
}

#[tokio::test]
async fn a_claim_that_finds_nothing_says_so_and_says_whether_admission_is_open() {
    let r = rig();
    r.session(1, "s1").await;
    assert_eq!(
        r.claim(1, "s1", 1).await,
        ClaimResponse::Nothing {
            admission: Admission::Open
        }
    );
    r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!(
        r.claim(1, "s1", 1).await,
        ClaimResponse::Refused {
            reason: ClaimRefusal::AdmissionClosed
        }
    );
}

#[tokio::test]
async fn a_claim_is_fenced_by_the_incarnation_and_the_session() {
    let r = rig();
    r.admit(1, "a").await;
    // no session reported yet
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NoSession);
    r.session(2, "s1").await;
    let e = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleGeneration);
    let e = r
        .ledger
        .claim(RUN, &gen_(3), "s1", None, 1, false)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(e),
        Refusal::StaleGeneration,
        "an incarnation the server has not been told about holds nothing"
    );
    let e = r
        .ledger
        .claim(RUN, &gen_(2), "other-session", None, 1, false)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleEpoch);
    assert_eq!(
        r.state(1).await,
        MessageState::Pending,
        "no refused claim claimed anything"
    );
}

#[tokio::test]
async fn a_queued_cancel_wins_and_the_open_messages_end_rejected() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
            .await
            .unwrap(),
    );
    let res = r
        .ledger
        .claim(RUN, &gen_(1), "s1", None, 1, true)
        .await
        .unwrap();
    assert_eq!(
        res,
        ClaimResponse::Refused {
            reason: ClaimRefusal::CancelQueued
        }
    );
    let v = r.view(2).await;
    assert_eq!(
        (v.state, v.reason),
        (MessageState::Rejected, Some(RejectReason::RunCancelled))
    );
    assert_eq!(
        r.state(1).await,
        MessageState::Dispatching,
        "a message agentd already holds is its own to finish"
    );
    assert_eq!(
        r.ledger.list(RUN).await.unwrap().admission,
        Admission::Closed
    );
    assert!(matches!(
        r.refused(&send(3, "late")).await,
        Refusal::RunSealed(Some(RejectReason::RunCancelled))
    ));
}

#[tokio::test]
async fn a_claim_asked_again_gives_back_what_this_incarnation_already_holds() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    let first = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(ids(&first), [mid(1)]);
    // the answer was lost; agentd asks again and is told the same message again, marked as a redelivery
    let again = r.claim(1, "s1", 1).await;
    match again {
        ClaimResponse::Claimed {
            messages,
            redelivered,
        } => {
            assert!(redelivered);
            assert_eq!(ids(&messages), [mid(1)]);
            assert_eq!(messages[0].text, "a");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        r.state(2).await,
        MessageState::Pending,
        "the redelivery claimed nothing new"
    );
    // once the outcome is posted the next claim moves on
    r.ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 1 },
        )
        .await
        .unwrap();
    assert_eq!(ids(&claimed(r.claim(1, "s1", 1).await)), [mid(2)]);
    // a message another incarnation holds is nobody else's to be given
    r.session(2, "s1").await;
    assert_eq!(r.state(2).await, MessageState::DeliveryUnknown);
}

// ---- the outcome ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_submitted_message_is_delivered_in_its_turn_and_a_repeat_is_harmless() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    let o = r
        .ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 4 },
        )
        .await
        .unwrap();
    assert_eq!(o.state, MessageState::Delivered);
    let v = r.view(1).await;
    assert_eq!(v.delivered_turn, Some(4));
    assert_eq!(
        v.history.iter().map(|s| s.state).collect::<Vec<_>>(),
        [
            MessageState::Pending,
            MessageState::Dispatching,
            MessageState::Delivered
        ]
    );
    let writes = r.table.stats().committed;
    r.ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 4 },
        )
        .await
        .unwrap();
    assert_eq!(r.table.stats().committed, writes);
    // a different turn for a delivered message is a contradiction
    let e = r
        .ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 5 },
        )
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::BadTransition(_)));
    // a slot is free again
    assert_eq!(r.ledger.list(RUN).await.unwrap().messages.len(), 1);
}

#[tokio::test]
async fn only_a_dispatching_message_can_be_reported_submitted_failed_or_released() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    for o in [
        MessageOutcome::Submitted { turn: 1 },
        MessageOutcome::Failed { detail: "x".into() },
    ] {
        let e = r
            .ledger
            .outcome(RUN, &gen_(1), "s1", &mid(1), 1, &o)
            .await
            .unwrap_err();
        assert!(
            matches!(refusal(e), Refusal::BadTransition(_)),
            "{o:?} on a pending message"
        );
    }
    // a release answers a claim: nobody claimed this message, so there is nothing to give back
    let e = r
        .outcome(1, "s1", 1, 1, &MessageOutcome::Released)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleAttempt);
    assert_eq!(r.state(1).await, MessageState::Pending);
    let e = r
        .ledger
        .outcome(RUN, &gen_(1), "s1", &mid(404), 1, &MessageOutcome::Released)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NotFound);
}

#[tokio::test]
async fn a_failed_message_keeps_a_bounded_detail() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    let long = "e".repeat(MAX_DETAIL_CHARS + 50);
    r.ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Failed { detail: long },
        )
        .await
        .unwrap();
    let v = r.view(1).await;
    assert_eq!(v.state, MessageState::Failed);
    assert_eq!(v.detail.unwrap().chars().count(), MAX_DETAIL_CHARS);
}

#[tokio::test]
async fn a_released_claim_returns_to_pending_in_the_next_mode() {
    let r = rig();
    r.session(1, "s1").await;
    r.ledger
        .admit(RUN, OWNER, &send_now(1, "interrupt"))
        .await
        .unwrap();
    claimed(r.claim(1, "s1", 1).await);
    r.ledger
        .outcome(RUN, &gen_(1), "s1", &mid(1), 1, &MessageOutcome::Released)
        .await
        .unwrap();
    let v = r.view(1).await;
    assert_eq!(
        (v.state, v.mode),
        (MessageState::Pending, MessageMode::Next)
    );
    let n = r.ledger.notices(RUN).await.unwrap();
    assert_eq!(
        (n.len(), n[0].mode),
        (1, MessageMode::Next),
        "the notice carries the demoted mode"
    );
    // it can be claimed again, by this incarnation
    assert_eq!(ids(&claimed(r.claim(1, "s1", 1).await)), [mid(1)]);
}

#[tokio::test]
async fn a_claim_given_back_after_the_session_changed_is_held_not_left_pending_in_an_old_session() {
    let r = rig();
    r.session(1, "s1").await;
    r.ledger
        .admit(RUN, OWNER, &send_now(1, "interrupt"))
        .await
        .unwrap();
    claimed(r.claim(1, "s1", 1).await);
    r.session(1, "s2").await; // the same incarnation started a new session while the claim was out
    r.ledger
        .outcome(RUN, &gen_(1), "s1", &mid(1), 1, &MessageOutcome::Released)
        .await
        .unwrap();
    let v = r.view(1).await;
    assert_eq!((v.state, v.mode), (MessageState::Held, MessageMode::Next));
    assert!(
        r.ledger.notices(RUN).await.unwrap().is_empty(),
        "a held message is not offered"
    );
}

#[tokio::test]
async fn agentd_may_reject_a_pending_or_claimed_message_only_for_its_own_reasons() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(2)), 1, false)
            .await
            .unwrap(),
    );
    // message 1 was never claimed (attempt 0); message 2 is the first claim of its message (attempt 1)
    for (n, attempt, reason) in [
        (1, 0, RejectReason::ContinuationLimit),
        (2, 1, RejectReason::EngineBusy),
    ] {
        r.ledger
            .outcome(
                RUN,
                &gen_(1),
                "s1",
                &mid(n),
                attempt,
                &MessageOutcome::Rejected { reason },
            )
            .await
            .unwrap();
        let v = r.view(n).await;
        assert_eq!((v.state, v.reason), (MessageState::Rejected, Some(reason)));
    }
    r.admit(3, "c").await;
    for reason in [
        RejectReason::RunSealed,
        RejectReason::RunCancelled,
        RejectReason::Limit,
        RejectReason::NoMessageControl,
    ] {
        let e = r
            .ledger
            .outcome(
                RUN,
                &gen_(1),
                "s1",
                &mid(3),
                1,
                &MessageOutcome::Rejected { reason },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(refusal(e), Refusal::BadTransition(_)),
            "{reason:?}"
        );
    }
    assert_eq!(r.state(3).await, MessageState::Pending);
}

#[tokio::test]
async fn an_outcome_is_fenced_by_the_incarnation_and_the_claim_not_by_the_name_of_the_session() {
    // the session id on an outcome is informative (review of 2026-10-16): the engine may have changed session between the claim and the
    // outcome, and the outcome of a message agentd really sent must still be recorded
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    let o = r
        .outcome(
            1,
            "some-other-session",
            1,
            1,
            &MessageOutcome::Submitted { turn: 1 },
        )
        .await
        .unwrap();
    assert_eq!(o.state, MessageState::Delivered);
    // and after the session changed under the same incarnation
    r.admit(2, "b").await;
    claimed(r.claim(1, "s1", 1).await);
    r.session(1, "s2").await;
    r.outcome(1, "s2", 2, 1, &MessageOutcome::Submitted { turn: 2 })
        .await
        .unwrap();
    assert_eq!(r.state(2).await, MessageState::Delivered);
    // what still fences an outcome is the incarnation
    r.admit(3, "c").await;
    claimed(r.claim(1, "s2", 1).await);
    let e = r
        .outcome(2, "s2", 3, 1, &MessageOutcome::Submitted { turn: 3 })
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleGeneration);
    assert_eq!(r.state(3).await, MessageState::Dispatching);
}

// ---- the seal ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_seal_with_a_pending_message_returns_it_and_leaves_admission_open() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "late").await;
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    match res {
        SealResponse::Pending { messages } => assert_eq!(
            messages
                .iter()
                .map(|m| m.message_id.clone())
                .collect::<Vec<_>>(),
            [mid(1)]
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.ledger.list(RUN).await.unwrap().admission, Admission::Open);
    assert_eq!(
        r.state(1).await,
        MessageState::Pending,
        "a seal that returns messages claims none"
    );
}

#[tokio::test]
async fn a_seal_with_nothing_pending_closes_admission_and_a_later_message_is_refused() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    claimed(r.claim(1, "s1", 1).await);
    r.ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 1 },
        )
        .await
        .unwrap();
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    assert_eq!(
        res,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 0,
            delivery_unknown: 0
        }
    );
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(page.admission, Admission::Closed);
    assert!(matches!(
        r.refused(&send(2, "too late")).await,
        Refusal::RunSealed(Some(RejectReason::RunSealed))
    ));
    assert_eq!(
        r.ledger.list(RUN).await.unwrap().messages.len(),
        1,
        "the refused message stored nothing"
    );
    // a second seal is the same answer
    assert_eq!(
        r.ledger
            .seal(RUN, &gen_(1), "s1", SealMode::Normal)
            .await
            .unwrap(),
        SealResponse::Sealed {
            watermark: 1,
            rejected: 0,
            delivery_unknown: 0
        }
    );
}

#[tokio::test]
async fn a_seal_rejects_the_held_messages_in_the_same_write_that_closes_admission() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.session(1, "s2").await; // 1 is held
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s2", SealMode::Normal)
        .await
        .unwrap();
    assert_eq!(
        res,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 1,
            delivery_unknown: 0
        }
    );
    let v = r.view(1).await;
    assert_eq!(
        (v.state, v.reason),
        (MessageState::Rejected, Some(RejectReason::RunSealed))
    );
}

#[tokio::test]
async fn the_limit_seal_rejects_pending_messages_as_continuation_limit() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::LimitReached)
        .await
        .unwrap();
    assert_eq!(
        res,
        SealResponse::Sealed {
            watermark: 1,
            rejected: 1,
            delivery_unknown: 0
        }
    );
    assert_eq!(
        r.view(1).await.reason,
        Some(RejectReason::ContinuationLimit)
    );
    assert!(matches!(
        r.refused(&send(2, "x")).await,
        Refusal::RunSealed(Some(RejectReason::ContinuationLimit))
    ));
}

#[tokio::test]
async fn the_terminal_seal_never_returns_a_message_and_turns_a_dispatching_one_into_unknown() {
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
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Terminal)
        .await
        .unwrap();
    assert_eq!(
        res,
        SealResponse::Sealed {
            watermark: 2,
            rejected: 1,
            delivery_unknown: 1
        }
    );
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
    assert_eq!(r.state(2).await, MessageState::Rejected);
}

#[tokio::test]
async fn a_seal_before_any_message_or_session_is_fenced_and_a_sealed_run_answers_without_fencing() {
    let r = rig();
    let e = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NoSession);
    r.session(1, "s1").await;
    let e = r
        .ledger
        .seal(RUN, &gen_(1), "other", SealMode::Normal)
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleEpoch);
    r.ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    // after the close a seal from anybody gets the closed answer
    assert_eq!(
        r.ledger
            .seal(RUN, &gen_(1), "s1", SealMode::Normal)
            .await
            .unwrap(),
        SealResponse::Sealed {
            watermark: 0,
            rejected: 0,
            delivery_unknown: 0
        }
    );
}

// ---- close from outside agentd ------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_closes_a_run_that_never_had_a_message_so_a_late_message_finds_it_closed() {
    let r = rig();
    let c = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!((c.rejected, c.delivery_unknown, c.watermark), (0, 0, 0));
    assert!(matches!(
        r.refused(&send(1, "late")).await,
        Refusal::RunSealed(_)
    ));
    // and twice is the same
    assert_eq!(
        r.ledger
            .close(RUN, CloseReason::Finalize)
            .await
            .unwrap()
            .rejected,
        0
    );
}

#[tokio::test]
async fn finalize_rejects_what_is_pending_or_held_and_never_a_delivered_message() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "delivered").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
            .await
            .unwrap(),
    );
    r.ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 1 },
        )
        .await
        .unwrap();
    r.admit(2, "dispatching").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(2)), 1, false)
            .await
            .unwrap(),
    );
    r.admit(3, "pending").await;
    r.session(1, "s2").await; // 3 is held
    r.admit(4, "pending in s2").await;
    let c = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!((c.rejected, c.delivery_unknown, c.watermark), (2, 1, 4));
    assert_eq!(
        r.state(1).await,
        MessageState::Delivered,
        "finalize never rejects a delivered message"
    );
    assert_eq!(r.state(2).await, MessageState::DeliveryUnknown);
    assert_eq!(r.view(3).await.reason, Some(RejectReason::RunSealed));
    assert_eq!(r.view(4).await.reason, Some(RejectReason::RunSealed));
}

#[tokio::test]
async fn a_cancel_rejects_the_open_messages_as_cancelled_and_leaves_a_claimed_one_alone() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "held by agentd").await;
    r.admit(2, "pending").await;
    claimed(
        r.ledger
            .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
            .await
            .unwrap(),
    );
    let c = r.ledger.close(RUN, CloseReason::Cancel).await.unwrap();
    assert_eq!((c.rejected, c.delivery_unknown), (1, 0));
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    assert_eq!(r.view(2).await.reason, Some(RejectReason::RunCancelled));
}

// ---- a confirmation ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn confirming_a_held_message_sends_a_copy_and_supersedes_the_original() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "do the thing").await;
    r.session(1, "s2").await;
    assert_eq!(r.state(1).await, MessageState::Held);
    let a = r
        .ledger
        .admit(RUN, OWNER, &confirm(2, "do the thing", 1, "s2"))
        .await
        .unwrap();
    assert_eq!(a.message.state, MessageState::Pending);
    assert_eq!(a.message.confirms, Some(mid(1)));
    assert_eq!(
        (a.message.epoch.as_deref(), a.message.epoch_no),
        (Some("s2"), 2)
    );
    let old = r.view(1).await;
    assert_eq!(
        (old.state, old.superseded_by),
        (MessageState::Superseded, Some(mid(2)))
    );
    // exactly one of the two can ever be claimed
    let got = claimed(r.claim(1, "s2", 3).await);
    assert_eq!(ids(&got), [mid(2)]);
    // the retry of the confirmation returns the stored copy
    let again = r
        .ledger
        .admit(RUN, OWNER, &confirm(2, "do the thing", 1, "s2"))
        .await
        .unwrap();
    assert_eq!(
        again.outcome,
        harness_protocol::messages::SendOutcome::AlreadyAccepted
    );
}

#[tokio::test]
async fn a_confirmation_fails_when_the_session_changed_again_or_the_message_is_not_held() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.session(1, "s2").await;
    r.session(1, "s3").await; // the user was shown s2
    let e = r
        .ledger
        .admit(RUN, OWNER, &confirm(2, "a", 1, "s2"))
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::EpochChanged);
    assert_eq!(
        r.state(1).await,
        MessageState::Held,
        "a failed confirmation changes nothing"
    );
    assert_eq!(r.ledger.list(RUN).await.unwrap().watermark, 1);
    // a pending message cannot be confirmed
    r.admit(3, "b").await;
    let e = r
        .ledger
        .admit(RUN, OWNER, &confirm(4, "b", 3, "s3"))
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NotHeld(MessageState::Pending));
    // a message that does not exist, or belongs to somebody else
    let e = r
        .ledger
        .admit(RUN, OWNER, &confirm(5, "z", 99, "s3"))
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NotFound);
    let e = r
        .ledger
        .admit(RUN, OTHER, &confirm(6, "a", 1, "s3"))
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::NotFound);
}

#[tokio::test]
async fn a_confirmation_repeats_the_text_and_the_mode_of_the_message_it_replaces() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "original text").await;
    r.session(1, "s2").await;
    let e = r
        .ledger
        .admit(RUN, OWNER, &confirm(2, "edited text", 1, "s2"))
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::ConfirmMismatch);
    let mut now = confirm(3, "original text", 1, "s2");
    now.mode = MessageMode::Now;
    let e = r.ledger.admit(RUN, OWNER, &now).await.unwrap_err();
    assert_eq!(refusal(e), Refusal::ConfirmMismatch);
    assert_eq!(r.state(1).await, MessageState::Held);
}

#[tokio::test]
async fn a_message_that_may_have_reached_cline_can_be_sent_again_on_purpose() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "maybe").await;
    claimed(r.claim(1, "s1", 1).await);
    r.session(2, "s2").await; // the worker was lost
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
    let a = r
        .ledger
        .admit(RUN, OWNER, &confirm(2, "maybe", 1, "s2"))
        .await
        .unwrap();
    assert_eq!(a.message.state, MessageState::Pending);
    assert_eq!(r.state(1).await, MessageState::Superseded);
}

#[tokio::test]
async fn a_confirmation_frees_the_slot_of_the_message_it_replaces() {
    let r = rig();
    let cap = harness_protocol::messages::MAX_OUTSTANDING_MESSAGES;
    r.session(1, "s1").await;
    r.fill(cap).await;
    r.session(1, "s2").await; // every message is held, the cap is reached
    assert_eq!(
        r.refused(&send(cap + 1, "d")).await,
        Refusal::TooManyPending
    );
    r.ledger
        .admit(RUN, OWNER, &confirm(cap + 1, "m", 1, "s2"))
        .await
        .unwrap();
    assert_eq!(
        r.ledger
            .list(RUN)
            .await
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.state.is_open())
            .count(),
        cap as usize
    );
}

#[test]
fn the_clock_formats_unix_seconds_as_utc_timestamps() {
    assert_eq!(message_ledger::rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(message_ledger::rfc3339(T0), "2026-10-16T14:02:11Z");
    assert_eq!(message_ledger::rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(
        message_ledger::rfc3339(1_709_164_799),
        "2024-02-28T23:59:59Z"
    );
    assert_eq!(
        message_ledger::rfc3339(-5),
        "1970-01-01T00:00:00Z",
        "a clock before 1970 reads as the epoch"
    );
    assert_eq!(message_ledger::Now::at(T0).minute(), 29_869_322);
}
