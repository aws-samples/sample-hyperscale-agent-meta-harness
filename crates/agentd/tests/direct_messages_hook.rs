//! The messages module as a hook, without a driver: the module is called the way the seam calls it
//! (events, controls, the vote at a turn end, the seal, the shutdown) against the server model (`MemoryStore`), and the tests pin
//! what it asks the store and what it votes. The whole run, with the fake engine, is `direct_messages_e2e.rs`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpError, Watched};
use agentd::direct::classify::{Classified, PromptDone, RawEnd, classify};
use agentd::direct::hooks::{
    DirectHook, DriverCtl, Event, LadderEvent, PromptReplied, PromptSent, SessionStarted, TurnEnd,
    TurnVote,
};
use agentd::direct::messages::{
    FailReason, MemoryStore, Message, MessagesHook, Mode, Op, RejectReason, Settings, State, Wiring,
};
use agentd::direct::retry::RESUME_PROMPT_INTERRUPTED;
use agentd::logging::MemoryLog;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::common::Usage;
use harness_protocol::direct::{DirectSwitch, DirectSwitchKind, InstructAction, InstructOrigin};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct Rig {
    hook: Arc<MessagesHook>,
    store: Arc<MemoryStore>,
    log: Arc<MemoryLog>,
    cancel: watch::Sender<bool>,
}

impl Rig {
    async fn new() -> Self {
        Self::with(Settings::for_tests()).await
    }

    async fn with(settings: Settings) -> Self {
        let store = MemoryStore::new();
        let hook = MessagesHook::new(store.clone(), settings);
        let log = MemoryLog::new();
        let (cancel, rx) = watch::channel(false);
        hook.bind(Wiring::new(DriverCtl::new(), rx, log.clone()));
        let r = Self {
            hook,
            store,
            log,
            cancel,
        };
        r.session("s1", 1);
        r.hook.flush().await;
        r
    }

    /// As [`Rig::new`], with a log of the engine's own runs at `rounds`: the file the engine's observer writes a start line to for each
    /// run the engine begins.
    async fn with_rounds(rounds: &std::path::Path) -> Self {
        let store = MemoryStore::new();
        let hook = MessagesHook::new(store.clone(), Settings::for_tests());
        let log = MemoryLog::new();
        let (cancel, rx) = watch::channel(false);
        hook.bind(Wiring::new(DriverCtl::new(), rx, log.clone()).with_rounds(rounds.to_path_buf()));
        let r = Self {
            hook,
            store,
            log,
            cancel,
        };
        r.session("s1", 1);
        r.hook.flush().await;
        r
    }

    /// The driver starts a session: the module learns the epoch.
    fn session(&self, id: &str, number: u32) {
        self.hook.on_event(&Event::SessionStarted(SessionStarted {
            session_id: id,
            number,
            response: &json!({}),
            cwd: "/w",
            trigger: None,
        }));
    }

    /// The control plane accepts a message; `hand_out` is what the next events response carries.
    fn accept(&self, id: &str, text: &str, epoch: &str) {
        self.store
            .accept(Message {
                id: id.into(),
                text: text.into(),
                mode: Mode::Next,
                epoch: epoch.into(),
                seq: 0,
            })
            .unwrap();
    }

    fn accept_now(&self, id: &str, text: &str, epoch: &str) {
        self.store
            .accept(Message {
                id: id.into(),
                text: text.into(),
                mode: Mode::Now,
                epoch: epoch.into(),
                seq: 0,
            })
            .unwrap();
    }

    fn hand_out(&self) {
        self.hook.offer(self.store.handout());
    }

    /// The driver sent a prompt, got updates and an answer.
    fn prompt(&self, no: u32, session: &str, text: &str, stop: &str) {
        self.hook.on_event(&Event::PromptSent(PromptSent {
            no,
            session_id: session,
            text,
        }));
        self.hook.on_wire(&agentd::direct::hooks::Wire {
            seq: u64::from(no),
            dir: agentd::acp::Dir::FromEngine,
            msg: &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk"}}}),
            stale: false,
        });
        self.hook.on_event(&Event::PromptReplied(PromptReplied {
            no,
            session_id: session,
            stop_reason: Some(stop),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            }),
            error: None,
        }));
    }

    async fn turn_end(&self, left: u64) -> TurnVote {
        let raw = done_raw("end_turn");
        let classified = classify(&raw);
        self.hook
            .on_turn_end(&end(&raw, &classified, left, false))
            .await
    }

    async fn seal(&self, left: u64) -> TurnVote {
        let raw = done_raw("end_turn");
        let classified = classify(&raw);
        self.hook
            .on_seal(&end(&raw, &classified, left, false))
            .await
    }
}

fn done_raw(stop: &str) -> RawEnd {
    RawEnd {
        watched: Watched::Done(Ok(PromptDone {
            stop_reason: stop.into(),
            usage: None,
        })),
        cancel_sent: false,
        message: "I made the change.".into(),
        setup_error: None,
    }
}

fn end<'a>(
    raw: &'a RawEnd,
    classified: &'a Classified,
    left: u64,
    interrupted: bool,
) -> TurnEnd<'a> {
    TurnEnd {
        n: 1,
        session_id: Some("s1"),
        session: 1,
        raw,
        classified,
        files_changed: Some(1),
        interrupted,
        engine_working: false,
        time_left: Duration::from_secs(left),
        tool_calls: 2,
    }
}

fn prompt_text(v: &TurnVote) -> &str {
    match v {
        TurnVote::Prompt(t) => t,
        other => panic!("{other:?}"),
    }
}

// ---- the happy path -------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pending_message_at_the_turn_end_is_claimed_and_voted_as_the_next_prompt() {
    let r = Rig::new().await;
    r.accept("m1", "please also add a test", "s1");
    r.hand_out();
    let vote = r.turn_end(10_000).await;
    assert_eq!(prompt_text(&vote), "please also add a test");
    // Claimed before it was sent, not yet delivered: the prompt has not gone out.
    assert_eq!(r.store.state("m1"), Some(State::Dispatching));
    // The driver sends it: the module acknowledges, and the message is delivered in that turn.
    r.prompt(2, "s1", "please also add a test", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
}

#[tokio::test]
async fn nothing_pending_votes_done_and_the_seal_closes_the_run() {
    let r = Rig::new().await;
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert!(!r.store.admission_open());
    // The run is sealed: a message now is refused and stores nothing.
    assert_eq!(
        r.store.accept(Message {
            id: "late".into(),
            text: "x".into(),
            mode: Mode::Next,
            epoch: "s1".into(),
            seq: 0
        }),
        Err(RejectReason::RunSealed)
    );
}

#[tokio::test]
async fn a_message_the_hand_out_has_not_shown_yet_is_found_by_the_seal_and_sent() {
    // The completion race: the message was accepted a moment before the end and has not been handed out (the hand-out takes up to 2 s).
    let r = Rig::new().await;
    r.accept("m1", "last-moment correction", "s1");
    assert_eq!(
        r.turn_end(10_000).await,
        TurnVote::Done,
        "nothing was handed out"
    );
    let vote = r.seal(10_000).await;
    assert_eq!(prompt_text(&vote), "last-moment correction");
    r.prompt(2, "s1", "last-moment correction", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    // After that turn the seal finds nothing and closes.
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert!(!r.store.admission_open());
    let calls = r.store.calls();
    let order: Vec<&str> = calls
        .iter()
        .filter(|c| c.starts_with("seal") || c.starts_with("claim") || c.starts_with("submitted"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        order,
        [
            "seal s1 normal",
            "claim m1 s1",
            "submitted m1 2",
            "seal s1 normal"
        ]
    );
}

#[tokio::test]
async fn two_messages_are_two_turns_in_order_never_one_prompt() {
    let r = Rig::new().await;
    r.accept("m1", "first", "s1");
    r.accept("m2", "second", "s1");
    r.hand_out();
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "first");
    r.prompt(2, "s1", "first", "end_turn");
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "second");
    r.prompt(3, "s1", "second", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(r.store.state("m2"), Some(State::Delivered { turn: 3 }));
}

#[tokio::test]
async fn a_now_message_goes_before_a_next_message_that_arrived_earlier() {
    let r = Rig::new().await;
    r.accept("m1", "older next", "s1");
    r.accept_now("m2", "newer now", "s1");
    r.hand_out();
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "newer now");
    r.prompt(2, "s1", "newer now", "end_turn");
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "older next");
}

#[tokio::test]
async fn a_repeated_hand_out_of_the_same_message_is_sent_once() {
    let r = Rig::new().await;
    r.accept("m1", "once", "s1");
    r.hand_out();
    r.hand_out();
    r.hand_out();
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "once");
    r.prompt(2, "s1", "once", "end_turn");
    // The server has not seen the submission yet and hands it out again: nothing more is sent.
    r.hand_out();
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    let claims = r
        .store
        .calls()
        .iter()
        .filter(|c| c.starts_with("claim"))
        .count();
    assert_eq!(claims, 1);
}

// ---- withdrawal and the claim ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_withdrawn_message_is_never_sent() {
    let r = Rig::new().await;
    r.accept("m1", "never mind", "s1");
    r.hand_out();
    r.store.withdraw("m1").unwrap();
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.store.state("m1"), Some(State::Withdrawn));
}

#[tokio::test]
async fn a_withdrawal_that_arrives_during_the_claim_wins_when_it_commits_first() {
    let r = Rig::new().await;
    r.accept("m1", "race", "s1");
    r.hand_out();
    // The claim waits before it is applied; the withdrawal commits in that window.
    r.store.delay(Op::Claim, Duration::from_millis(200));
    let store = r.store.clone();
    let t = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.withdraw("m1")
    });
    let vote = r.turn_end(10_000).await;
    assert!(t.await.unwrap().is_ok(), "the withdrawal committed first");
    assert_eq!(vote, TurnVote::Done, "the claim lost: nothing is sent");
    assert_eq!(r.store.state("m1"), Some(State::Withdrawn));
}

#[tokio::test]
async fn a_withdrawal_that_arrives_after_the_claim_is_refused_and_the_message_runs() {
    let r = Rig::new().await;
    r.accept("m1", "race", "s1");
    r.hand_out();
    r.store.delay_after(Op::Claim, Duration::from_millis(200));
    let store = r.store.clone();
    let t = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.withdraw("m1")
    });
    let vote = r.turn_end(10_000).await;
    assert!(t.await.unwrap().is_err(), "the claim had already won");
    assert_eq!(prompt_text(&vote), "race");
    r.prompt(2, "s1", "race", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
}

#[tokio::test]
async fn a_lost_claim_response_is_repeated_and_finds_the_same_claim() {
    let r = Rig::new().await;
    r.accept("m1", "once only", "s1");
    r.hand_out();
    r.store.lose_response(Op::Claim, 1);
    let vote = r.turn_end(10_000).await;
    assert_eq!(prompt_text(&vote), "once only");
    r.prompt(2, "s1", "once only", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
}

#[tokio::test]
async fn a_claim_the_store_cannot_answer_leaves_the_message_eligible_and_the_run_unharmed() {
    let r = Rig::new().await;
    r.accept("m1", "wait for the store", "s1");
    r.hand_out();
    r.store.fail(Op::Claim, 9);
    assert_eq!(
        r.turn_end(10_000).await,
        TurnVote::Done,
        "a store that is down never holds the run"
    );
    assert_eq!(
        r.store.state("m1"),
        Some(State::Pending),
        "nothing was applied"
    );
    // The store comes back: the seal asks again and the message is still there.
    r.store.fail(Op::Claim, 0);
    r.store.fail(Op::Seal, 0);
    let vote = r.seal(10_000).await;
    assert_eq!(prompt_text(&vote), "wait for the store");
}

#[tokio::test]
async fn a_seal_the_store_cannot_answer_ends_the_run_and_the_terminal_close_settles_it() {
    let r = Rig::new().await;
    r.accept("m1", "x", "s1");
    // Three failures are all the module tries: the seal gives up and the run goes on to its end.
    r.store.fail(Op::Seal, 3);
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    // The shutdown asks again, and the pending message ends rejected for the sealed run.
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Rejected(RejectReason::RunSealed))
    );
}

// ---- the session (epoch) --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_new_session_holds_what_was_written_for_the_old_one_and_a_confirmed_copy_is_delivered() {
    let r = Rig::new().await;
    r.accept("m1", "for session one", "s1");
    r.hand_out();
    r.session("s2", 2);
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Held));
    assert_eq!(
        r.turn_end(10_000).await,
        TurnVote::Done,
        "the held message is not sent"
    );
    // The user confirms: the copy is written against the new session and delivered; the original is superseded.
    r.store
        .confirm(
            "m1",
            "s2",
            Message {
                id: "m1b".into(),
                text: "for session one".into(),
                mode: Mode::Next,
                epoch: "s2".into(),
                seq: 0,
            },
        )
        .unwrap();
    r.hand_out();
    let raw = done_raw("end_turn");
    let classified = classify(&raw);
    let e = TurnEnd {
        session_id: Some("s2"),
        session: 2,
        ..end(&raw, &classified, 10_000, false)
    };
    let vote = r.hook.on_turn_end(&e).await;
    assert_eq!(prompt_text(&vote), "for session one");
    r.prompt(5, "s2", "for session one", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Superseded));
    assert_eq!(r.store.state("m1b"), Some(State::Delivered { turn: 5 }));
}

#[tokio::test]
async fn the_same_session_reported_twice_changes_nothing_and_a_model_change_is_not_a_new_session() {
    // A model change raises no session event: the epoch is the session id, so a message addressed to the task stays pending.
    let r = Rig::new().await;
    r.accept("m1", "keep", "s1");
    r.hand_out();
    r.session("s1", 1);
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Pending));
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "keep");
}

// ---- the continuation allowance --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn after_the_allowance_is_spent_a_further_message_is_rejected_and_the_run_is_sealed() {
    let mut s = Settings::for_tests();
    s.max_turns = Some(2);
    let r = Rig::with(s).await;
    for (i, id) in ["m1", "m2", "m3"].iter().enumerate() {
        r.accept(id, &format!("message {i}"), "s1");
    }
    r.hand_out();
    // The first stop looks like done (end_turn): the allowance starts. Two message turns are allowed.
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "message 0");
    r.prompt(2, "s1", "message 0", "end_turn");
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "message 1");
    r.prompt(3, "s1", "message 1", "end_turn");
    // The third is over the limit: not sent, rejected for it.
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    r.hook.flush().await;
    assert_eq!(
        r.store.state("m3"),
        Some(State::Rejected(RejectReason::ContinuationLimit))
    );
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert!(!r.store.admission_open());
    let prompts = r
        .store
        .calls()
        .iter()
        .filter(|c| c.starts_with("submitted"))
        .count();
    assert_eq!(prompts, 2);
}

#[tokio::test]
async fn too_little_time_left_seals_the_run_and_rejects_what_is_pending_as_sealed() {
    let r = Rig::new().await;
    r.accept("m1", "no time", "s1");
    r.hand_out();
    // Less than 300 s remain: no further turn starts.
    assert_eq!(r.turn_end(299).await, TurnVote::Done);
    assert_eq!(r.seal(299).await, TurnVote::Done);
    assert_eq!(
        r.store.state("m1"),
        Some(State::Rejected(RejectReason::RunSealed))
    );
    assert!(
        r.store.calls().iter().any(|c| c == "seal s1 terminal"),
        "{:?}",
        r.store.calls()
    );
}

#[tokio::test]
async fn a_message_delivered_before_the_first_done_looking_stop_does_not_count() {
    let mut s = Settings::for_tests();
    s.max_turns = Some(1);
    let r = Rig::with(s).await;
    r.accept("m1", "mid-work", "s1");
    r.accept("m2", "after done", "s1");
    r.hand_out();
    // The stop ended for a token limit: it does not look like done, so the first message does not use the allowance.
    let raw = done_raw("max_tokens");
    let classified = classify(&raw);
    let vote = r
        .hook
        .on_turn_end(&end(&raw, &classified, 10_000, false))
        .await;
    assert_eq!(prompt_text(&vote), "mid-work");
    r.prompt(2, "s1", "mid-work", "end_turn");
    // Now Cline stopped for good: the allowance starts and allows its one turn.
    assert_eq!(prompt_text(&r.turn_end(10_000).await), "after done");
}

// ---- the user's cancel -----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_users_cancel_is_final_nothing_is_claimed_and_the_close_says_cancelled() {
    let r = Rig::new().await;
    r.accept("m1", "too late", "s1");
    r.hand_out();
    r.cancel.send_replace(true);
    r.store.queue_cancel();
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert!(
        r.store.calls().iter().all(|c| !c.starts_with("claim")),
        "{:?}",
        r.store.calls()
    );
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Rejected(RejectReason::RunCancelled))
    );
}

#[tokio::test]
async fn a_batch_with_a_cancel_queues_none_of_its_messages() {
    let r = Rig::new().await;
    r.accept("m1", "same batch", "s1");
    let queued = r.hook.offer_batch(true, r.store.handout());
    assert_eq!(queued, 0, "a Cancel in the batch beats the messages in it");
}

// ---- the end ----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claimed_message_that_never_went_out_ends_failed_when_the_run_ends() {
    let r = Rig::new().await;
    r.accept("m1", "claimed, never sent", "s1");
    r.hand_out();
    let vote = r.turn_end(10_000).await;
    assert!(matches!(vote, TurnVote::Prompt(_)));
    // The driver dropped the vote (the deadline passed while the claim was out): no PromptSent follows.
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::NotSent))
    );
    // The close is idempotent: a second one changes nothing.
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::NotSent))
    );
}

#[tokio::test]
async fn a_delivered_message_is_not_touched_by_the_close() {
    let r = Rig::new().await;
    r.accept("m1", "sent", "s1");
    r.hand_out();
    r.turn_end(10_000).await;
    r.prompt(2, "s1", "sent", "end_turn");
    r.hook.close().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
}

#[tokio::test]
async fn the_acknowledgement_of_a_send_survives_a_lost_response() {
    let r = Rig::new().await;
    r.accept("m1", "ack", "s1");
    r.hand_out();
    r.turn_end(10_000).await;
    r.store.lose_response(Op::Submitted, 1);
    r.prompt(2, "s1", "ack", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
}

#[tokio::test]
async fn a_retry_of_the_same_prompt_by_the_ladder_is_one_delivery_in_one_turn() {
    // The retry ladder sends the same text again after a provider error: the message is delivered once, in the turn of the first send.
    let r = Rig::new().await;
    r.accept("m1", "retry me", "s1");
    r.hand_out();
    r.turn_end(10_000).await;
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no: 2,
        session_id: "s1",
        text: "retry me",
    }));
    let err = AcpError::Rpc {
        code: -32603,
        message: "Service unavailable".into(),
        data: None,
    };
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no: 2,
        session_id: "s1",
        stop_reason: None,
        usage: None,
        error: Some(&err),
    }));
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no: 3,
        session_id: "s1",
        text: "retry me",
    }));
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    let acks = r
        .store
        .calls()
        .iter()
        .filter(|c| c.starts_with("submitted"))
        .count();
    assert_eq!(acks, 1);
}

#[tokio::test]
async fn text_longer_than_the_limit_is_not_sent() {
    let r = Rig::new().await;
    r.accept("m1", &"x".repeat(262_145), "s1");
    r.hand_out();
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    r.hook.flush().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::TooLarge))
    );
}

// ---- the tripwire -----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_end_turn_reply_with_no_usage_and_no_update_is_a_tripwire_and_never_done() {
    let r = Rig::new().await;
    // The engine queued the prompt instead of running it: a fast end_turn, usage 0, no update since the prompt.
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no: 1,
        session_id: "s1",
        text: "the task",
    }));
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no: 1,
        session_id: "s1",
        stop_reason: Some("end_turn"),
        usage: Some(Usage::default()),
        error: None,
    }));
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        vote,
        TurnVote::Prompt(RESUME_PROMPT_INTERRUPTED.to_string()),
        "the engine is reset, never reported as done"
    );
    assert_eq!(r.hook.stats().tripwires, 1);
    assert!(
        r.log.lines().iter().any(|l| l.contains("tripwire")),
        "{:?}",
        r.log.lines()
    );
}

#[tokio::test]
async fn a_reply_with_updates_or_with_usage_is_not_a_tripwire() {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    // usage 0 but an update arrived: the engine did something
    let r = Rig::new().await;
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no: 1,
        session_id: "s1",
        text: "t",
    }));
    r.hook.on_wire(&agentd::direct::hooks::Wire {
        seq: 1,
        dir: agentd::acp::Dir::FromEngine,
        msg: &json!({"method": "session/update", "params": {"sessionId": "s1"}}),
        stale: false,
    });
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no: 1,
        session_id: "s1",
        stop_reason: Some("end_turn"),
        usage: Some(Usage::default()),
        error: None,
    }));
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.hook.stats().tripwires, 0);
}

// ---- seal modes on the wire of the store ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_modes_of_the_seal_follow_the_budget_and_the_time() {
    let r = Rig::new().await;
    r.seal(10_000).await;
    assert!(r.store.calls().iter().any(|c| c == "seal s1 normal"));
    let mut s = Settings::for_tests();
    s.max_turns = Some(0);
    let r = Rig::with(s).await;
    r.prompt(1, "s1", "x", "end_turn");
    r.turn_end(10_000).await;
    r.seal(10_000).await;
    assert!(
        r.store.calls().iter().any(|c| c == "seal s1 limit-reached"),
        "{:?}",
        r.store.calls()
    );
}

// ---- the continuation allowance is not reset -------------------------------------------------------------------------

#[tokio::test]
async fn a_model_change_an_answer_and_a_new_session_leave_the_allowance_where_it_was() {
    // An allowance a stage set (here 8 message turns) starts at the first done-looking stop. `direct_messages_unit.rs` shows that
    // another done-looking stop does not reset it; this shows the same for the three things that change the run under it. The proof that
    // nothing reset it is the limit: three turns were used, so five are left and the ninth message is refused.
    let mut limits = Settings::for_tests();
    limits.max_turns = Some(8);
    let r = Rig::with(limits).await;
    for k in 1..=3u32 {
        let (id, text) = (format!("m{k}"), format!("message {k}"));
        r.accept(&id, &text, "s1");
        r.hand_out();
        assert_eq!(prompt_text(&r.turn_end(10_000).await), text);
        r.prompt(k + 1, "s1", &text, "end_turn");
        r.hook.flush().await;
    }
    let before = r.hook.allowance();
    assert_eq!(before.0, 3, "TRIGGER PRESENT: three message turns are used");
    // 1. A model change: the driver switched the route of the session (no new session starts; the epoch stays).
    let switch = DirectSwitch {
        n: 1,
        at: "2026-10-17T00:00:00Z".into(),
        from: "us.anthropic.claude-sonnet-5-5".into(),
        to: "us.anthropic.claude-opus-5-5".into(),
        kind: DirectSwitchKind::Model,
        class: "transient".into(),
        detail: None,
    };
    r.hook
        .on_event(&Event::Ladder(LadderEvent::Switch(&switch)));
    assert_eq!(r.hook.allowance(), before, "a model change");
    // 2. An answer: the user answers a question of the run and the driver sends the answer as a prompt of its own (not a message).
    r.hook.on_controls(&[PendingControl::Instruct {
        request_id: "01M5A0000000000000000000RQ".into(),
        action: InstructAction::ContinueWithPrompt,
        text: Some("use the second option".into()),
        model: None,
        origin: InstructOrigin::User,
    }]);
    r.prompt(5, "s1", "use the second option", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.hook.allowance(), before, "an answer");
    // 3. A new session: the engine was replaced and the epoch moves on.
    r.session("s2", 2);
    r.hook.flush().await;
    assert_eq!(r.hook.allowance(), before, "a new session");
    // Five turns are left, in the new session; the ninth message of the allowance is refused for the limit.
    for k in 4..=8u32 {
        let (id, text) = (format!("m{k}"), format!("message {k}"));
        r.accept(&id, &text, "s2");
        r.hand_out();
        assert_eq!(
            prompt_text(&r.turn_end(10_000).await),
            text,
            "turn {k} of 8 is still allowed"
        );
        r.prompt(k + 2, "s2", &text, "end_turn");
        r.hook.flush().await;
    }
    assert_eq!(r.hook.allowance().0, 8);
    r.accept("m9", "one too many", "s2");
    r.hand_out();
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(
        r.store.state("m9"),
        Some(State::Rejected(RejectReason::ContinuationLimit)),
        "{:?}",
        r.store.history("m9")
    );
}

// ---- a now message beyond the allowance ----------------------------------------------------------------------------

/// Waits (bounded) until the interrupt flow, a task the offer starts, has settled the message.
async fn settled(r: &Rig, id: &str) -> Option<State> {
    for _ in 0..100 {
        match r.store.state(id) {
            Some(State::Pending | State::Accepted) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            other => return other,
        }
    }
    r.store.state(id)
}

#[tokio::test]
async fn a_now_message_beyond_the_allowance_is_refused_at_once_and_interrupts_nothing() {
    // The allowance is spent (no turn is allowed and the first done-looking stop has started it). A `now` message that comes while a prompt
    // is open would cut that prompt and then be refused for the limit; it is refused at once, without a claim, and the prompt runs on.
    let mut s = Settings::for_tests();
    s.max_turns = Some(0);
    let r = Rig::with(s).await;
    r.prompt(1, "s1", "the task", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    r.hook.pretend_prompt_open(true);
    r.accept_now("m1", "redirect", "s1");
    r.hand_out();
    assert_eq!(
        settled(&r, "m1").await,
        Some(State::Rejected(RejectReason::ContinuationLimit)),
        "{:?} {:?}",
        r.store.history("m1"),
        r.log.lines()
    );
    r.hook.flush().await;
    let calls = r.store.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("claim")),
        "a message that will be refused is not claimed: {calls:?}"
    );
    assert_eq!(r.hook.stats().interrupts, 0);
    assert_eq!(r.hook.stats().claims, 0);
    assert!(
        !r.log
            .lines()
            .iter()
            .any(|l| l.contains("delivered after this turn")),
        "the user is not told it will be delivered: {:?}",
        r.log.lines()
    );
    // Nothing is left for the turn end to send.
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert!(!r.store.calls().iter().any(|c| c.starts_with("claim")));
}

#[tokio::test]
async fn the_allowance_decides_before_the_other_reasons_a_now_message_cannot_interrupt() {
    // Same message, same engine state (its cancel is not safe after the tripwire), two allowances. With the allowance left, the message
    // goes out after the turn and the user is told so. With it spent, the message is refused and no promise of delivery is made.
    async fn tripped(max_turns: u32) -> Rig {
        let mut s = Settings::for_tests();
        s.max_turns = Some(max_turns);
        let r = Rig::with(s).await;
        r.hook.on_event(&Event::PromptSent(PromptSent {
            no: 1,
            session_id: "s1",
            text: "the task",
        }));
        r.hook.on_event(&Event::PromptReplied(PromptReplied {
            no: 1,
            session_id: "s1",
            stop_reason: Some("end_turn"),
            usage: Some(Usage::default()),
            error: None,
        }));
        assert_eq!(
            r.turn_end(10_000).await,
            TurnVote::Prompt(RESUME_PROMPT_INTERRUPTED.to_string())
        );
        assert_eq!(
            r.hook.stats().tripwires,
            1,
            "TRIGGER PRESENT: the cancel is not safe"
        );
        r
    }
    let left = tripped(8).await;
    left.hook.pretend_prompt_open(true);
    left.accept_now("m1", "redirect", "s1");
    left.hand_out();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        left.log
            .lines()
            .iter()
            .any(|l| l.contains("could not interrupt; delivered after this turn")),
        "{:?}",
        left.log.lines()
    );
    assert_eq!(
        left.store.state("m1"),
        Some(State::Pending),
        "kept for the turn end"
    );

    let spent = tripped(0).await;
    spent.hook.pretend_prompt_open(true);
    spent.accept_now("m1", "redirect", "s1");
    spent.hand_out();
    assert_eq!(
        settled(&spent, "m1").await,
        Some(State::Rejected(RejectReason::ContinuationLimit)),
        "{:?}",
        spent.log.lines()
    );
    assert!(
        !spent
            .log
            .lines()
            .iter()
            .any(|l| l.contains("delivered after this turn")),
        "{:?}",
        spent.log.lines()
    );
}

// ----: a message to an engine that works by itself, and a `now` message that found no prompt to interrupt -------------------------

/// A turn end at which a module asked before this one voted `Wait`: the engine works through the goal plugin's follow-up rounds.
async fn turn_end_while_the_engine_works(r: &Rig, left: u64) -> TurnVote {
    let raw = done_raw("end_turn");
    let classified = classify(&raw);
    let mut e = end(&raw, &classified, left, false);
    e.engine_working = true;
    r.hook.on_turn_end(&e).await
}

/// How often the user was told that the message did not interrupt (the log line of the notice; this rig has no event recorder).
fn notices(r: &Rig, id: &str) -> usize {
    let line = format!("message {id}: could not interrupt; delivered after this turn");
    r.log.lines().iter().filter(|l| l.contains(&line)).count()
}

/// The driver sent a prompt and the engine answered at once: `end_turn`, no usage, no update since the prompt. It is the answer of a
/// prompt the engine put in its own queue.
fn queued_answer(r: &Rig, no: u32, text: &str) {
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no,
        session_id: "s1",
        text,
    }));
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no,
        session_id: "s1",
        stop_reason: Some("end_turn"),
        usage: None,
        error: None,
    }));
}

#[tokio::test]
async fn b857_a_now_message_that_found_no_prompt_open_goes_out_at_the_turn_end_and_the_user_is_told_once()
 {
    let r = Rig::new().await;
    // No prompt is open (the run is between two prompts, in a retry wait or setting a session up): no interrupt starts.
    r.accept_now("m1", "stop and do this", "s1");
    r.hand_out();
    assert_eq!(r.hook.stats().interrupts, 0);
    let vote = r.turn_end(10_000).await;
    assert_eq!(prompt_text(&vote), "stop and do this");
    assert_eq!(notices(&r, "m1"), 1, "{:?}", r.log.lines());
    // a repeated hand-out and another turn end tell nobody again
    r.hand_out();
    r.prompt(2, "s1", "stop and do this", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(notices(&r, "m1"), 1, "{:?}", r.log.lines());
}

#[tokio::test]
async fn b857_a_next_message_that_goes_out_while_the_engine_works_gets_no_notice() {
    let r = Rig::new().await;
    r.accept("m1", "when you are done, also this", "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "when you are done, also this");
    assert_eq!(notices(&r, "m1"), 0, "{:?}", r.log.lines());
}

#[tokio::test]
async fn b857_a_message_the_engine_queued_behind_its_own_round_is_not_a_tripwire() {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept_now("m1", "stop and do this", "s1");
    r.hand_out();
    // The goal module voted `Wait` in this poll: the engine works through a follow-up round, and the message goes out beside it.
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "stop and do this");
    assert_eq!(notices(&r, "m1"), 1, "{:?}", r.log.lines());
    // The engine takes the prompt behind its round and answers at once.
    queued_answer(&r, 2, "stop and do this");
    r.hook.flush().await;
    assert_eq!(r.hook.stats().tripwires, 0, "{:?}", r.log.lines());
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    // The engine is kept: the next turn end has nothing to send and resets nothing.
    assert_eq!(
        turn_end_while_the_engine_works(&r, 10_000).await,
        TurnVote::Done
    );
    assert!(
        !r.log
            .lines()
            .iter()
            .any(|l| l.contains("tripwire") || l.contains("killed")),
        "{:?}",
        r.log.lines()
    );
}

#[tokio::test]
async fn b857_the_same_answer_to_a_message_sent_to_an_idle_engine_is_still_a_tripwire() {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also this", "s1");
    r.hand_out();
    // Nobody said the engine works by itself: the prompt went to an engine that had ended its turn.
    let vote = r.turn_end(10_000).await;
    assert_eq!(prompt_text(&vote), "also this");
    queued_answer(&r, 2, "also this");
    assert_eq!(r.hook.stats().tripwires, 1, "{:?}", r.log.lines());
    assert_eq!(
        r.turn_end(10_000).await,
        TurnVote::Prompt(RESUME_PROMPT_INTERRUPTED.to_string())
    );
}

#[tokio::test]
async fn b857_a_prompt_of_another_module_while_the_engine_works_is_judged_as_before() {
    // Only the user's message this module voted in that state is expected in the engine's queue.
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    assert_eq!(
        turn_end_while_the_engine_works(&r, 10_000).await,
        TurnVote::Done
    );
    queued_answer(&r, 2, "a prompt of another module");
    assert_eq!(r.hook.stats().tripwires, 1, "{:?}", r.log.lines());
}

// ----: a message the engine took behind a round that turned out to be its last -------------------------------------------------

#[tokio::test]
async fn b857_a_message_taken_behind_a_round_is_sent_again_when_the_engine_is_idle_and_began_no_run_since()
 {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also update the README", "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "also update the README");
    queued_answer(&r, 2, "also update the README");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    // While the engine works by itself nothing is sent again: the engine reads its queue when its round has ended.
    assert_eq!(
        turn_end_while_the_engine_works(&r, 10_000).await,
        TurnVote::Done
    );
    // No module says the engine works any more, and the engine began no run after it took the message (this rig has no record of the
    // engine's runs): the engine cannot have read it. It goes out once more, as an ordinary prompt.
    let claims = r.hook.stats().claims;
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        prompt_text(&vote),
        "also update the README",
        "{:?}",
        r.log.lines()
    );
    r.prompt(3, "s1", "also update the README", "end_turn");
    r.hook.flush().await;
    // The same delivery: no second claim, no second acknowledgement, and the row keeps the turn of the first send.
    assert_eq!(r.hook.stats().claims, claims);
    assert_eq!(r.hook.stats().delivered, 1);
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(r.hook.stats().tripwires, 0, "{:?}", r.log.lines());
    // Once: the next turn end and the seal have nothing to send, and the run closes.
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert!(!r.store.admission_open());
}

/// The driver sent a prompt while the engine worked through a round. The round went on: one `session/update` of it was read between
/// the prompt and its answer. The engine answered the request with `end_turn` and no usage: it put the prompt in its own queue.
fn queued_answer_in_a_round_that_streams(r: &Rig, no: u32, text: &str) {
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no,
        session_id: "s1",
        text,
    }));
    r.hook.on_wire(&agentd::direct::hooks::Wire {
        seq: u64::from(no),
        dir: agentd::acp::Dir::FromEngine,
        msg: &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk"}}}),
        stale: false,
    });
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no,
        session_id: "s1",
        stop_reason: Some("end_turn"),
        usage: None,
        error: None,
    }));
}

/// What the message module remembers of a message it sent while the engine worked: it went out again when the engine was idle and had
/// begun no run since, as one delivery.
async fn sent_again_once_when_the_engine_is_idle(r: &Rig) {
    r.hook.flush().await;
    assert_eq!(r.hook.stats().tripwires, 0, "{:?}", r.log.lines());
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert!(
        r.log
            .lines()
            .iter()
            .any(|l| l.contains("message m1 went out while the engine worked by itself")),
        "the message is remembered as taken behind the round: {:?}",
        r.log.lines()
    );
    // While the engine works by itself nothing is sent again.
    assert_eq!(
        turn_end_while_the_engine_works(r, 10_000).await,
        TurnVote::Done
    );
    // The round was the engine's last and it began no run after it took the message: the message goes out once more.
    let claims = r.hook.stats().claims;
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        prompt_text(&vote),
        "also update the README",
        "{:?}",
        r.log.lines()
    );
    r.prompt(3, "s1", "also update the README", "end_turn");
    r.hook.flush().await;
    assert_eq!(r.hook.stats().claims, claims);
    assert_eq!(r.hook.stats().delivered, 1);
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert_eq!(r.hook.stats().tripwires, 0, "{:?}", r.log.lines());
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert!(!r.store.admission_open());
}

/// The engine that works through a follow-up round is sending updates of that round: one of them between the prompt and its answer
/// says nothing about whether the engine queued the prompt.
#[tokio::test]
async fn b857_a_message_taken_behind_a_round_that_streams_is_sent_again_when_the_engine_is_idle() {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also update the README", "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "also update the README");
    queued_answer_in_a_round_that_streams(&r, 2, "also update the README");
    sent_again_once_when_the_engine_is_idle(&r).await;
}

/// A busy engine may answer the request later than the window of the tripwire: the window is for an engine that was idle.
#[tokio::test]
async fn b857_a_message_a_working_engine_answered_late_is_sent_again_when_the_engine_is_idle() {
    let r = Rig::with(Settings {
        trip_window: Duration::ZERO,
        ..Settings::for_tests()
    })
    .await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also update the README", "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "also update the README");
    queued_answer(&r, 2, "also update the README");
    sent_again_once_when_the_engine_is_idle(&r).await;
}

/// The same answers to a message that went to an idle engine are no tripwire and nothing is remembered: an idle engine that streams
/// or takes its time ran the prompt.
#[tokio::test]
async fn b857_an_idle_engine_that_streams_before_an_answer_without_usage_is_judged_as_before() {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also this", "s1");
    r.hand_out();
    let vote = r.turn_end(10_000).await;
    assert_eq!(prompt_text(&vote), "also this");
    queued_answer_in_a_round_that_streams(&r, 2, "also this");
    r.hook.flush().await;
    assert_eq!(r.hook.stats().tripwires, 0, "{:?}", r.log.lines());
    assert!(
        !r.log
            .lines()
            .iter()
            .any(|l| l.contains("went out while the engine worked by itself")),
        "{:?}",
        r.log.lines()
    );
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
}

/// A file for the observer's log of the engine's runs, in the test's own scratch space.
fn rounds_log(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "b857-rounds-{name}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// The observer writes that run `n` of the engine began, or ended.
fn run_line(file: &std::path::Path, ev: &str, n: u32) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    let line = if ev == "start" {
        json!({"ev": "start", "n": n, "t": 1})
    } else {
        json!({"ev": "end", "n": n, "t": 2, "status": "completed", "iterations": 1, "text": "ok"})
    };
    writeln!(f, "{line}").unwrap();
}

/// The first prompt was answered, the engine went on by itself (run 2 is in flight), and the message `id` went out beside that run: the
/// engine took it behind the run.
async fn taken_behind_run_two(r: &Rig, file: &std::path::Path, id: &str, text: &str, no: u32) {
    if no == 2 {
        r.prompt(1, "s1", "the task", "end_turn");
        run_line(file, "start", 1);
        run_line(file, "end", 1);
        run_line(file, "start", 2);
    }
    r.accept(id, text, "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(r, 10_000).await;
    assert_eq!(prompt_text(&vote), text);
    queued_answer(r, no, text);
    r.hook.flush().await;
    assert_eq!(r.store.state(id), Some(State::Delivered { turn: no }));
}

#[tokio::test]
async fn b857_a_message_the_engine_began_a_run_after_is_not_sent_again() {
    let file = rounds_log("read");
    let r = Rig::with_rounds(&file).await;
    taken_behind_run_two(&r, &file, "m1", "also update the README", 2).await;
    // The run in flight ends and the engine begins the next one, with the message in its queue; later its loop ends.
    run_line(&file, "end", 2);
    run_line(&file, "start", 3);
    run_line(&file, "end", 3);
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert!(
        r.log
            .lines()
            .iter()
            .any(|l| l.contains("message m1: the engine began a run after it took the message")),
        "{:?}",
        r.log.lines()
    );
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert!(!r.store.admission_open());
}

#[tokio::test]
async fn b857_the_runs_the_engine_began_before_it_took_the_message_do_not_count() {
    let file = rounds_log("last");
    let r = Rig::with_rounds(&file).await;
    taken_behind_run_two(&r, &file, "m1", "also update the README", 2).await;
    // The run in flight ends, and it was the engine's last: nothing began after the message.
    run_line(&file, "end", 2);
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        prompt_text(&vote),
        "also update the README",
        "{:?}",
        r.log.lines()
    );
    // A vote the driver did not send is made again, at the seal as at a turn end.
    let vote = r.seal(10_000).await;
    assert_eq!(prompt_text(&vote), "also update the README");
    assert!(r.store.admission_open(), "nothing is sealed over it");
    r.prompt(3, "s1", "also update the README", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
    assert_eq!(r.hook.stats().delivered, 1);
}

#[tokio::test]
async fn b857_two_messages_behind_one_round_need_a_run_each() {
    let file = rounds_log("two");
    let r = Rig::with_rounds(&file).await;
    taken_behind_run_two(&r, &file, "m1", "also update the README", 2).await;
    taken_behind_run_two(&r, &file, "m2", "and the changelog", 3).await;
    // One run began after the two were taken: the engine read the first, and its loop ended before the second.
    run_line(&file, "end", 2);
    run_line(&file, "start", 3);
    run_line(&file, "end", 3);
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        prompt_text(&vote),
        "and the changelog",
        "{:?}",
        r.log.lines()
    );
    r.prompt(4, "s1", "and the changelog", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);

    // No run began after the two were taken: both go out again, oldest first, and the run of the first send is not taken for a run
    // of the second.
    let file = rounds_log("two-none");
    let r = Rig::with_rounds(&file).await;
    taken_behind_run_two(&r, &file, "m1", "also update the README", 2).await;
    taken_behind_run_two(&r, &file, "m2", "and the changelog", 3).await;
    run_line(&file, "end", 2);
    let vote = r.turn_end(10_000).await;
    assert_eq!(prompt_text(&vote), "also update the README");
    r.prompt(4, "s1", "also update the README", "end_turn");
    run_line(&file, "start", 3);
    run_line(&file, "end", 3);
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        prompt_text(&vote),
        "and the changelog",
        "{:?}",
        r.log.lines()
    );
    r.prompt(5, "s1", "and the changelog", "end_turn");
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.hook.stats().delivered, 2);
}

#[tokio::test]
async fn b857_a_new_session_is_told_by_the_record_and_nothing_is_sent_again() {
    let file = rounds_log("session");
    let r = Rig::with_rounds(&file).await;
    taken_behind_run_two(&r, &file, "m1", "also update the README", 2).await;
    run_line(&file, "end", 2);
    // The engine is replaced: the first prompt of the new session carries the record of the delivered messages.
    r.session("s2", 2);
    r.hook.flush().await;
    assert_eq!(r.turn_end(10_000).await, TurnVote::Done);
    assert_eq!(r.seal(10_000).await, TurnVote::Done);
}

#[tokio::test]
async fn b857_a_message_that_cannot_go_out_again_is_named_to_the_user_as_not_read() {
    let file = rounds_log("late");
    let r = Rig::with_rounds(&file).await;
    taken_behind_run_two(&r, &file, "m1", "also update the README", 2).await;
    run_line(&file, "end", 2);
    // Less than the 300 s a message turn needs are left of the run: no prompt goes out, and the user is not left to take
    // "delivered" for read.
    assert_eq!(r.turn_end(100).await, TurnVote::Done);
    assert_eq!(
        r.log
            .lines()
            .iter()
            .filter(|l| l
                .contains("message m1: not read; the run ended before this message was reached"))
            .count(),
        1,
        "{:?}",
        r.log.lines()
    );
    assert_eq!(r.turn_end(100).await, TurnVote::Done);
}

/// The log of the engine's runs can be longer than one call reads. A count of part of it is no count: the lines read later are runs the
/// engine began before it took the message, and they must not settle it.
#[tokio::test]
async fn b857_a_log_of_runs_too_long_to_read_at_once_does_not_settle_a_message() {
    let file = rounds_log("long");
    let r = Rig::with_rounds(&file).await;
    // 1,500 runs, all before the message; their end lines carry a long text, as the observer's do.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .unwrap();
        let text = "x".repeat(1_500);
        for n in 1..=1_500u32 {
            writeln!(f, "{}", json!({"ev": "start", "n": n, "t": 1})).unwrap();
            writeln!(
                f,
                "{}",
                json!({"ev": "end", "n": n, "t": 2, "status": "completed", "iterations": 1, "text": text})
            )
            .unwrap();
        }
    }
    // TRIGGER PRESENT: more than the 2 MiB one call reads.
    assert!(std::fs::metadata(&file).unwrap().len() > 2 * 1024 * 1024 + 64 * 1024);
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also update the README", "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "also update the README");
    queued_answer(&r, 2, "also update the README");
    r.hook.flush().await;
    // No run began after the message: it goes out again, whatever the rest of the log holds.
    let vote = r.turn_end(10_000).await;
    assert_eq!(
        prompt_text(&vote),
        "also update the README",
        "{:?}",
        r.log.lines()
    );
}

// ----: what the user said is kept for the sessions that follow ------------------------------------------------------------------

#[tokio::test]
async fn b857_a_delivered_message_joins_the_record_of_the_run_and_no_other_prompt_does() {
    use agentd::direct::messages::earlier::{self, Earlier, Said};
    let agent_data = std::env::temp_dir().join(format!(
        "b857-said-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&agent_data).unwrap();
    let store = MemoryStore::new();
    let hook = MessagesHook::new(store.clone(), Settings::for_tests());
    let log = MemoryLog::new();
    let (_cancel, rx) = watch::channel(false);
    let said = earlier::Log::start(
        &agent_data,
        1,
        Earlier::default(),
        agentd::secrets::SecretSet::default(),
        None,
        log.clone(),
    );
    hook.bind(Wiring::new(DriverCtl::new(), rx, log.clone()).with_said(said.clone()));
    let session = |id: &str, number: u32| {
        hook.on_event(&Event::SessionStarted(SessionStarted {
            session_id: id,
            number,
            response: &json!({}),
            cwd: "/w",
            trigger: None,
        }));
    };
    let sent = |no: u32, session: &str, text: &str| {
        hook.on_event(&Event::PromptSent(PromptSent {
            no,
            session_id: session,
            text,
        }));
    };
    let accept = |id: &str, text: &str, epoch: &str| {
        store
            .accept(Message {
                id: id.into(),
                text: text.into(),
                mode: Mode::Next,
                epoch: epoch.into(),
                seq: 0,
            })
            .unwrap();
    };
    let raw = done_raw("end_turn");
    let classified = classify(&raw);
    session("s1", 1);
    hook.flush().await;
    // the task is no message of the user to a working run
    sent(1, "s1", "the task");
    hook.flush().await;
    assert!(said.record().is_empty());
    // a message is claimed and voted: not delivered yet
    accept("m1", "use tabs, not spaces", "s1");
    hook.offer(store.handout());
    let vote = hook
        .on_turn_end(&end(&raw, &classified, 10_000, false))
        .await;
    assert_eq!(prompt_text(&vote), "use tabs, not spaces");
    hook.flush().await;
    assert!(said.record().is_empty(), "voted is not delivered");
    // the prompt goes out: that is the delivery
    sent(2, "s1", "use tabs, not spaces");
    assert_eq!(
        said.record().messages,
        vec![Said {
            turn: 1,
            session: 1,
            text: "use tabs, not spaces".into(),
            id: Some("m1".into()),
            inherited: false,
            unconfirmed: false,
        }]
    );
    // the ladder sends the same text again: the same delivery
    sent(3, "s1", "use tabs, not spaces");
    // a prompt of another module, with the same words as nothing the user sent
    sent(4, "s1", "Continue working toward the goal.");
    hook.flush().await;
    assert_eq!(said.record().messages.len(), 1);
    // a message delivered in the next session carries that session's number
    session("s2", 2);
    hook.flush().await;
    accept("m2", "skip the Windows build", "s2");
    hook.offer(store.handout());
    let e = TurnEnd {
        session_id: Some("s2"),
        session: 2,
        ..end(&raw, &classified, 10_000, false)
    };
    let vote = hook.on_turn_end(&e).await;
    assert_eq!(prompt_text(&vote), "skip the Windows build");
    sent(5, "s2", "skip the Windows build");
    hook.flush().await;
    assert_eq!(
        said.record().messages,
        vec![
            Said {
                turn: 1,
                session: 1,
                text: "use tabs, not spaces".into(),
                id: Some("m1".into()),
                inherited: false,
                unconfirmed: false,
            },
            Said {
                turn: 1,
                session: 2,
                text: "skip the Windows build".into(),
                id: Some("m2".into()),
                inherited: false,
                unconfirmed: false,
            }
        ]
    );
    // the machine holds the record for the digest of a fresh session
    assert_eq!(earlier::read_local(&agent_data), said.record());
}

// ----: what a review of the change found ----------------------------------------------------------------------------------------

/// The engine can be replaced a moment after a message went out (the tripwire, a dead engine), and the fresh session's first prompt is
/// written at once. The record must hold the message when `PromptSent` returns: a record that waits for the module's worker, which may be
/// in the middle of a call to the control plane, comes too late.
#[tokio::test]
async fn b857_the_record_holds_a_delivery_when_the_prompt_goes_out_not_when_the_worker_gets_to_it()
{
    use agentd::direct::messages::earlier::{self, Earlier};
    let agent_data = std::env::temp_dir().join(format!(
        "b857-said-now-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&agent_data).unwrap();
    let store = MemoryStore::new();
    let hook = MessagesHook::new(store.clone(), Settings::for_tests());
    let log = MemoryLog::new();
    let (_cancel, rx) = watch::channel(false);
    let said = earlier::Log::start(
        &agent_data,
        1,
        Earlier::default(),
        agentd::secrets::SecretSet::default(),
        None,
        log.clone(),
    );
    hook.bind(Wiring::new(DriverCtl::new(), rx, log.clone()).with_said(said.clone()));
    hook.on_event(&Event::SessionStarted(SessionStarted {
        session_id: "s1",
        number: 1,
        response: &json!({}),
        cwd: "/w",
        trigger: None,
    }));
    hook.flush().await;
    store
        .accept(Message {
            id: "m1".into(),
            text: "keep the public names".into(),
            mode: Mode::Next,
            epoch: "s1".into(),
            seq: 0,
        })
        .unwrap();
    hook.offer(store.handout());
    let raw = done_raw("end_turn");
    let classified = classify(&raw);
    let vote = hook
        .on_turn_end(&end(&raw, &classified, 10_000, false))
        .await;
    assert_eq!(prompt_text(&vote), "keep the public names");
    hook.on_event(&Event::PromptSent(PromptSent {
        no: 2,
        session_id: "s1",
        text: "keep the public names",
    }));
    // No await between the event and the reads: the module's worker has not run.
    assert_eq!(said.record().messages.len(), 1, "in the record at once");
    assert_eq!(
        earlier::read_local(&agent_data).messages.len(),
        1,
        "and in the file a fresh session reads"
    );
}

/// The ladder sends a message again after a provider error. No poll stands behind that send, so nobody said the engine works by itself:
/// an answer that comes at once with no usage is judged as it is for a prompt to an idle engine.
#[tokio::test]
async fn b857_a_resend_of_a_message_by_the_ladder_is_judged_as_a_prompt_to_an_idle_engine() {
    let r = Rig::new().await;
    r.prompt(1, "s1", "the task", "end_turn");
    r.accept("m1", "also this", "s1");
    r.hand_out();
    let vote = turn_end_while_the_engine_works(&r, 10_000).await;
    assert_eq!(prompt_text(&vote), "also this");
    // the first send ends in a provider error
    r.hook.on_event(&Event::PromptSent(PromptSent {
        no: 2,
        session_id: "s1",
        text: "also this",
    }));
    let failed = AcpError::Protocol("503 Service unavailable".into());
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no: 2,
        session_id: "s1",
        stop_reason: None,
        usage: None,
        error: Some(&failed),
    }));
    assert_eq!(r.hook.stats().tripwires, 0);
    // the ladder's resend of the same text, answered from a queue
    queued_answer(&r, 3, "also this");
    assert_eq!(r.hook.stats().tripwires, 1, "{:?}", r.log.lines());
}
