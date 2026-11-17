//! What a module is told at a turn end about an engine that works by itself (`TurnEnd.engine_working`, `src/direct/hooks.rs`).
//! A module that votes `Wait` says the engine is still working on its own (the goal loop's follow-up rounds) and no prompt of the harness
//! is open. A module asked after it, in the same poll, must know: a prompt it votes now goes to a busy engine, which takes the prompt
//! behind the round it is working on. No AWS, no engine: the modules here are fakes.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::acp::Watched;
use agentd::direct::classify::{Classified, PromptDone, RawEnd, classify};
use agentd::direct::hooks::{DirectHook, Hooks, TurnEnd, TurnVote};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Seen = Arc<Mutex<Vec<(String, bool)>>>;

/// A fake module: notes what it was told about the engine, and votes what the test set.
struct Sees {
    name: &'static str,
    seen: Seen,
    vote: Mutex<TurnVote>,
}

impl Sees {
    fn new(name: &'static str, seen: &Seen, vote: TurnVote) -> Arc<Self> {
        Arc::new(Self {
            name,
            seen: seen.clone(),
            vote: Mutex::new(vote),
        })
    }

    fn set(&self, vote: TurnVote) {
        *self.vote.lock().unwrap() = vote;
    }
}

impl DirectHook for Sees {
    fn name(&self) -> &'static str {
        self.name
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push((format!("{}:turn", self.name), end.engine_working));
            self.vote.lock().unwrap().clone()
        })
    }

    fn on_seal<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push((format!("{}:seal", self.name), end.engine_working));
            self.vote.lock().unwrap().clone()
        })
    }
}

fn answered() -> RawEnd {
    RawEnd {
        watched: Watched::Done(Ok(PromptDone {
            stop_reason: "end_turn".into(),
            usage: None,
        })),
        cancel_sent: false,
        message: "All done.".into(),
        setup_error: None,
    }
}

async fn ask(h: &Hooks, seal: bool) -> TurnVote {
    let raw = answered();
    let classified: Classified = classify(&raw);
    let end = TurnEnd {
        n: 1,
        session_id: Some("s1"),
        session: 1,
        raw: &raw,
        classified: &classified,
        files_changed: Some(2),
        interrupted: false,
        engine_working: false,
        time_left: Duration::from_secs(600),
        tool_calls: 3,
    };
    if seal {
        h.seal(&end).await
    } else {
        h.turn_end(&end).await
    }
}

fn wait() -> TurnVote {
    TurnVote::Wait {
        until: Duration::from_millis(20),
    }
}

fn took(seen: &Seen) -> Vec<(String, bool)> {
    std::mem::take(&mut *seen.lock().unwrap())
}

fn told(name: &str, working: bool) -> (String, bool) {
    (name.to_string(), working)
}

#[tokio::test]
async fn b857_a_module_asked_after_a_wait_is_told_the_engine_works_by_itself() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let before = Sees::new("before", &seen, TurnVote::Done);
    let waiter = Sees::new("waiter", &seen, wait());
    let after = Sees::new(
        "after",
        &seen,
        TurnVote::Prompt("the user's message".into()),
    );
    let h = Hooks::new(vec![
        before.clone() as Arc<dyn DirectHook>,
        waiter.clone() as Arc<dyn DirectHook>,
        after.clone() as Arc<dyn DirectHook>,
    ]);
    // The prompt of a later module still replaces the wait: the message goes out while the engine works.
    assert_eq!(
        ask(&h, false).await,
        TurnVote::Prompt("the user's message".into())
    );
    assert_eq!(
        took(&seen),
        [
            told("before:turn", false),
            told("waiter:turn", false),
            told("after:turn", true)
        ],
        "only the module asked after the wait is told"
    );
}

#[tokio::test]
async fn b857_a_poll_in_which_nobody_waits_tells_nobody_and_the_word_does_not_outlive_its_poll() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let waiter = Sees::new("waiter", &seen, wait());
    let after = Sees::new("after", &seen, TurnVote::Done);
    let h = Hooks::new(vec![
        waiter.clone() as Arc<dyn DirectHook>,
        after.clone() as Arc<dyn DirectHook>,
    ]);
    assert_eq!(ask(&h, false).await, wait());
    assert_eq!(
        took(&seen),
        [told("waiter:turn", false), told("after:turn", true)]
    );
    // The loop is over: the same module has no opinion at the next ask, and the module after it is told nothing.
    waiter.set(TurnVote::Done);
    assert_eq!(ask(&h, false).await, TurnVote::Done);
    assert_eq!(
        took(&seen),
        [told("waiter:turn", false), told("after:turn", false)]
    );
}

#[tokio::test]
async fn b857_at_the_seal_a_wait_is_no_opinion_and_no_module_is_told_the_engine_works() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let waiter = Sees::new("waiter", &seen, wait());
    let after = Sees::new("after", &seen, TurnVote::Done);
    let h = Hooks::new(vec![
        waiter.clone() as Arc<dyn DirectHook>,
        after.clone() as Arc<dyn DirectHook>,
    ]);
    assert_eq!(ask(&h, true).await, TurnVote::Done);
    assert_eq!(
        took(&seen),
        [told("waiter:seal", false), told("after:seal", false)]
    );
}
