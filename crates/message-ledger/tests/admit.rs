//! Admission: a message is stored pending with the next sequence number, an identical retry returns the stored
//! message and uses no slot, and the limits hold exactly.

#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::messages::{
    Admission, MAX_OUTSTANDING_MESSAGES, MAX_RUN_ADMISSIONS_PER_MINUTE, MAX_RUN_MESSAGE_BYTES,
    MAX_RUN_MESSAGES, MessageMode, MessageState, SendOutcome,
};
use harness_protocol::store::keys;
use message_ledger::Refusal;
use support::*;

#[tokio::test]
async fn a_message_is_stored_pending_with_the_next_sequence_number() {
    let r = rig();
    let a = r.admit(1, "first").await;
    assert_eq!(a.outcome, SendOutcome::Accepted);
    assert_eq!(a.message.state, MessageState::Pending);
    assert_eq!(
        (a.message.seq, a.message.epoch_no, a.message.epoch.clone()),
        (1, 0, None)
    );
    assert_eq!(a.message.accepted_at, "2026-10-16T14:02:11Z");
    assert_eq!(a.message.history.len(), 1);
    let b = r.admit(2, "second").await;
    assert_eq!(b.message.seq, 2);
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!(page.watermark, 2);
    assert_eq!(page.admission, Admission::Open);
    assert_eq!(
        page.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [1, 2]
    );
    // the item holds what the owner sent and who sent it, under the keys of the decision record
    let item = r
        .table
        .item(&keys::run_pk(RUN), &keys::message_sk(&mid(1)))
        .unwrap();
    assert_eq!(item["principalArn"], OWNER);
    assert_eq!(item["text"], "first");
    assert_eq!(item["version"], 1);
    assert!(item.get("ownerPrincipalArn").is_none() && item.get("createdAt").is_none());
    let ctl = r.table.item(&keys::run_pk(RUN), keys::MESSAGE_CTL).unwrap();
    assert_eq!(
        (ctl["seq"].as_u64(), ctl["version"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(ctl["open"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_message_written_before_the_first_session_belongs_to_the_session_that_follows() {
    let r = rig();
    r.admit(1, "early").await;
    r.session(1, "sess-a").await;
    let v = r.view(1).await;
    assert_eq!(
        v.state,
        MessageState::Pending,
        "the first session is not a change of session"
    );
    assert_eq!((v.epoch.as_deref(), v.epoch_no), (Some("sess-a"), 1));
    // a message written during the session carries it
    let m = r.admit(2, "later").await.message;
    assert_eq!((m.epoch.as_deref(), m.epoch_no), (Some("sess-a"), 1));
}

#[tokio::test]
async fn an_identical_retry_returns_the_stored_message_and_uses_no_slot() {
    let r = rig();
    r.admit(1, "same").await;
    r.session(1, "s1").await;
    let writes = r.table.stats().committed;
    let again = r.ledger.admit(RUN, OWNER, &send(1, "same")).await.unwrap();
    assert_eq!(again.outcome, SendOutcome::AlreadyAccepted);
    assert_eq!(again.message.message_id, mid(1));
    assert_eq!(r.table.stats().committed, writes, "a retry writes nothing");
    let page = r.ledger.list(RUN).await.unwrap();
    assert_eq!((page.watermark, page.messages.len()), (1, 1));
    // the retry reads the message as it is now, not as it was accepted
    r.ledger
        .claim(RUN, &gen_(1), "s1", None, 1, false)
        .await
        .unwrap();
    let later = r.ledger.admit(RUN, OWNER, &send(1, "same")).await.unwrap();
    assert_eq!(later.outcome, SendOutcome::AlreadyAccepted);
    assert_eq!(later.message.state, MessageState::Dispatching);
}

#[tokio::test]
async fn a_retry_after_the_run_is_sealed_still_finds_its_message() {
    let r = rig();
    r.admit(1, "last words").await;
    r.session(1, "s1").await;
    r.ledger
        .close(RUN, message_ledger::CloseReason::Finalize)
        .await
        .unwrap();
    let again = r
        .ledger
        .admit(RUN, OWNER, &send(1, "last words"))
        .await
        .unwrap();
    assert_eq!(again.outcome, SendOutcome::AlreadyAccepted);
    assert_eq!(again.message.state, MessageState::Rejected);
    // a new id is refused
    assert!(matches!(
        r.refused(&send(2, "new")).await,
        Refusal::RunSealed(_)
    ));
}

#[tokio::test]
async fn the_same_id_with_another_payload_is_a_conflict_and_changes_nothing() {
    let r = rig();
    r.admit(1, "one").await;
    assert_eq!(
        r.refused(&send(1, "two")).await,
        Refusal::IdempotencyConflict
    );
    assert_eq!(
        r.refused(&send_now(1, "one")).await,
        Refusal::IdempotencyConflict
    );
    assert_eq!(r.view(1).await.text, "one");
    assert_eq!(r.ledger.list(RUN).await.unwrap().watermark, 1);
}

#[tokio::test]
async fn another_principal_cannot_find_a_message_by_its_id() {
    let r = rig();
    r.admit(1, "mine").await;
    let e = r
        .ledger
        .admit(RUN, OTHER, &send(1, "mine"))
        .await
        .unwrap_err();
    assert_eq!(
        refusal(e),
        Refusal::NotFound,
        "no idempotency result is disclosed to another principal"
    );
}

#[tokio::test]
async fn at_most_three_messages_are_outstanding_and_a_settled_one_frees_its_slot() {
    let r = rig();
    for n in 1..=MAX_OUTSTANDING_MESSAGES {
        r.admit(n, "m").await;
    }
    assert_eq!(
        r.refused(&send(4, "one too many")).await,
        Refusal::TooManyPending
    );
    assert_eq!(
        r.ledger.list(RUN).await.unwrap().messages.len(),
        3,
        "a refused message stores nothing"
    );
    r.ledger.withdraw(RUN, &mid(2)).await.unwrap();
    let ok = r.admit(4, "now there is room").await;
    assert_eq!(ok.message.seq, 4);
    assert_eq!(
        r.refused(&send(5, "full again")).await,
        Refusal::TooManyPending
    );
}

#[tokio::test]
async fn held_and_dispatching_messages_count_toward_the_cap() {
    let r = rig();
    r.session(1, "s1").await;
    r.admit(1, "a").await;
    r.admit(2, "b").await;
    r.admit(3, "c").await;
    r.ledger
        .claim(RUN, &gen_(1), "s1", Some(&mid(1)), 1, false)
        .await
        .unwrap();
    assert_eq!(r.state(1).await, MessageState::Dispatching);
    assert_eq!(r.refused(&send(4, "d")).await, Refusal::TooManyPending);
    r.session(1, "s2").await;
    assert_eq!(r.state(2).await, MessageState::Held);
    assert_eq!(r.refused(&send(4, "d")).await, Refusal::TooManyPending);
}

#[tokio::test]
async fn a_run_takes_at_most_100_messages() {
    let r = rig();
    for n in 1..=MAX_RUN_MESSAGES {
        // one clock minute per ten messages keeps the rate limit out of the way; each message is withdrawn to free its slot
        r.clock.set(T0 + i64::from(n / 5) * 60);
        r.admit(n, "m").await;
        r.ledger.withdraw(RUN, &mid(n)).await.unwrap();
    }
    r.clock.advance(60);
    let e = r.refused(&send(MAX_RUN_MESSAGES + 1, "101st")).await;
    assert!(
        matches!(e, Refusal::RunLimit(ref m) if m.contains("100")),
        "{e:?}"
    );
    assert_eq!(r.ledger.list(RUN).await.unwrap().watermark, 100);
}

#[tokio::test]
async fn a_run_takes_at_most_4_mib_of_text_superseded_and_withdrawn_messages_included() {
    let r = rig();
    let chunk = "x".repeat(131_000);
    let n_ok = (MAX_RUN_MESSAGE_BYTES / 131_000) as u32; // 32 messages of 131,000 bytes: 4,192,000 bytes
    for n in 1..=n_ok {
        r.clock.set(T0 + i64::from(n / 5) * 60);
        r.admit(n, &chunk).await;
        r.ledger.withdraw(RUN, &mid(n)).await.unwrap();
    }
    // 4,192,000 + 131,000 > 4,194,304 (4 MiB)
    r.clock.advance(120);
    let e = r.refused(&send(n_ok + 1, &chunk)).await;
    assert!(
        matches!(e, Refusal::RunLimit(ref m) if m.contains("4096")),
        "{e:?}"
    );
    // a short one still fits: the cap is on bytes, not on count
    r.admit(n_ok + 2, &"y".repeat(2_000)).await;
}

#[tokio::test]
async fn a_run_takes_ten_admissions_a_minute_and_the_window_moves_with_the_clock() {
    let r = rig();
    for n in 1..=MAX_RUN_ADMISSIONS_PER_MINUTE {
        r.admit(n, "m").await;
        r.ledger.withdraw(RUN, &mid(n)).await.unwrap();
    }
    assert_eq!(
        r.refused(&send(11, "eleventh")).await,
        Refusal::RateLimited("run")
    );
    // still the same minute 59 seconds later
    let minute_start = (T0 / 60) * 60;
    r.clock.set(minute_start + 59);
    assert_eq!(
        r.refused(&send(11, "eleventh")).await,
        Refusal::RateLimited("run")
    );
    r.clock.set(minute_start + 60);
    r.admit(11, "next minute").await;
}

#[tokio::test]
async fn an_owner_is_admitted_thirty_times_a_minute_across_runs() {
    let r = rig();
    for (i, run) in [RUN, RUN2, "01M4ZHFBV5SDX7J92E6YRH4QTY"]
        .into_iter()
        .enumerate()
    {
        for k in 1..=10u32 {
            let n = i as u32 * 10 + k;
            r.ledger.admit(run, OWNER, &send(n, "m")).await.unwrap();
            r.ledger.withdraw(run, &mid(n)).await.unwrap();
        }
    }
    let e = r
        .ledger
        .admit(
            "01M4ZHFBV5SDX7J92E6YRH4QTZ",
            OWNER,
            &send(31, "thirty-first"),
        )
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::RateLimited("owner"));
    // another owner is not held back by this one
    r.ledger
        .admit(
            "01M4ZHFBV5SDX7J92E6YRH4QTZ",
            OTHER,
            &send(32, "other owner"),
        )
        .await
        .unwrap();
    // and the counter lives in a partition of its own
    let rate = r
        .table
        .item(
            &keys::owner_rate_pk(OWNER),
            &keys::owner_rate_sk(T0 as u64 / 60),
        )
        .unwrap();
    assert_eq!(rate["count"], 30);
    assert!(rate["ttl"].as_u64().unwrap() > T0 as u64);
}

#[tokio::test]
async fn a_blank_or_oversized_text_and_a_bad_id_are_refused_before_anything_is_read() {
    let r = rig();
    assert!(matches!(
        r.refused(&send(1, "   ")).await,
        Refusal::BadInput(_)
    ));
    assert!(matches!(
        r.refused(&send(1, &"a".repeat(262_145))).await,
        Refusal::BadInput(_)
    ));
    let mut bad = send(1, "ok");
    bad.message_id = "nope".into();
    assert!(matches!(r.refused(&bad).await, Refusal::BadInput(_)));
    assert_eq!(r.table.stats().gets, 0);
    assert_eq!(r.table.stats().committed, 0);
}

#[tokio::test]
async fn the_mode_is_kept_and_a_now_message_is_pending_like_any_other() {
    let r = rig();
    let m = r
        .ledger
        .admit(RUN, OWNER, &send_now(1, "interrupt"))
        .await
        .unwrap()
        .message;
    assert_eq!((m.mode, m.state), (MessageMode::Now, MessageState::Pending));
}
