//! Each test names the schedule it closes.
#![allow(clippy::unwrap_used)]

use agentd::acp::{Dir, Watched};
use agentd::direct::classify::{PromptDone, RawEnd, classify};
use agentd::direct::hooks::{
    DirectHook, DriverCtl, Event, PromptReplied, PromptSent, SessionStarted, TurnEnd, TurnVote,
    Wire,
};
use agentd::direct::messages::reap::Reaper;
use agentd::direct::messages::{
    FailReason, MemoryStore, Message, MessageStore, MessagesHook, Mode, Op, RejectReason, Settings,
    State, Wiring,
};
use agentd::logging::MemoryLog;
use harness_protocol::common::Usage;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

mod common;

struct Rig {
    hook: Arc<MessagesHook>,
    store: Arc<MemoryStore>,
    log: Arc<MemoryLog>,
}

impl Rig {
    async fn new() -> Self {
        Self::with(Settings::for_tests(), None).await
    }

    async fn with(settings: Settings, reaper: Option<Reaper>) -> Self {
        let store = MemoryStore::new();
        let hook = MessagesHook::new(store.clone(), settings);
        let log = MemoryLog::new();
        let (_cancel, rx) = watch::channel(false);
        let mut w = Wiring::new(DriverCtl::new(), rx, log.clone());
        if let Some(r) = reaper {
            w = w.with_reaper(r);
        }
        hook.bind(w);
        let r = Self { hook, store, log };
        r.session("s1", 1);
        r.hook.flush().await;
        r
    }

    /// A module whose store is the server model as the worker generation `generation` sees it.
    async fn viewed(generation: u32) -> Self {
        let store = MemoryStore::new();
        let hook = MessagesHook::new(store.view(generation), Settings::for_tests());
        let log = MemoryLog::new();
        let (_cancel, rx) = watch::channel(false);
        hook.bind(Wiring::new(DriverCtl::new(), rx, log.clone()));
        let r = Self { hook, store, log };
        r.session("s1", 1);
        r.hook.flush().await;
        r
    }

    fn session(&self, id: &str, number: u32) {
        self.hook.on_event(&Event::SessionStarted(SessionStarted {
            session_id: id,
            number,
            response: &json!({}),
            cwd: "/w",
            trigger: None,
        }));
    }

    fn accept(&self, id: &str, text: &str, mode: Mode) {
        self.store
            .accept(Message {
                id: id.into(),
                text: text.into(),
                mode,
                epoch: "s1".into(),
                seq: 0,
            })
            .unwrap();
    }

    fn hand_out(&self) {
        self.hook.offer(self.store.handout());
    }

    fn sent(&self, no: u32, session: &str, text: &str) {
        self.hook.on_event(&Event::PromptSent(PromptSent {
            no,
            session_id: session,
            text,
        }));
    }

    fn replied(&self, no: u32, session: &str) {
        self.hook.on_wire(&Wire {
            seq: u64::from(no),
            dir: Dir::FromEngine,
            msg: &json!({"method": "session/update", "params": {"sessionId": session}}),
            stale: false,
        });
        self.hook.on_event(&Event::PromptReplied(PromptReplied {
            no,
            session_id: session,
            stop_reason: Some("end_turn"),
            usage: Some(Usage {
                input_tokens: 5,
                output_tokens: 5,
                ..Usage::default()
            }),
            error: None,
        }));
    }

    async fn turn_end_ms(&self, left_ms: u64) -> TurnVote {
        let raw = RawEnd {
            watched: Watched::Done(Ok(PromptDone {
                stop_reason: "end_turn".into(),
                usage: None,
            })),
            cancel_sent: false,
            message: "done".into(),
            setup_error: None,
        };
        let classified = classify(&raw);
        let end = TurnEnd {
            n: 1,
            session_id: Some("s1"),
            session: 1,
            raw: &raw,
            classified: &classified,
            files_changed: Some(1),
            interrupted: false,
            time_left: Duration::from_millis(left_ms),
            tool_calls: 1,
        };
        self.hook.on_turn_end(&end).await
    }

    async fn turn_end(&self) -> TurnVote {
        self.turn_end_ms(10_000_000).await
    }

    async fn seal(&self) -> TurnVote {
        let raw = RawEnd {
            watched: Watched::Done(Ok(PromptDone {
                stop_reason: "end_turn".into(),
                usage: None,
            })),
            cancel_sent: false,
            message: "done".into(),
            setup_error: None,
        };
        let classified = classify(&raw);
        let end = TurnEnd {
            n: 1,
            session_id: Some("s1"),
            session: 1,
            raw: &raw,
            classified: &classified,
            files_changed: Some(1),
            interrupted: false,
            time_left: Duration::from_secs(10_000),
            tool_calls: 1,
        };
        self.hook.on_seal(&end).await
    }
}

// ---- a claim that comes back late ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_that_comes_back_after_the_session_changed_is_given_back_and_not_sent() {
    // Schedule: the claim commits in session s1, its answer is slow, session s2 starts meanwhile. The text must not go into s2.
    let r = Rig::new().await;
    r.accept("m1", "for session one", Mode::Next);
    r.hand_out();
    r.store.delay_after(Op::Claim, Duration::from_millis(250));
    let hook = r.hook.clone();
    let late = tokio::spawn(async move {
        let raw = RawEnd {
            watched: Watched::Done(Ok(PromptDone {
                stop_reason: "end_turn".into(),
                usage: None,
            })),
            cancel_sent: false,
            message: "done".into(),
            setup_error: None,
        };
        let classified = classify(&raw);
        let end = TurnEnd {
            n: 1,
            session_id: Some("s1"),
            session: 1,
            raw: &raw,
            classified: &classified,
            files_changed: Some(1),
            interrupted: false,
            time_left: Duration::from_secs(10_000),
            tool_calls: 1,
        };
        hook.on_turn_end(&end).await
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    r.session("s2", 2);
    assert_eq!(
        late.await.unwrap(),
        TurnVote::Done,
        "a message claimed in the old session is not sent into the new one"
    );
    r.hook.flush().await;
    assert!(
        r.store.calls().iter().any(|c| c == "release m1"),
        "{:?}",
        r.store.calls()
    );
    assert_eq!(
        r.store.state("m1"),
        Some(State::Held),
        "{:?}",
        r.store.history("m1")
    );
}

#[tokio::test]
async fn a_vote_the_driver_never_sent_is_given_back_when_the_session_changes() {
    let r = Rig::new().await;
    r.accept("m1", "voted, never sent", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.session("s2", 2);
    r.hook.flush().await;
    assert!(
        r.store.calls().iter().any(|c| c == "release m1"),
        "{:?}",
        r.store.calls()
    );
    assert_eq!(r.store.state("m1"), Some(State::Held));
}

#[tokio::test]
async fn the_clock_is_read_when_the_message_is_about_to_go_out_not_when_the_turn_ended() {
    // 300.02 s were left at the turn end and the claim took 0.1 s: less than the 300 s the module keeps, so the message is given back.
    let r = Rig::new().await;
    r.accept("m1", "too late by a hair", Mode::Next);
    r.hand_out();
    r.store.delay_after(Op::Claim, Duration::from_millis(120));
    assert_eq!(r.turn_end_ms(300_020).await, TurnVote::Done);
    r.hook.flush().await;
    assert!(
        r.store.calls().iter().any(|c| c == "release m1"),
        "{:?}",
        r.store.calls()
    );
}

// ---- an interrupt claim that is out while the run ends -------------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_an_interrupt_makes_while_the_run_ends_is_settled_and_never_left_dispatching() {
    // Schedule: a `now` message is being claimed (slow answer) when the shutdown starts. The close waits for the claim, then fails the
    // message as not sent; nothing is left dispatching.
    let r = Rig::new().await;
    r.hook.pretend_prompt_open(true);
    r.accept("m1", "redirect", Mode::Now);
    r.store.delay_after(Op::Claim, Duration::from_millis(300));
    r.hand_out();
    tokio::time::sleep(Duration::from_millis(60)).await;
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::NotSent)),
        "{:?} {:?}",
        r.store.history("m1"),
        r.store.calls()
    );
}

#[tokio::test]
async fn the_close_does_not_drain_before_the_interrupt_claim_arrives() {
    let r = Rig::new().await;
    r.hook.pretend_prompt_open(true);
    r.accept("m1", "redirect", Mode::Now);
    r.store.delay_after(Op::Claim, Duration::from_millis(200));
    r.hand_out();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let t = std::time::Instant::now();
    r.hook.close().await;
    assert!(
        t.elapsed() >= Duration::from_millis(100),
        "the close waited for the claim: {:?}",
        t.elapsed()
    );
    assert!(!matches!(r.store.state("m1"), Some(State::Dispatching)));
}

#[tokio::test]
async fn a_turn_end_waits_for_the_claim_an_interrupt_is_making() {
    // The `now` message's claim is slow and the queue is empty: the turn end waits for the claim instead of reading an empty queue.
    let r = Rig::new().await;
    r.hook.pretend_prompt_open(true);
    r.accept("m1", "interrupt me", Mode::Now);
    r.store.delay_after(Op::Claim, Duration::from_millis(150));
    r.hand_out();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let first = r.turn_end().await;
    assert_eq!(first, TurnVote::Prompt("interrupt me".into()));
}

#[tokio::test]
async fn an_interrupt_aimed_at_a_turn_that_has_ended_goes_out_at_the_next_turn_end_instead() {
    // Schedule: the claim is slow; the turn the user saw ends and another prompt starts; the claim then returns. The cancel would hit the
    // other prompt, so the module does not ask for it.
    let r = Rig::new().await;
    r.hook.pretend_prompt_open(true);
    r.sent(1, "s1", "the task");
    r.accept("m1", "redirect", Mode::Now);
    r.store.delay_after(Op::Claim, Duration::from_millis(200));
    r.hand_out();
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.replied(1, "s1");
    r.sent(2, "s1", "a later prompt of the harness");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        r.log.lines().iter().any(|l| l.contains("a later prompt")),
        "the module said why it did not interrupt: {:?}",
        r.log.lines()
    );
    r.replied(2, "s1");
    assert_eq!(r.turn_end().await, TurnVote::Prompt("redirect".into()));
}

// ---- outcomes the module owes ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_acknowledgement_that_could_not_be_posted_is_retried_at_the_close() {
    let r = Rig::new().await;
    r.accept("m1", "sent", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.store.fail(Op::Submitted, 3);
    r.sent(2, "s1", "sent");
    r.hook.flush().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Dispatching),
        "the three attempts failed"
    );
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Delivered { turn: 2 }),
        "{:?}",
        r.store.calls()
    );
}

#[tokio::test]
async fn a_failure_to_settle_a_too_long_message_is_retried_at_the_close() {
    let r = Rig::new().await;
    r.accept("m1", &"x".repeat(262_145), Mode::Next);
    r.hand_out();
    r.store.fail(Op::Failed, 3);
    assert_eq!(r.turn_end().await, TurnVote::Done);
    assert_eq!(r.store.state("m1"), Some(State::Dispatching));
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::TooLarge))
    );
}

#[tokio::test]
async fn a_claim_that_could_not_be_given_back_is_given_back_at_the_close() {
    // 300.02 s left at the turn end, a slow claim: by the time it returns the clock says no, and the release fails three times.
    let r = Rig::new().await;
    r.accept("m1", "late", Mode::Next);
    r.hand_out();
    r.store.delay_after(Op::Claim, Duration::from_millis(120));
    r.store.fail(Op::Release, 3);
    assert_eq!(r.turn_end_ms(300_020).await, TurnVote::Done);
    assert_eq!(
        r.store.state("m1"),
        Some(State::Dispatching),
        "not given back yet"
    );
    r.hook.close().await;
    assert!(
        matches!(r.store.state("m1"), Some(State::Rejected(_))),
        "given back at the close, then rejected by the terminal seal: {:?}",
        r.store.history("m1")
    );
}

// ---- the sent text is matched to the claim ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_prompt_with_the_same_text_in_another_session_is_not_the_claimed_message() {
    let r = Rig::new().await;
    r.accept("m1", "same words", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.sent(2, "s9", "same words");
    r.hook.flush().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Dispatching),
        "not acknowledged: it is another session's prompt"
    );
}

#[tokio::test]
async fn nothing_is_sent_while_a_prompt_is_open() {
    let r = Rig::new().await;
    r.accept("m1", "wait", Mode::Next);
    r.hand_out();
    r.hook.pretend_prompt_open(true);
    assert_eq!(
        r.turn_end().await,
        TurnVote::Done,
        "a turn end with a prompt open is not a turn end: nothing is claimed"
    );
    assert!(
        r.store.calls().iter().all(|c| !c.starts_with("claim")),
        "{:?}",
        r.store.calls()
    );
    assert!(
        r.log.lines().iter().any(|l| l.contains("prompt is open")),
        "{:?}",
        r.log.lines()
    );
}

// ---- the seal --------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_seal_that_keeps_returning_messages_the_module_cannot_send_closes_the_run_instead_of_asking_eight_times()
 {
    let r = Rig::new().await;
    r.accept("m1", "stuck", Mode::Next);
    r.hand_out();
    r.store.fail(Op::Claim, 99);
    assert_eq!(r.seal().await, TurnVote::Done);
    let seals: Vec<String> = r
        .store
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("seal"))
        .collect();
    assert!(seals.len() <= 3, "{seals:?}");
    assert_eq!(
        seals.last().map(String::as_str),
        Some("seal s1 terminal"),
        "{seals:?}"
    );
    assert_eq!(
        r.store.state("m1"),
        Some(State::Rejected(RejectReason::RunSealed))
    );
}

// ---- the allowance ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_message_turn_that_never_got_a_reply_is_charged_when_the_session_ends() {
    let r = Rig::new().await;
    r.accept("m1", "hangs", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.sent(2, "s1", "hangs");
    tokio::time::sleep(Duration::from_millis(60)).await;
    r.session("s2", 2);
    let (turns, exec) = r.hook.allowance();
    assert_eq!(turns, 1);
    assert!(exec >= Duration::from_millis(50), "{exec:?}");
}

#[tokio::test]
async fn a_message_turn_is_charged_when_another_prompt_replaces_it_without_a_reply() {
    let r = Rig::new().await;
    r.accept("m1", "error then resume", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.sent(2, "s1", "error then resume");
    tokio::time::sleep(Duration::from_millis(40)).await;
    r.sent(3, "s1", "a different prompt");
    let (_, exec) = r.hook.allowance();
    assert!(exec >= Duration::from_millis(30), "{exec:?}");
}

// ---- the kill path says when it cannot vouch --------------------------------------------------------------------------------------------

fn fabricated_proc(tag: &str) -> (PathBuf, u32) {
    // A /proc tree with one tagged process that no kill can reach: a pid no kernel hands out.
    let root = common::scratch("proc-hard");
    let pid: u32 = 4_194_303;
    let d = root.join(pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("stat"),
        format!("{pid} (stuck) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 77 0 0 0\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("status"),
        "Name:\tstuck\nUid:\t1000\t1000\t1000\t1000\n",
    )
    .unwrap();
    std::fs::write(d.join("environ"), format!("AGENTD_ENGINE_TAG={tag}\0")).unwrap();
    (root, pid)
}

#[tokio::test]
async fn when_the_engines_processes_cannot_be_stopped_the_run_stops_instead_of_sending_a_prompt_into_a_live_engine()
 {
    let (root, _) = fabricated_proc("t1");
    let reaper = Reaper::with_root(root, 100, "t1", None);
    let mut s = Settings::for_tests();
    s.quiesce = Duration::from_millis(100);
    let r = Rig::with(s, Some(reaper)).await;
    // the tripwire: end_turn, usage 0, no update, an answer within moments
    r.sent(1, "s1", "the task");
    r.hook.on_event(&Event::PromptReplied(PromptReplied {
        no: 1,
        session_id: "s1",
        stop_reason: Some("end_turn"),
        usage: Some(Usage::default()),
        error: None,
    }));
    match r.turn_end().await {
        TurnVote::Stop { detail } => assert!(detail.contains("could not be stopped"), "{detail}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_engine_user_is_swept_only_when_it_is_known_to_differ_from_agentds_own() {
    use agentd::direct::messages::reap::sweep_uid;
    assert_eq!(sweep_uid(Some(0), Some(1001)), Some(1001));
    assert_eq!(
        sweep_uid(Some(1001), Some(1001)),
        None,
        "the same user: the sweep would take agentd"
    );
    assert_eq!(
        sweep_uid(None, Some(1001)),
        None,
        "agentd's own user is unknown: fail closed"
    );
    assert_eq!(sweep_uid(Some(0), None), None);
}

#[test]
fn a_proc_that_cannot_be_read_is_reported_and_never_read_as_an_empty_family() {
    let nowhere = PathBuf::from("/nonexistent-proc-root-for-the-test");
    let r = Reaper::with_root(nowhere, 100, "t", None);
    let report = r.reap_blocking(false, Duration::from_millis(200));
    assert!(report.unverified, "{report:?}");
}

#[test]
fn the_engine_that_agentd_marked_is_the_only_one_kept() {
    // Two tagged children of agentd; the marked one is kept, the other is a target.
    let root = common::scratch("proc-keep");
    let mk = |pid: u32, start: u64| {
        let d = root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!("{pid} (p) S 100 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0 0\n"),
        )
        .unwrap();
        std::fs::write(d.join("status"), "Name:\tp\nUid:\t1000\t1000\t1000\t1000\n").unwrap();
        std::fs::write(d.join("environ"), "AGENTD_ENGINE_TAG=t2\0").unwrap();
    };
    mk(4_194_301, 10);
    mk(4_194_302, 20);
    let r = Reaper::with_root(root, 100, "t2", None);
    r.mark_engine_pid(4_194_301);
    let report = r.reap_blocking(true, Duration::from_millis(150));
    assert!(!report.killed.contains(&4_194_301), "{report:?}");
    assert!(report.killed.contains(&4_194_302), "{report:?}");
}

#[test]
fn the_settings_come_from_the_environment_and_off_turns_the_module_off() {
    let get = |pairs: Vec<(&'static str, &'static str)>| {
        Settings::from_env(&move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        })
    };
    let s = get(vec![]);
    assert!(s.enabled);
    assert_eq!((s.max_turns, s.interrupts, s.queue_cap), (8, 5, 16));
    assert_eq!(s.max_exec, Duration::from_secs(1_800));
    assert_eq!(s.min_left, Duration::from_secs(300));
    let s = get(vec![
        ("AGENTD_MESSAGES", "off"),
        ("AGENTD_MESSAGES_MAX_TURNS", "3"),
        ("AGENTD_MESSAGES_MAX_EXEC_S", "60"),
        ("AGENTD_MESSAGES_MIN_LEFT_S", "0"),
        ("AGENTD_MESSAGES_INTERRUPTS", "1"),
        ("AGENTD_MESSAGES_QUIESCE_S", "2"),
        ("AGENTD_MESSAGES_QUEUE", "4"),
    ]);
    assert!(!s.enabled);
    assert_eq!((s.max_turns, s.interrupts, s.queue_cap), (3, 1, 4));
    assert_eq!(
        (s.max_exec, s.min_left, s.quiesce),
        (
            Duration::from_secs(60),
            Duration::ZERO,
            Duration::from_secs(2)
        )
    );
    // a value that does not parse is ignored
    assert_eq!(
        get(vec![("AGENTD_MESSAGES_MAX_TURNS", "many")]).max_turns,
        8
    );
}

// ---- protocol 1.38.0: outcomes before the seal ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_acknowledgement_of_a_send_is_posted_before_the_seal_asks_whether_anything_is_pending()
{
    // The send's acknowledgement is a background call; the seal waits for it.
    let r = Rig::new().await;
    r.accept("m1", "sent", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.store.delay(Op::Submitted, Duration::from_millis(150));
    r.sent(2, "s1", "sent");
    r.replied(2, "s1");
    assert_eq!(r.seal().await, TurnVote::Done);
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 2 }));
    assert!(!r.store.admission_open(), "the seal closed the run");
    let calls = r.store.calls();
    let seals = calls.iter().filter(|c| c.starts_with("seal")).count();
    assert_eq!(
        seals, 1,
        "one seal, not a refused one and a second: {calls:?}"
    );
}

#[tokio::test]
async fn a_seal_the_store_refuses_as_a_bad_transition_ends_the_vote_and_the_close_settles_the_message()
 {
    // A message the module does not know is dispatching (another claimant): the normal seal is refused; the run goes on to its end and the
    // terminal seal turns it into delivery-unknown.
    let r = Rig::new().await;
    r.accept("m1", "someone else's", Mode::Next);
    r.store.claim("m1", "s1").await.unwrap();
    assert_eq!(r.seal().await, TurnVote::Done);
    assert!(r.store.admission_open());
    r.hook.close().await;
    assert_eq!(r.store.state("m1"), Some(State::DeliveryUnknown));
}

#[tokio::test]
async fn a_claim_whose_answer_was_lost_is_found_before_the_seal_and_sent() {
    // The claim is applied on the server and its answer is lost three times: the module cannot tell whether it holds the message. Before the
    // seal it asks what it holds (a claim without an id), finds the message it never sent and sends it, once.
    let r = Rig::new().await;
    r.accept("m1", "the lost one", Mode::Next);
    r.hand_out();
    r.store.lose_response(Op::Claim, 3);
    assert_eq!(r.turn_end().await, TurnVote::Done);
    assert_eq!(
        r.store.state("m1"),
        Some(State::Dispatching),
        "TRIGGER PRESENT: the server holds it and the module does not know"
    );
    assert_eq!(r.seal().await, TurnVote::Prompt("the lost one".into()));
    r.sent(3, "s1", "the lost one");
    r.replied(3, "s1");
    r.hook.flush().await;
    assert_eq!(r.store.state("m1"), Some(State::Delivered { turn: 3 }));
    // the notice that was handed out is not claimed and sent a second time
    assert_eq!(r.turn_end().await, TurnVote::Done);
}

#[tokio::test]
async fn at_the_close_a_claim_the_module_never_saw_ends_failed_not_sent_and_not_delivery_unknown() {
    let r = Rig::new().await;
    r.accept("m1", "never seen", Mode::Next);
    r.hand_out();
    r.store.lose_response(Op::Claim, 3);
    assert_eq!(r.turn_end().await, TurnVote::Done);
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::NotSent)),
        "the module knows it never sent it: {:?}",
        r.store.history("m1")
    );
}

#[tokio::test]
async fn a_claim_found_in_an_older_session_is_given_back_and_not_sent() {
    let r = Rig::new().await;
    r.accept("m1", "written for session one", Mode::Next);
    r.hand_out();
    r.store.lose_response(Op::Claim, 3);
    assert_eq!(r.turn_end().await, TurnVote::Done);
    r.session("s2", 2);
    r.hook.flush().await;
    assert_eq!(
        r.seal().await,
        TurnVote::Done,
        "nothing of session one goes into session two"
    );
    assert!(
        r.store.calls().iter().any(|c| c == "release m1"),
        "{:?}",
        r.store.calls()
    );
    assert!(
        r.store.history("m1").contains(&State::Held),
        "{:?}",
        r.store.history("m1")
    );
}

#[tokio::test]
async fn after_the_users_cancel_the_close_asks_no_normal_seal_and_fails_the_unsent_message() {
    let r = Rig::new().await;
    r.accept("m1", "voted, then the user cancelled", Mode::Next);
    r.hand_out();
    assert!(matches!(r.turn_end().await, TurnVote::Prompt(_)));
    r.hook.offer_batch(true, Vec::new());
    r.hook.close().await;
    assert_eq!(
        r.store.state("m1"),
        Some(State::Failed(FailReason::NotSent))
    );
    let seals: Vec<String> = r
        .store
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("seal"))
        .collect();
    assert_eq!(
        seals,
        ["seal s1 terminal"],
        "the terminal seal does not wait, a normal seal would be refused"
    );
}

#[tokio::test]
async fn a_replaced_agentd_stays_out_and_calls_nothing_more() {
    let r = Rig::viewed(1).await;
    r.accept("m1", "for the new agentd", Mode::Next);
    r.store.worker_lost();
    let before = r.store.calls().len();
    r.hand_out();
    assert_eq!(r.turn_end().await, TurnVote::Done);
    assert_eq!(r.seal().await, TurnVote::Done);
    r.hook.close().await;
    assert_eq!(r.store.calls().len(), before, "{:?}", r.store.calls());
    assert_eq!(
        r.store.state("m1"),
        Some(State::Pending),
        "the new agentd's message is untouched"
    );
}
