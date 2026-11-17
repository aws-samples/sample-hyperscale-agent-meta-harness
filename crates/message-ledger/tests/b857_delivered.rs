//! The messages of a run that were delivered to an engine, as a later session of the run is told them (`Ledger::delivered`).
//!
//! The read answers by the ledger's own states: only `delivered` counts. The text is the one control-api stored when the user sent the
//! message. A worker's report names a message by its id and moves its state; no report carries a text that the read returns. The read
//! writes nothing and takes no generation and no session, so it moves no message and fences nobody.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    MAX_HISTORY_TEXT_BYTES, MessageOutcome, MessageState, RejectReason,
};
use message_ledger::CloseReason;
use support::*;

/// Claims the next message for incarnation `g` in session `epoch` and reports it as sent in `turn`.
async fn deliver(r: &Rig, g: u32, epoch: &str, turn: u32) -> String {
    let got = claimed(r.claim(g, epoch, 1).await);
    assert_eq!(got.len(), 1);
    let n = got[0].seq;
    r.outcome(
        g,
        epoch,
        n,
        got[0].attempt,
        &MessageOutcome::Submitted { turn },
    )
    .await
    .unwrap();
    got[0].message_id.clone()
}

#[tokio::test]
async fn b857_a_run_with_no_message_has_no_delivered_message_and_the_read_creates_nothing() {
    let r = rig();
    let got = r.ledger.delivered(RUN).await.unwrap();
    assert!(got.messages.is_empty());
    assert_eq!(got.cut, 0);
    assert!(r.table.all().is_empty(), "the read made no item");
    assert_eq!(r.table.stats().committed, 0);
}

#[tokio::test]
async fn b857_delivered_messages_come_back_in_the_order_admitted_with_their_ids_and_stored_texts() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "use tabs, not spaces").await;
    r.admit(2, "name the file REPORT.md").await;
    r.admit(3, "skip the Windows build").await;
    // Delivered out of the order admitted: 1, then 3 by its id, then 2.
    assert_eq!(deliver(&r, 1, "s1", 2).await, mid(1));
    let c = r
        .ledger
        .claim(RUN, &gen_(1), "s1", Some(&mid(3)), 1, false)
        .await
        .unwrap();
    let c = claimed(c);
    r.outcome(
        1,
        "s1",
        3,
        c[0].attempt,
        &MessageOutcome::Submitted { turn: 3 },
    )
    .await
    .unwrap();
    assert_eq!(deliver(&r, 1, "s1", 4).await, mid(2));
    let writes = r.table.stats().committed;
    let got = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        got.messages
            .iter()
            .map(|m| (
                m.message_id.clone(),
                m.seq,
                m.delivered_turn,
                m.text.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            (mid(1), 1, Some(2), "use tabs, not spaces"),
            (mid(2), 2, Some(4), "name the file REPORT.md"),
            (mid(3), 3, Some(3), "skip the Windows build"),
        ],
        "the ledger's order is the order the messages were admitted in"
    );
    assert_eq!(got.cut, 0);
    assert_eq!(r.table.stats().committed, writes, "the read wrote nothing");
    // another run reads nothing of this one
    assert!(r.ledger.delivered(RUN2).await.unwrap().messages.is_empty());
}

#[tokio::test]
async fn b857_only_the_state_delivered_counts() {
    let r = rig();
    r.session(1, "s1").await;
    for (n, text) in [
        (1, "delivered"),
        (2, "failed"),
        (3, "rejected"),
        (4, "withdrawn"),
        (5, "dispatching"),
        (6, "pending"),
    ] {
        r.admit(n, text).await;
    }
    // 1 delivered
    assert_eq!(deliver(&r, 1, "s1", 2).await, mid(1));
    // 2 claimed and never sent: failed. The detail is the worker's text and is never a message text.
    let c = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(c[0].message_id, mid(2));
    r.outcome(
        1,
        "s1",
        2,
        c[0].attempt,
        &MessageOutcome::Failed {
            detail: "not-sent: words of the worker".into(),
        },
    )
    .await
    .unwrap();
    // 3 rejected by the worker
    r.outcome(
        1,
        "s1",
        3,
        0,
        &MessageOutcome::Rejected {
            reason: RejectReason::ContinuationLimit,
        },
    )
    .await
    .unwrap();
    // 4 withdrawn by the user
    r.ledger.withdraw(RUN, &mid(4)).await.unwrap();
    // 5 claimed, no outcome yet: dispatching. 6 stays pending.
    let c = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(c[0].message_id, mid(5));
    assert_eq!(r.state(5).await, MessageState::Dispatching);
    assert_eq!(r.state(6).await, MessageState::Pending);
    let got = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        got.messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["delivered"]
    );
    assert_eq!(
        got.cut, 0,
        "a message that was not delivered is not a cut one"
    );
    assert!(
        !serde_json::to_string(&got)
            .unwrap()
            .contains("words of the worker"),
        "a worker's report supplies no text"
    );
}

/// A delivery whose report reached the ledger a moment before the session was lost is a delivered message for the session that follows.
/// One whose report never arrived is not: the ledger holds it as `dispatching`, the next incarnation's session report makes it
/// `delivery-unknown`, and it is sent again only when the user confirms it. Messages that were pending become `held` the same way.
#[tokio::test]
async fn b857_a_report_that_arrived_before_the_session_was_lost_counts_and_one_that_did_not_is_never_called_delivered()
 {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "reported in time").await;
    r.admit(2, "sent, the report was lost").await;
    r.admit(3, "still pending").await;
    assert_eq!(deliver(&r, 1, "s1", 2).await, mid(1));
    let c = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(c[0].message_id, mid(2));
    // The session is lost here. The next machine asks before it has an engine session: nothing about incarnation 1 has changed yet.
    let asked = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        asked
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["reported in time"]
    );
    assert_eq!(
        r.state(2).await,
        MessageState::Dispatching,
        "the read fenced nobody and settled nothing"
    );
    // The new incarnation reports its session: the ledger's own rules settle the other two.
    let ack = r.session(2, "s2").await;
    assert_eq!((ack.held, ack.delivery_unknown), (1, 1));
    assert_eq!(r.state(2).await, MessageState::DeliveryUnknown);
    assert_eq!(r.state(3).await, MessageState::Held);
    let after = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(after, asked, "neither is called delivered");
    // The user confirms the unknown one: the copy is a new message, pending in the new session, and is delivered there as a prompt.
    r.ledger
        .admit(
            RUN,
            OWNER,
            &confirm(4, "sent, the report was lost", 2, "s2"),
        )
        .await
        .unwrap();
    assert_eq!(r.state(2).await, MessageState::Superseded);
    assert_eq!(
        r.ledger.delivered(RUN).await.unwrap().messages,
        asked.messages,
        "pending is not delivered"
    );
    assert_eq!(deliver(&r, 2, "s2", 2).await, mid(4));
    let last = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        last.messages
            .iter()
            .map(|m| (m.message_id.clone(), m.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (mid(1), "reported in time"),
            (mid(4), "sent, the report was lost")
        ],
        "once, under the id of the copy that was delivered"
    );
}

#[tokio::test]
async fn b857_the_answer_keeps_whole_messages_within_the_bound_and_counts_the_older_ones_it_cut() {
    let r = rig();
    r.session(1, "s1").await;
    // Seven messages of 1,000 characters of three bytes each: 3,000 bytes a message, 21,000 bytes in all.
    let text = |n: u32| format!("{n}{}", "\u{20ac}".repeat(1000));
    for n in 1..=7u32 {
        r.clock.set(T0 + i64::from(n) * 60);
        r.admit(n, &text(n)).await;
        deliver(&r, 1, "s1", n + 1).await;
    }
    let got = r.ledger.delivered(RUN).await.unwrap();
    let bytes: usize = got.messages.iter().map(|m| m.text.len()).sum();
    assert!(bytes <= MAX_HISTORY_TEXT_BYTES, "{bytes}");
    assert_eq!(got.cut, 2, "the two oldest");
    assert_eq!(
        got.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [3, 4, 5, 6, 7]
    );
    for m in &got.messages {
        assert_eq!(m.text, text(m.seq), "a kept message is whole");
    }
}

#[tokio::test]
async fn b857_a_closed_run_still_answers_what_was_delivered() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "keep the public names").await;
    r.admit(2, "never reached").await;
    deliver(&r, 1, "s1", 2).await;
    r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    let got = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        got.messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["keep the public names"]
    );
}

// ---- messages handed to an earlier session with no report of their delivery ------------------------------------------------------

/// What an answer names as handed to an earlier session with no report of the delivery: the id, the order and the text of each.
fn unconfirmed(answer: &harness_protocol::messages::HistoryResponse) -> Vec<(String, u32, String)> {
    let v = serde_json::to_value(answer).unwrap();
    v.get("unconfirmed")
        .and_then(|u| u.as_array())
        .map(|a| {
            a.iter()
                .map(|m| {
                    (
                        m["messageId"].as_str().unwrap().to_string(),
                        u32::try_from(m["seq"].as_u64().unwrap()).unwrap(),
                        m["text"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A session that is lost between the hand-over of a message and the report of its delivery leaves the message `dispatching`, and the
/// session report of the next incarnation makes it `delivery-unknown`. In both states the read names it, apart from the delivered
/// messages, with the text the ledger stored: the engine of the lost session may have received it. When the user confirms it, the
/// copy takes its place and the message is named once, as the delivered copy.
#[tokio::test]
async fn b857_a_message_handed_to_a_session_that_was_lost_before_its_report_is_answered_as_unconfirmed()
 {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "reported in time").await;
    r.admit(2, "sent, the report was lost").await;
    r.admit(3, "still pending").await;
    assert_eq!(deliver(&r, 1, "s1", 2).await, mid(1));
    let c = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(c[0].message_id, mid(2));
    let writes = r.table.stats().committed;
    // The session is lost here. The next machine asks before it has an engine session.
    let asked = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        r.state(2).await,
        MessageState::Dispatching,
        "TRIGGER PRESENT: handed over, no report"
    );
    assert_eq!(
        asked
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["reported in time"]
    );
    assert_eq!(
        unconfirmed(&asked),
        [(mid(2), 2, "sent, the report was lost".to_string())],
        "named apart from the delivered messages, with the stored text"
    );
    assert_eq!(r.table.stats().committed, writes, "the read wrote nothing");
    // The new incarnation reports its session: the ledger's own rules settle the two open messages.
    let ack = r.session(2, "s2").await;
    assert_eq!((ack.held, ack.delivery_unknown), (1, 1));
    assert_eq!(r.state(2).await, MessageState::DeliveryUnknown);
    let later = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(later.messages, asked.messages);
    assert_eq!(
        unconfirmed(&later),
        unconfirmed(&asked),
        "a session after that one is told the same"
    );
    // A message that waited and is held now never left the service: it is in neither list.
    let all = serde_json::to_string(&later).unwrap();
    assert!(!all.contains("still pending"), "{all}");
    // The user confirms the unknown one: the copy is a new message, and the original is superseded and leaves the answer.
    r.ledger
        .admit(
            RUN,
            OWNER,
            &confirm(4, "sent, the report was lost", 2, "s2"),
        )
        .await
        .unwrap();
    assert_eq!(r.state(2).await, MessageState::Superseded);
    let confirmed = r.ledger.delivered(RUN).await.unwrap();
    assert!(
        unconfirmed(&confirmed).is_empty(),
        "the copy is pending and comes as a prompt of its own: {confirmed:?}"
    );
    assert_eq!(deliver(&r, 2, "s2", 2).await, mid(4));
    let last = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        last.messages
            .iter()
            .map(|m| (m.message_id.clone(), m.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (mid(1), "reported in time"),
            (mid(4), "sent, the report was lost")
        ]
    );
    assert!(unconfirmed(&last).is_empty(), "once, as the delivered copy");
}

/// Only a claim without a report is unconfirmed. A message the worker reported as never sent, one that was withdrawn, and one that
/// still waits are in neither list. A claim that is open when the run closes becomes `delivery-unknown` and stays named.
#[tokio::test]
async fn b857_only_a_claim_without_a_report_is_unconfirmed() {
    let r = rig();
    r.session(1, "s1").await;
    for (n, text) in [
        (1, "never sent"),
        (2, "withdrawn"),
        (3, "claimed, no report"),
        (4, "waiting"),
    ] {
        r.admit(n, text).await;
    }
    let c = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(c[0].message_id, mid(1));
    r.outcome(
        1,
        "s1",
        1,
        c[0].attempt,
        &MessageOutcome::Failed {
            detail: "not-sent: words of the worker".into(),
        },
    )
    .await
    .unwrap();
    r.ledger.withdraw(RUN, &mid(2)).await.unwrap();
    let c = claimed(r.claim(1, "s1", 1).await);
    assert_eq!(c[0].message_id, mid(3));
    assert_eq!(r.state(4).await, MessageState::Pending);
    let got = r.ledger.delivered(RUN).await.unwrap();
    assert!(got.messages.is_empty());
    assert_eq!(got.cut, 0);
    assert_eq!(
        unconfirmed(&got),
        [(mid(3), 3, "claimed, no report".to_string())]
    );
    // The run closes while the claim is open: the ledger calls the message `delivery-unknown`, and the waiting one is rejected.
    r.ledger.close(RUN, CloseReason::Finalize).await.unwrap();
    assert_eq!(r.state(3).await, MessageState::DeliveryUnknown);
    assert_eq!(r.state(4).await, MessageState::Rejected);
    let got = r.ledger.delivered(RUN).await.unwrap();
    assert_eq!(
        unconfirmed(&got),
        [(mid(3), 3, "claimed, no report".to_string())]
    );
    assert!(
        !serde_json::to_string(&got)
            .unwrap()
            .contains("words of the worker"),
        "a worker's report supplies no text"
    );
}

/// One bound holds the delivered and the unconfirmed messages together: whole messages, and the oldest of either kind are cut first
/// and counted.
#[tokio::test]
async fn b857_one_bound_holds_delivered_and_unconfirmed_messages_and_the_oldest_are_cut_first() {
    let r = rig();
    r.session(1, "s1").await;
    // Seven messages of 1,000 characters of three bytes each: 3,001 bytes a message, 21,007 bytes in all.
    let text = |n: u32| format!("{n}{}", "\u{20ac}".repeat(1000));
    for n in 1..=7u32 {
        r.clock.set(T0 + i64::from(n) * 60);
        r.admit(n, &text(n)).await;
    }
    for n in 1..=5u32 {
        deliver(&r, 1, "s1", n + 1).await;
    }
    // The last two are handed over, and the session is lost before a report.
    let c = claimed(r.claim(1, "s1", 2).await);
    assert_eq!(ids(&c), [mid(6), mid(7)]);
    let got = r.ledger.delivered(RUN).await.unwrap();
    let named = unconfirmed(&got);
    let bytes: usize = got.messages.iter().map(|m| m.text.len()).sum::<usize>()
        + named.iter().map(|m| m.2.len()).sum::<usize>();
    assert!(bytes <= MAX_HISTORY_TEXT_BYTES, "{bytes}");
    assert_eq!(got.cut, 2, "the two oldest of both kinds together");
    assert_eq!(
        got.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(named.iter().map(|m| m.1).collect::<Vec<_>>(), [6, 7]);
    for m in &got.messages {
        assert_eq!(m.text, text(m.seq), "a kept message is whole");
    }
    for m in &named {
        assert_eq!(m.2, text(m.1), "a kept message is whole");
    }
}
