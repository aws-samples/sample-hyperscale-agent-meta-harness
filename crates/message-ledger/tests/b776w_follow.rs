//! follow-up (limits audit row D09): twenty messages may wait for a running Cline, and a claim still takes three at a time. The
//! notices of a seal answer carry no text, so twenty of them are small; a claim carries the text of up to three messages of 256 KiB, which is
//! what one Lambda answer holds, so the claim batch stays where it is.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, MAX_CLAIM_BATCH, MAX_OUTSTANDING_MESSAGES, MessageState, SealMode, SealResponse,
};
use message_ledger::{Ledger, LedgerError, Refusal};
use std::sync::Arc;
use support::*;

#[tokio::test]
async fn b776w_twenty_messages_wait_and_the_twenty_first_is_refused() {
    assert_eq!(MAX_OUTSTANDING_MESSAGES, 20);
    let r = rig();
    r.fill(MAX_OUTSTANDING_MESSAGES).await;
    let e = r
        .refused(&send(MAX_OUTSTANDING_MESSAGES + 1, "one too many"))
        .await;
    assert_eq!(e, Refusal::TooManyPending);
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(page.messages.len(), 20, "a refused message stores nothing");
    assert_eq!(page.watermark, 20);
    // a withdrawn message frees its slot, and the next message takes the next sequence number
    r.ledger.withdraw(RUN, &mid(7)).await.unwrap();
    let ok = r.admit(21, "now there is room").await;
    assert_eq!(ok.message.seq, 21);
    assert_eq!(
        r.refused(&send(22, "full again")).await,
        Refusal::TooManyPending
    );
}

#[tokio::test]
async fn b776w_the_refusal_names_the_number_that_the_constant_holds() {
    let r = rig();
    r.fill(MAX_OUTSTANDING_MESSAGES).await;
    let e = r.refused(&send(MAX_OUTSTANDING_MESSAGES + 1, "x")).await;
    assert_eq!(
        e.message(),
        "20 messages are waiting for Cline already; wait for one to be delivered or withdraw one"
    );
}

#[tokio::test]
async fn b776w_a_seal_lists_all_twenty_waiting_notices_in_server_order() {
    let r = rig();
    r.session(1, "s1").await;
    r.fill(MAX_OUTSTANDING_MESSAGES).await;
    let answer = r
        .ledger
        .seal(RUN, &gen_(1), "s1", SealMode::Normal)
        .await
        .unwrap();
    let SealResponse::Pending { messages } = answer else {
        panic!("expected pending notices, got {answer:?}");
    };
    assert_eq!(messages.len(), 20);
    let seqs: Vec<u32> = messages.iter().map(|m| m.seq).collect();
    assert_eq!(seqs, (1..=20).collect::<Vec<u32>>());
    // a seal with waiting messages leaves admission open
    assert_eq!(r.ledger.list(RUN).await.unwrap().admission, Admission::Open);
}

#[tokio::test]
async fn b776w_a_claim_takes_three_messages_at_a_time_while_twenty_wait() {
    assert_eq!(MAX_CLAIM_BATCH, 3);
    let r = rig();
    r.session(1, "s1").await;
    r.fill(MAX_OUTSTANDING_MESSAGES).await;
    // a batch of 20 is cut to the claim batch, oldest first
    let first = claimed(r.claim(1, "s1", 20).await);
    assert_eq!(ids(&first), [mid(1), mid(2), mid(3)]);
    for n in 1..=3 {
        assert_eq!(r.state(n).await, MessageState::Dispatching);
    }
    assert_eq!(r.state(4).await, MessageState::Pending);
    // the claimed messages still count toward the twenty
    assert_eq!(r.refused(&send(21, "full")).await, Refusal::TooManyPending);
}

#[tokio::test]
async fn b776w_held_messages_count_toward_twenty_and_a_confirmation_replaces_one() {
    let r = rig();
    r.session(1, "s1").await;
    r.fill(MAX_OUTSTANDING_MESSAGES).await;
    r.session(1, "s2").await; // a new session: every pending message is held
    assert_eq!(r.state(1).await, MessageState::Held);
    assert_eq!(r.state(20).await, MessageState::Held);
    assert_eq!(r.refused(&send(21, "d")).await, Refusal::TooManyPending);
    // a confirmation takes the place of the held message it names, so the count stays at twenty
    let ok = r
        .ledger
        .admit(RUN, OWNER, &confirm(21, "m", 1, "s2"))
        .await
        .unwrap();
    assert_eq!(ok.message.state, MessageState::Pending);
    assert_eq!(r.state(1).await, MessageState::Superseded);
    assert_eq!(r.refused(&send(22, "e")).await, Refusal::TooManyPending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn b776w_concurrent_admissions_stop_at_twenty_and_share_no_sequence_number() {
    let r = rig();
    // sixteen waiting, the clock in the fourth minute: four slots are free and the run has room for ten admissions in this minute
    r.fill(MAX_OUTSTANDING_MESSAGES - 4).await;
    let ledger = Arc::new(Ledger::with_clock(r.table.clone(), r.clock.clone()));
    let mut handles = Vec::new();
    for n in 17..=24u32 {
        let l = ledger.clone();
        handles.push(tokio::spawn(async move {
            l.admit(RUN, OWNER, &send(n, "m")).await
        }));
    }
    let (mut ok, mut full) = (0u32, 0u32);
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(LedgerError::Refused(Refusal::TooManyPending)) => full += 1,
            Err(other) => panic!("{other:?}"),
        }
    }
    assert_eq!((ok, full), (4, 4));
    let page = ledger.list(RUN).await.unwrap();
    let mut seqs: Vec<u32> = page.messages.iter().map(|m| m.seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=20).collect::<Vec<u32>>());
    assert_eq!(page.watermark, 20);
}
