//! The races names: a withdrawal and a claim of one message, a late message and the seal, a finalize and a claim,
//! two requests that read the same version. A competing writer is put exactly between the read and the write of the operation under test
//! (`MemTable::before_next_transact`), so the outcome does not depend on timing.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, ClaimResponse, MAX_OUTSTANDING_MESSAGES, MessageOutcome, MessageState, SealMode,
    SealResponse,
};
use message_ledger::{CloseReason, Ledger, Refusal};
use std::sync::Arc;
use support::*;

fn block<F>(f: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    // the hook runs inside a transaction call that is already on the runtime: a new current-thread runtime on its own thread
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

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_that_loses_to_a_withdrawal_finds_the_message_withdrawn_and_sends_nothing() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    // the claim has read the message pending; the user withdraws it before the claim writes
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move { rival.withdraw(RUN, &mid(1)).await.unwrap() });
    });
    let got = r.claim(1, "s1", 1).await;
    assert_eq!(
        got,
        ClaimResponse::Nothing {
            admission: Admission::Open
        }
    );
    assert_eq!(r.state(1).await, MessageState::Withdrawn);
    assert!(
        r.view(1)
            .await
            .history
            .iter()
            .all(|s| s.state != MessageState::Dispatching),
        "a withdrawn message was never dispatching"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawal_that_loses_to_a_claim_is_refused_and_the_message_is_dispatched_once() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move {
            rival
                .claim(RUN, &gen_(1), "s1", None, 1, false)
                .await
                .unwrap()
        });
    });
    let e = r.ledger.withdraw(RUN, &mid(1)).await.unwrap_err();
    assert_eq!(refusal(e), Refusal::AlreadyDispatching);
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    let h = r.view(1).await.history;
    assert_eq!(
        h.iter()
            .filter(|s| s.state == MessageState::Dispatching)
            .count(),
        1
    );
    assert!(h.iter().all(|s| s.state != MessageState::Withdrawn));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_arrives_between_the_seals_read_and_write_is_not_lost() {
    let r = rig();
    r.session(1, "s1").await;
    // the seal reads an empty ledger; a message is admitted before it writes
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move {
            rival
                .admit(RUN, OWNER, &send(1, "just in time"))
                .await
                .unwrap()
        });
    });
    let res = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    match res {
        SealResponse::Pending { messages } => assert_eq!(messages.len(), 1),
        other => panic!("the seal must return the late message, got {other:?}"),
    }
    assert_eq!(r.ledger.list(RUN).await.unwrap().admission, Admission::Open);
    assert_eq!(r.state(1).await, MessageState::Pending);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_loses_to_the_seal_is_refused_with_run_sealed_and_stores_nothing() {
    let r = rig();
    r.session(1, "s1").await;
    // the admission has read an open ledger; the seal closes it before the message is written
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move {
            rival
                .seal(RUN, &gen_(1), "s1", SealMode::Normal)
                .await
                .unwrap()
        });
    });
    let e = r
        .ledger
        .admit(RUN, OWNER, &send(1, "too late"))
        .await
        .unwrap_err();
    assert!(matches!(refusal(e), Refusal::RunSealed(_)));
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(
        (page.messages.len(), page.admission, page.watermark),
        (0, Admission::Closed, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_that_loses_to_the_finalize_close_is_told_admission_is_closed() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move { rival.close(RUN, CloseReason::Finalize).await.unwrap() });
    });
    let got = r.claim(1, "s1", 1).await;
    assert_eq!(
        got,
        ClaimResponse::Refused {
            reason: harness_protocol::messages::ClaimRefusal::AdmissionClosed
        }
    );
    let v = r.view(1).await;
    assert_eq!(
        v.state,
        MessageState::Rejected,
        "finalize rejected it before it could be claimed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finalize_close_that_loses_to_a_claim_makes_the_claimed_message_unknown_not_rejected() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move {
            rival
                .claim(RUN, &gen_(1), "s1", None, 1, false)
                .await
                .unwrap()
        });
    });
    let c = r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!((c.rejected, c.delivery_unknown), (0, 1));
    assert_eq!(
        r.state(1).await,
        MessageState::DeliveryUnknown,
        "a claimed message is never reported rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outcome_that_loses_to_a_replaced_incarnation_is_fenced_and_the_message_stays_unknown() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.claim(1, "s1", 1).await;
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move { rival.report_session(RUN, &gen_(2), "s9", 1).await.unwrap() });
    });
    let e = r
        .ledger
        .outcome(
            RUN,
            &gen_(1),
            "s1",
            &mid(1),
            1,
            &MessageOutcome::Submitted { turn: 3 },
        )
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::StaleGeneration);
    assert_eq!(r.state(1).await, MessageState::DeliveryUnknown);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_requests_that_read_the_same_version_are_serialized_by_their_conditions() {
    let r = rig();
    r.admit(1, "a").await;
    // admit 2 reads version 1 of the control item, admit 3 writes first: admit 2 starts over and takes the next sequence number
    let rival = Ledger::with_clock(r.table.clone(), r.clock.clone());
    r.table.before_next_transact(move |_| {
        block(async move { rival.admit(RUN, OWNER, &send(3, "rival")).await.unwrap() });
    });
    let b = r.ledger.admit(RUN, OWNER, &send(2, "retry")).await.unwrap();
    assert_eq!(
        b.message.seq, 3,
        "the rival took seq 2, so this message is the third"
    );
    let seqs: Vec<(u32, String)> = r
        .ledger
        .list(RUN)
        .await
        .unwrap()
        .messages
        .into_iter()
        .map(|m| (m.seq, m.message_id))
        .collect();
    assert_eq!(seqs, [(1, mid(1)), (2, mid(3)), (3, mid(2))]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_table_is_retried_and_a_table_that_stays_busy_is_unavailable() {
    let r = rig();
    r.table
        .fail_next(message_ledger::TableError::Contended("busy".into()));
    r.admit(1, "a").await;
    assert_eq!(
        r.table.stats().committed,
        1,
        "the busy answer was retried and the message was written once"
    );
    for _ in 0..8 {
        r.table
            .fail_next(message_ledger::TableError::ConditionFailed { index: 0 });
    }
    let e = r.ledger.admit(RUN, OWNER, &send(2, "b")).await.unwrap_err();
    assert!(
        matches!(e, message_ledger::LedgerError::Unavailable(_)),
        "{e:?}"
    );
    let list = r.ledger.list(RUN).await.unwrap();
    assert_eq!(
        list.messages.len(),
        1,
        "an unavailable answer stored nothing"
    );
    // the table gave up with a refusal of its own: not retried
    r.table
        .fail_next(message_ledger::TableError::Invalid("too big".into()));
    let e = r.ledger.admit(RUN, OWNER, &send(3, "c")).await.unwrap_err();
    assert!(matches!(e, message_ledger::LedgerError::Unavailable(m) if m.contains("too big")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_admissions_never_exceed_the_cap_and_never_share_a_sequence_number() {
    let r = rig();
    let ledger = Arc::new(Ledger::with_clock(r.table.clone(), r.clock.clone()));
    let mut handles = Vec::new();
    for n in 1..=6u32 {
        let l = ledger.clone();
        handles.push(tokio::spawn(async move {
            l.admit(RUN, OWNER, &send(n, "m")).await
        }));
    }
    let mut ok = 0u32;
    let mut full = 0u32;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(message_ledger::LedgerError::Refused(Refusal::TooManyPending)) => full += 1,
            Err(other) => panic!("{other:?}"),
        }
    }
    assert_eq!(
        (ok, full),
        (MAX_OUTSTANDING_MESSAGES, 6 - MAX_OUTSTANDING_MESSAGES)
    );
    let page = ledger.list(RUN).await.unwrap();
    let mut seqs: Vec<u32> = page.messages.iter().map(|m| m.seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, [1, 2, 3]);
    assert_eq!(page.watermark, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_of_one_message_dispatch_it_once() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "once").await;
    let ledger = Arc::new(Ledger::with_clock(r.table.clone(), r.clock.clone()));
    let mut handles = Vec::new();
    for _ in 0..5 {
        let l = ledger.clone();
        handles.push(tokio::spawn(async move {
            l.claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false).await
        }));
    }
    let mut fresh = 0;
    for h in handles {
        match h.await.unwrap().unwrap() {
            ClaimResponse::Claimed {
                redelivered: false, ..
            } => fresh += 1,
            ClaimResponse::Claimed {
                redelivered: true, ..
            } => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        fresh, 1,
        "exactly one claim moved the message; the others were told it again"
    );
    let h = r.view(1).await.history;
    assert_eq!(
        h.iter()
            .filter(|s| s.state == MessageState::Dispatching)
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_withdrawals_and_claims_leave_each_message_in_one_consistent_state() {
    let r = rig();
    r.session(1, "s1").await;
    for n in 1..=3u32 {
        r.admit(n, "m").await;
    }
    let ledger = Arc::new(Ledger::with_clock(r.table.clone(), r.clock.clone()));
    let mut handles = Vec::new();
    for n in 1..=3u32 {
        let l = ledger.clone();
        handles.push(tokio::spawn(async move {
            l.withdraw(RUN, &mid(n)).await.map(|_| ())
        }));
        let l = ledger.clone();
        handles.push(tokio::spawn(async move {
            l.claim(RUN, &gen_(1), "s1", Some(&mid(n)), 1, false)
                .await
                .map(|_| ())
        }));
    }
    for h in handles {
        let _ = h.await.unwrap();
    }
    for n in 1..=3u32 {
        let v = r.view(n).await;
        // a message is withdrawn or dispatching, never both: the history holds one of the two
        let withdrawn = v.history.iter().any(|s| s.state == MessageState::Withdrawn);
        let dispatched = v
            .history
            .iter()
            .any(|s| s.state == MessageState::Dispatching);
        assert!(withdrawn ^ dispatched, "message {n}: {:?}", v.history);
        assert_eq!(
            v.state,
            if withdrawn {
                MessageState::Withdrawn
            } else {
                MessageState::Dispatching
            }
        );
    }
    // the control item agrees with the items
    let page = ledger.list(RUN).await.unwrap();
    let open = page.messages.iter().filter(|m| m.state.is_open()).count();
    let ctl = r
        .table
        .item(
            &harness_protocol::store::keys::run_pk(RUN),
            harness_protocol::store::keys::MESSAGE_CTL,
        )
        .unwrap();
    assert_eq!(ctl["open"].as_array().unwrap().len(), open);
}
