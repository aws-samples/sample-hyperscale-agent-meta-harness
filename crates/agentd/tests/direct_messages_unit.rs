//! The messages module, pure part: the states of a message, the server model every test of the module runs against (`MemoryStore`, the
//! in-memory `MessageStore`), the queue agentd holds, and the continuation allowance.
#![allow(clippy::unwrap_used)]

use agentd::direct::messages::{
    Budget, ClaimOutcome, FailReason, MemoryStore, Message, MessageStore, Mode, Notice, Op, Queue,
    Refusal, RejectReason, SealMode, SealOutcome, State, Verdict, WithdrawRefused,
};
use std::sync::Arc;
use std::time::Duration;

fn msg(id: &str, text: &str, epoch: &str) -> Message {
    Message {
        id: id.into(),
        text: text.into(),
        mode: Mode::Next,
        epoch: epoch.into(),
        seq: 0,
    }
}

async fn store_in(epoch: &str) -> Arc<MemoryStore> {
    let s = MemoryStore::new();
    s.report_epoch(epoch, 1).await.unwrap();
    s
}

// ---- accept, hand-out, claim --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_accepted_message_is_pending_and_handed_out_in_server_order() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "first", "s1")).unwrap();
    s.accept(msg("m2", "second", "s1")).unwrap();
    assert_eq!(s.state("m1"), Some(State::Pending));
    let out = s.handout();
    assert_eq!(
        out.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["m1", "m2"]
    );
    // Hand-out only transports: nothing changed, and it can repeat.
    assert_eq!(s.state("m1"), Some(State::Pending));
    assert_eq!(s.handout().len(), 2);
}

#[tokio::test]
async fn a_claim_moves_a_pending_message_to_dispatching_once() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    match s.claim("m1", "s1").await.unwrap() {
        ClaimOutcome::Claimed(m) => assert_eq!((m.id.as_str(), m.text.as_str()), ("m1", "go")),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.state("m1"), Some(State::Dispatching));
    // A claimed message is no longer handed out.
    assert!(s.handout().is_empty());
}

#[tokio::test]
async fn a_repeated_claim_by_the_same_worker_is_the_same_claim_and_another_worker_gets_nothing() {
    // The response of a claim can be lost; the retry must find the claim it made (an idempotent claim), while a fresh microVM, which
    // is another generation, must never take a message the old one may have sent.
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    let old = s.view(1);
    assert!(matches!(
        old.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        old.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    s.worker_lost();
    assert_eq!(s.state("m1"), Some(State::DeliveryUnknown));
    let fresh = s.view(2);
    assert!(matches!(
        fresh.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Refused(Refusal::NotPending(_))
    ));
    // The old generation is told it is stale, not that the message is unknown.
    assert!(matches!(
        old.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Refused(Refusal::StaleGeneration)
    ));
    // A delivery-unknown message is never sent again by itself.
    assert!(s.handout().is_empty());
}

#[tokio::test]
async fn a_claim_for_another_epoch_is_refused() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    assert!(matches!(
        s.claim("m1", "s2").await.unwrap(),
        ClaimOutcome::Refused(Refusal::EpochMoved)
    ));
    assert_eq!(s.state("m1"), Some(State::Pending));
}

#[tokio::test]
async fn an_unknown_message_cannot_be_claimed() {
    let s = store_in("s1").await;
    assert!(matches!(
        s.claim("nope", "s1").await.unwrap(),
        ClaimOutcome::Refused(Refusal::Unknown)
    ));
}

// ---- the race between withdrawal and claim -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_withdrawn_message_cannot_be_claimed() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    s.withdraw("m1").unwrap();
    assert_eq!(s.state("m1"), Some(State::Withdrawn));
    assert!(matches!(
        s.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Refused(Refusal::NotPending(_))
    ));
    assert!(s.handout().is_empty());
}

#[tokio::test]
async fn a_withdrawal_after_the_claim_is_refused_because_the_message_may_already_run() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    assert_eq!(s.withdraw("m1"), Err(WithdrawRefused::AlreadyDispatching));
    assert_eq!(s.state("m1"), Some(State::Dispatching));
    s.submitted("m1", 3).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Delivered { turn: 3 }));
    assert!(matches!(
        s.withdraw("m1"),
        Err(WithdrawRefused::NotWithdrawable(State::Delivered { .. }))
    ));
}

#[tokio::test]
async fn a_held_message_can_be_withdrawn_and_an_unknown_one_cannot() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    s.report_epoch("s2", 2).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Held));
    s.withdraw("m1").unwrap();
    assert_eq!(s.state("m1"), Some(State::Withdrawn));
    assert_eq!(s.withdraw("zz"), Err(WithdrawRefused::NotFound));
}

// ---- submitted, failed, released ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn submitted_sets_delivered_in_turn_k_and_only_for_a_claimed_message() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    // Nothing was claimed: a stray acknowledgement changes nothing.
    assert!(s.submitted("m1", 2).await.is_err());
    assert_eq!(s.state("m1"), Some(State::Pending));
    s.claim("m1", "s1").await.unwrap();
    s.submitted("m1", 2).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Delivered { turn: 2 }));
    // The acknowledgement is idempotent: the same turn again is a no-op, another turn is refused.
    s.submitted("m1", 2).await.unwrap();
    assert!(s.submitted("m1", 5).await.is_err());
}

#[tokio::test]
async fn a_claimed_message_that_was_never_sent_ends_failed() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    s.failed("m1", FailReason::NotSent).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Failed(FailReason::NotSent)));
}

#[tokio::test]
async fn a_released_claim_goes_back_to_pending_and_a_new_session_then_holds_it() {
    // An interrupt the engine did not acknowledge: the message is demoted to the next turn; the engine is replaced, so the
    // session changes and the message waits for the user to confirm it.
    let s = store_in("s1").await;
    s.accept(msg("m1", "go", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    s.release("m1").await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Pending));
    s.report_epoch("s2", 2).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Held));
    // A delivered message cannot be released.
    s.report_epoch("s2", 2).await.unwrap();
    s.accept(msg("m2", "x", "s2")).unwrap();
    s.claim("m2", "s2").await.unwrap();
    s.submitted("m2", 1).await.unwrap();
    assert!(s.release("m2").await.is_err());
}

// ---- the epoch ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_session_change_holds_every_pending_message_of_the_old_session() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.accept(msg("m2", "b", "s1")).unwrap();
    s.report_epoch("s2", 2).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Held));
    assert_eq!(s.state("m2"), Some(State::Held));
    assert!(s.handout().is_empty());
    assert!(matches!(
        s.claim("m1", "s2").await.unwrap(),
        ClaimOutcome::Refused(Refusal::NotPending(_))
    ));
}

#[tokio::test]
async fn reporting_the_same_epoch_again_changes_nothing() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.report_epoch("s1", 1).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::Pending));
}

#[tokio::test]
async fn a_message_written_against_an_old_session_is_held_when_it_arrives() {
    let s = store_in("s2").await;
    s.accept(msg("m1", "late", "s1")).unwrap();
    assert_eq!(s.state("m1"), Some(State::Held));
}

#[tokio::test]
async fn confirming_a_held_message_supersedes_it_and_delivers_a_copy_in_the_new_session() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "text", "s1")).unwrap();
    s.report_epoch("s2", 2).await.unwrap();
    s.confirm("m1", "s2", msg("m1b", "text", "s2")).unwrap();
    assert_eq!(s.state("m1"), Some(State::Superseded));
    assert_eq!(s.state("m1b"), Some(State::Pending));
    // The original never reaches the engine; the copy does.
    assert!(matches!(
        s.claim("m1", "s2").await.unwrap(),
        ClaimOutcome::Refused(_)
    ));
    assert!(matches!(
        s.claim("m1b", "s2").await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn a_confirmation_that_names_a_session_that_has_moved_on_fails_and_sends_nothing() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "text", "s1")).unwrap();
    s.report_epoch("s2", 2).await.unwrap();
    s.report_epoch("s3", 3).await.unwrap();
    assert!(s.confirm("m1", "s2", msg("m1b", "text", "s2")).is_err());
    assert_eq!(s.state("m1"), Some(State::Held));
    assert_eq!(s.state("m1b"), None);
}

#[tokio::test]
async fn only_a_confirmed_copy_reaches_superseded() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.accept(msg("m2", "b", "s1")).unwrap();
    s.report_epoch("s2", 2).await.unwrap();
    s.confirm("m2", "s2", msg("m2b", "b", "s2")).unwrap();
    assert_eq!(s.state("m1"), Some(State::Held));
    assert_eq!(s.state("m2"), Some(State::Superseded));
}

// ---- seal and close ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_seal_with_nothing_pending_closes_admission_and_a_later_message_is_refused_and_not_stored()
 {
    let s = store_in("s1").await;
    match s.seal("s1", SealMode::Normal).await.unwrap() {
        SealOutcome::Sealed { watermark } => assert_eq!(watermark, 0),
        other => panic!("{other:?}"),
    }
    assert!(!s.admission_open());
    assert_eq!(
        s.accept(msg("late", "too late", "s1")),
        Err(RejectReason::RunSealed)
    );
    assert_eq!(s.state("late"), None, "a refused message stores nothing");
}

#[tokio::test]
async fn a_seal_that_finds_a_pending_message_returns_it_and_leaves_admission_open() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.accept(msg("m2", "b", "s1")).unwrap();
    match s.seal("s1", SealMode::Normal).await.unwrap() {
        SealOutcome::Pending(v) => assert_eq!(
            v.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["m1", "m2"]
        ),
        other => panic!("{other:?}"),
    }
    assert!(s.admission_open(), "the run goes on");
    // Nothing was claimed by the seal.
    assert_eq!(s.state("m1"), Some(State::Pending));
}

#[tokio::test]
async fn the_seal_rejects_held_messages_in_the_same_step_without_a_confirmation_wait() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.report_epoch("s2", 2).await.unwrap();
    assert!(matches!(
        s.seal("s2", SealMode::Normal).await.unwrap(),
        SealOutcome::Sealed { .. }
    ));
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::RunSealed))
    );
}

#[tokio::test]
async fn a_seal_that_may_not_continue_rejects_the_pending_messages_for_the_limit() {
    // The continuation allowance is spent: the seal closes the run and says why the messages did not run.
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    assert!(matches!(
        s.seal("s1", SealMode::LimitReached).await.unwrap(),
        SealOutcome::Sealed { .. }
    ));
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::ContinuationLimit))
    );
}

#[tokio::test]
async fn a_terminal_seal_never_rejects_a_claimed_or_delivered_message() {
    let s = store_in("s1").await;
    for (id, text) in [("m1", "a"), ("m2", "b"), ("m3", "c")] {
        s.accept(msg(id, text, "s1")).unwrap();
    }
    s.claim("m1", "s1").await.unwrap();
    s.claim("m2", "s1").await.unwrap();
    s.submitted("m2", 2).await.unwrap();
    s.seal("s1", SealMode::Terminal).await.unwrap();
    // The claimed message was not sent as far as the server knows: its outcome is unknown, and it is never sent again.
    assert_eq!(s.state("m1"), Some(State::DeliveryUnknown));
    assert_eq!(s.state("m2"), Some(State::Delivered { turn: 2 }));
    assert_eq!(
        s.state("m3"),
        Some(State::Rejected(RejectReason::RunSealed))
    );
    assert!(!s.admission_open());
}

#[tokio::test]
async fn a_close_after_the_users_cancel_says_run_cancelled() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.queue_cancel();
    s.seal("s1", SealMode::Terminal).await.unwrap();
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
}

#[tokio::test]
async fn a_cancel_in_the_same_batch_beats_the_claim() {
    // The claim route refuses while a Cancel is queued; the message ends rejected for the cancel.
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.queue_cancel();
    assert!(matches!(
        s.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Refused(Refusal::Cancelled)
    ));
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
}

#[tokio::test]
async fn a_message_rejected_by_agentd_for_the_continuation_limit_is_final() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.reject("m1", RejectReason::ContinuationLimit)
        .await
        .unwrap();
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::ContinuationLimit))
    );
    assert!(matches!(
        s.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Refused(Refusal::NotPending(_))
    ));
}

// ---- faults the tests inject -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_injected_failure_returns_an_error_and_changes_nothing() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.fail(Op::Claim, 1);
    let e = s.claim("m1", "s1").await.unwrap_err();
    assert!(e.retryable, "{e:?}");
    assert_eq!(s.state("m1"), Some(State::Pending));
    assert!(matches!(
        s.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn a_lost_acknowledgement_applies_the_change_and_still_reports_an_error() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.lose_response(Op::Claim, 1);
    assert!(s.claim("m1", "s1").await.is_err());
    assert_eq!(
        s.state("m1"),
        Some(State::Dispatching),
        "the server applied the claim"
    );
    // The retry finds its own claim.
    assert!(matches!(
        s.claim("m1", "s1").await.unwrap(),
        ClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn the_store_keeps_the_history_of_a_message_and_a_log_of_the_calls() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    s.submitted("m1", 2).await.unwrap();
    assert_eq!(
        s.history("m1"),
        [
            State::Accepted,
            State::Pending,
            State::Dispatching,
            State::Delivered { turn: 2 }
        ]
    );
    let calls = s.calls();
    assert!(calls.iter().any(|c| c.starts_with("claim m1")), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("submitted m1 2")));
}

// ---- the safety claims over many interleavings -------------------------------------------------------------------------------

/// A small deterministic generator, so a failing interleaving can be replayed from its seed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

#[tokio::test]
async fn over_many_interleavings_no_message_is_both_withdrawn_and_sent_and_no_end_state_changes() {
    for seed in 0..40u64 {
        let mut r = Lcg(seed + 1);
        let s = store_in("s1").await;
        let mut epoch = "s1".to_string();
        let mut epoch_no = 1;
        let ids: Vec<String> = (0..6).map(|i| format!("m{i}")).collect();
        for id in &ids {
            s.accept(msg(id, "x", &epoch)).unwrap();
        }
        for _ in 0..60 {
            let id = &ids[r.next(6) as usize];
            match r.next(8) {
                0 => {
                    let _ = s.withdraw(id);
                }
                1 => {
                    let _ = s.claim(id, &epoch).await;
                }
                2 => {
                    let _ = s.submitted(id, 1 + r.next(4) as u32).await;
                }
                3 => {
                    let _ = s.release(id).await;
                }
                4 => {
                    epoch_no += 1;
                    epoch = format!("s{epoch_no}");
                    s.report_epoch(&epoch, epoch_no).await.unwrap();
                }
                5 => {
                    let _ = s.failed(id, FailReason::NotSent).await;
                }
                6 => {
                    let _ = s.reject(id, RejectReason::ContinuationLimit).await;
                }
                _ => {
                    let mode = if r.next(2) == 0 {
                        SealMode::Normal
                    } else {
                        SealMode::LimitReached
                    };
                    let _ = s.seal(&epoch, mode).await;
                }
            }
        }
        for id in &ids {
            let h = s.history(id);
            // An end state is final: nothing follows it.
            for w in h.windows(2) {
                assert!(
                    !w[0].is_end(),
                    "seed {seed}: {id} left the end state {:?} for {:?}: {h:?}",
                    w[0],
                    w[1]
                );
                // A claimed message is never withdrawn, rejected or superseded: its claim is given back first (release), or it
                // ends delivered, failed or delivery-unknown. A claim given back after the session moved on is held (protocol 1.38.0:
                // `released` lands held when the epoch changed since the claim, as any message written to an older session does).
                if w[0] == State::Dispatching {
                    assert!(
                        matches!(
                            w[1],
                            State::Pending
                                | State::Held
                                | State::Delivered { .. }
                                | State::Failed(_)
                                | State::DeliveryUnknown
                        ),
                        "seed {seed}: {id} was taken from dispatching to {:?}: {h:?}",
                        w[1]
                    );
                }
                // Only a pending or held message can be withdrawn or rejected.
                if matches!(
                    w[1],
                    State::Withdrawn | State::Rejected(_) | State::Superseded
                ) {
                    assert!(
                        matches!(w[0], State::Pending | State::Held),
                        "seed {seed}: {id} went from {:?} to {:?}: {h:?}",
                        w[0],
                        w[1]
                    );
                }
            }
            // A message that reached the engine (delivered, or sent with the outcome unknown) never shows rejected or withdrawn.
            let reached = h
                .iter()
                .any(|x| matches!(x, State::Delivered { .. } | State::DeliveryUnknown));
            let denied = h
                .iter()
                .any(|x| matches!(x, State::Withdrawn | State::Rejected(_) | State::Superseded));
            assert!(!(reached && denied), "seed {seed}: {id}: {h:?}");
        }
    }
}

// ---- the queue agentd holds --------------------------------------------------------------------------------------------------

fn note(id: &str, epoch: &str) -> Notice {
    Notice {
        id: id.into(),
        seq: 0,
        mode: Mode::Next,
        epoch: epoch.into(),
        bytes: 5,
    }
}

fn now(id: &str, epoch: &str) -> Notice {
    Notice {
        mode: Mode::Now,
        ..note(id, epoch)
    }
}

#[test]
fn the_queue_delivers_now_before_next_and_keeps_arrival_order_within_a_mode() {
    let mut q = Queue::new(16);
    q.offer(vec![note("a", "s1"), note("b", "s1")]);
    q.offer(vec![now("n1", "s1"), note("c", "s1"), now("n2", "s1")]);
    let order: Vec<String> = std::iter::from_fn(|| q.take_next()).map(|m| m.id).collect();
    assert_eq!(order, ["n1", "n2", "a", "b", "c"]);
}

#[test]
fn the_queue_uses_the_server_sequence_when_the_server_gave_one() {
    let mut q = Queue::new(16);
    let mut late = note("late", "s1");
    late.seq = 9;
    let mut early = note("early", "s1");
    early.seq = 2;
    q.offer(vec![late, early]);
    assert_eq!(q.take_next().unwrap().id, "early");
}

#[test]
fn a_repeated_hand_out_is_processed_once_even_after_the_message_left_the_queue() {
    let mut q = Queue::new(16);
    let first = q.offer(vec![note("a", "s1")]);
    assert_eq!((first.added, first.duplicate), (1, 0));
    assert_eq!(q.take_next().unwrap().id, "a");
    // The server hands it out again (it has not seen the claim yet): agentd does not process it a second time.
    let again = q.offer(vec![note("a", "s1")]);
    assert_eq!((again.added, again.duplicate), (0, 1));
    assert!(q.take_next().is_none());
}

#[test]
fn the_queue_is_bounded_and_counts_what_it_did_not_take() {
    let mut q = Queue::new(2);
    let r = q.offer(vec![note("a", "s1"), note("b", "s1"), note("c", "s1")]);
    assert_eq!((r.added, r.overflow), (2, 1));
    // The third stays eligible on the server and arrives again; once there is room it is taken.
    q.take_next();
    let r = q.offer(vec![note("c", "s1")]);
    assert_eq!(r.added, 1);
}

#[test]
fn a_session_change_drops_the_messages_written_against_the_old_session() {
    let mut q = Queue::new(16);
    q.offer(vec![note("a", "s1"), note("b", "s2")]);
    let dropped = q.retain_epoch("s2");
    assert_eq!(
        dropped.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(q.take_next().unwrap().id, "b");
    assert!(q.take_next().is_none());
}

#[test]
fn removing_a_message_by_id_takes_it_out_of_the_queue() {
    let mut q = Queue::new(16);
    q.offer(vec![note("a", "s1"), note("b", "s1")]);
    assert!(q.remove("a"));
    assert!(!q.remove("a"));
    assert_eq!(q.len(), 1);
    assert!(q.has_next_mode());
    assert!(!q.has_now());
}

// ---- the continuation allowance ----------------------------------------------------------------------------------------------

#[test]
fn nothing_is_limited_before_the_first_done_looking_stop() {
    let mut b = Budget::standard();
    assert_eq!(b.verdict(Duration::from_secs(10_000)), Verdict::Allowed);
    // A message delivered while Cline is still working does not count.
    b.note_stop(false);
    assert!(!b.turn_started());
    assert_eq!(b.turns(), 0);
    assert!(!b.started());
}

#[test]
fn a_stage_count_starts_at_the_first_done_looking_stop_and_allows_eight_turns() {
    let mut b = Budget::new(Some(8), None, Duration::from_secs(300));
    b.note_stop(true);
    assert!(b.started());
    for k in 1..=8 {
        assert_eq!(
            b.verdict(Duration::from_secs(10_000)),
            Verdict::Allowed,
            "turn {k}"
        );
        assert!(b.turn_started());
        b.turn_ended(Duration::from_secs(10));
    }
    assert_eq!(b.turns(), 8);
    assert_eq!(
        b.verdict(Duration::from_secs(10_000)),
        Verdict::ContinuationLimit
    );
}

#[test]
fn another_done_looking_stop_does_not_reset_the_allowance() {
    let mut b = Budget::standard();
    b.note_stop(true);
    for _ in 0..3 {
        b.turn_started();
        b.turn_ended(Duration::from_secs(1));
    }
    b.note_stop(true);
    b.note_stop(false);
    assert_eq!(b.turns(), 3);
}

#[test]
fn a_stage_limit_of_eighteen_hundred_seconds_of_extra_work_ends_the_allowance() {
    let mut b = Budget::new(
        None,
        Some(Duration::from_secs(1_800)),
        Duration::from_secs(300),
    );
    b.note_stop(true);
    b.turn_started();
    b.turn_ended(Duration::from_secs(1_799));
    assert_eq!(b.verdict(Duration::from_secs(10_000)), Verdict::Allowed);
    b.turn_started();
    b.turn_ended(Duration::from_secs(1));
    assert_eq!(b.exec(), Duration::from_secs(1_800));
    assert_eq!(
        b.verdict(Duration::from_secs(10_000)),
        Verdict::ContinuationLimit
    );
}

#[test]
fn too_little_time_left_is_too_late_even_inside_the_allowance() {
    // The deadline the driver gives already holds the 900 s reserve for delivery (MAX_DIRECT_TIMEOUT_S); 300 s more must remain.
    let b = Budget::standard();
    assert_eq!(b.verdict(Duration::from_secs(299)), Verdict::TooLate);
    assert_eq!(b.verdict(Duration::from_secs(300)), Verdict::Allowed);
}

#[test]
fn the_limits_can_be_set_for_a_test() {
    let mut b = Budget::new(
        Some(2),
        Some(Duration::from_secs(60)),
        Duration::from_secs(0),
    );
    b.note_stop(true);
    b.turn_started();
    b.turn_started();
    assert_eq!(
        b.verdict(Duration::from_secs(1)),
        Verdict::ContinuationLimit
    );
}

#[test]
fn a_turn_that_did_not_start_inside_the_allowance_adds_no_time() {
    let mut b = Budget::standard();
    b.turn_ended(Duration::from_secs(500));
    assert_eq!(b.exec(), Duration::ZERO);
}

// ---- what protocol 1.38.0 adds: sealing while a message is dispatching, and giving a claim back after a cancel --------------------------

#[tokio::test]
async fn a_normal_or_limit_seal_is_refused_while_a_message_is_dispatching_and_a_terminal_seal_is_not()
 {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    for mode in [SealMode::Normal, SealMode::LimitReached] {
        let e = s.seal("s1", mode).await.unwrap_err();
        assert!(!e.retryable && e.detail.contains("bad-transition"), "{e:?}");
        assert!(
            e.detail.contains("m1"),
            "the refusal names the message: {e:?}"
        );
        assert!(s.admission_open(), "a refused seal closes nothing");
        assert_eq!(s.state("m1"), Some(State::Dispatching));
    }
    s.seal("s1", SealMode::Terminal).await.unwrap();
    assert_eq!(s.state("m1"), Some(State::DeliveryUnknown));
}

#[tokio::test]
async fn a_claim_given_back_after_the_users_cancel_ends_rejected_for_it() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    s.queue_cancel();
    s.release("m1").await.unwrap();
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
    // and a repeated release of the same claim is harmless
    s.release("m1").await.unwrap();
    assert_eq!(
        s.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
}

#[tokio::test]
async fn a_claim_without_an_id_returns_what_this_generation_dispatches_also_after_a_session_change()
{
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    s.accept(msg("m2", "b", "s1")).unwrap();
    s.claim("m1", "s1").await.unwrap();
    let held = s.held("s1").await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].message.id, "m1");
    assert!(held[0].redelivered && held[0].current_session);
    assert_eq!(
        s.state("m2"),
        Some(State::Pending),
        "a claim that finds held messages claims nothing new"
    );
    s.report_epoch("s2", 2).await.unwrap();
    let held = s.held("s2").await.unwrap();
    assert_eq!(held.len(), 1);
    assert!(held[0].redelivered && !held[0].current_session, "{held:?}");
    // a fresh worker holds nothing of the old one's
    s.worker_lost();
    assert!(s.view(2).held("s2").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_claim_without_an_id_that_finds_nothing_held_claims_the_next_pending_message() {
    let s = store_in("s1").await;
    s.accept(msg("m1", "a", "s1")).unwrap();
    let held = s.held("s1").await.unwrap();
    assert_eq!(held.len(), 1);
    assert!(!held[0].redelivered, "a fresh claim is not a redelivery");
    assert_eq!(s.state("m1"), Some(State::Dispatching));
}

#[tokio::test]
async fn the_view_of_a_replaced_worker_generation_is_unavailable() {
    let s = store_in("s1").await;
    let old = s.view(1);
    assert!(!old.unavailable());
    s.worker_lost();
    assert!(old.unavailable(), "a replaced worker is fenced for good");
    assert!(!s.view(2).unavailable());
}
