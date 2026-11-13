//! The connect check of the tool hook.
//!
//! A silent skip of a broken tool server is a known behaviour of the pinned engine,
//! so the module judges what the engine did
//! instead of what agentd wrote. The clock starts at the first prompt of each session and not at `session/new` (the engine connects at the
//! prompt; `session/new` returns in 5 ms). 30 seconds later the module records one word from what the server saw since the clock started:
//! `none` (nothing arrived), `handshake-only` (an `initialize` and no `tools/list`: the failure of a read-only directory and of a 500 on
//! `initialize` look like this) or `ok`. The summary keeps the worst word over the sessions that were judged.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod hook_support;
mod tools_support;

use agentd::direct::tools::ToolsHook;
use agentd::direct::tools::connect::{Connect, DEFAULT_WINDOW, Watch, classify};
use agentd::direct::tools::settings::Settings;
use agentd::direct::tools::stats::Stats;
use hook_support::{Rig, prompt_sent, session_started};
use serde_json::json;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tools_support::echo::Echo;
use tools_support::{notification, request, rpc, until};

const SEC: Duration = Duration::from_secs(1);

fn seen(initializes: u64, lists: u64) -> Stats {
    Stats {
        initializes,
        lists,
        ..Stats::default()
    }
}

fn hook_with_window(window: Duration) -> ToolsHook {
    ToolsHook::new(Settings {
        port: 0,
        ..Settings::default()
    })
    .with_connect_window(window)
}

/// What the engine's client does on connecting: `initialize`, the `initialized` notification, and (when `list`) `tools/list`.
async fn connect_as_the_engine(addr: SocketAddr, list: bool) {
    let init = request(
        json!(0),
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    assert_eq!(rpc(addr, &init).await.status, 200);
    assert_eq!(
        rpc(addr, &notification("notifications/initialized", json!({})))
            .await
            .status,
        202
    );
    if list {
        assert_eq!(
            rpc(addr, &request(json!(1), "tools/list", json!({})))
                .await
                .status,
            200
        );
    }
}

// ---- the words, from what the server saw -----------------------------------------------------------------------------------------

#[test]
fn the_words_come_from_what_the_server_saw_since_the_clock_started() {
    assert_eq!(classify(&seen(0, 0), &seen(0, 0)), Connect::None);
    assert_eq!(classify(&seen(0, 0), &seen(1, 0)), Connect::HandshakeOnly);
    assert_eq!(classify(&seen(0, 0), &seen(1, 1)), Connect::Ok);
    // A list without an initialize is still a client that listed the tools.
    assert_eq!(classify(&seen(0, 0), &seen(0, 1)), Connect::Ok);
    // What happened before the clock does not count: the first session's connect is not the second session's.
    assert_eq!(classify(&seen(2, 2), &seen(2, 2)), Connect::None);
    assert_eq!(classify(&seen(2, 2), &seen(3, 2)), Connect::HandshakeOnly);
    assert_eq!(classify(&seen(2, 2), &seen(3, 3)), Connect::Ok);
    assert_eq!(classify(&seen(2, 1), &seen(2, 2)), Connect::Ok);
    // The words are the three of the summary, as kebab words.
    let words: Vec<&str> = [Connect::None, Connect::HandshakeOnly, Connect::Ok]
        .iter()
        .map(|c| c.as_str())
        .collect();
    assert_eq!(words, ["none", "handshake-only", "ok"]);
    // Worst first: a session that did not connect is not hidden by one that did.
    assert!(Connect::None < Connect::HandshakeOnly && Connect::HandshakeOnly < Connect::Ok);
}

#[test]
fn twenty_nine_seconds_is_not_yet_a_verdict_and_thirty_is() {
    assert_eq!(DEFAULT_WINDOW, 30 * SEC);
    let mut w = Watch::new(DEFAULT_WINDOW);
    // The first prompt of the session starts the clock at 100 s.
    assert!(w.prompt_sent("s1", 100 * SEC, &seen(0, 0)));
    assert!(w.judge_due(128 * SEC, &seen(0, 0)).is_empty());
    assert!(w.judge_due(129 * SEC, &seen(0, 0)).is_empty(), "29 s");
    assert_eq!(w.worst(), None);
    let due = w.judge_due(130 * SEC, &seen(0, 0));
    assert_eq!(due, [("s1".to_string(), Connect::None)], "30 s");
    assert_eq!(w.worst(), Some(Connect::None));
    // Judged once.
    assert!(w.judge_due(500 * SEC, &seen(5, 5)).is_empty());
    assert_eq!(w.worst(), Some(Connect::None));
}

#[test]
fn only_the_first_prompt_of_a_session_starts_a_clock() {
    let mut w = Watch::new(DEFAULT_WINDOW);
    assert!(w.prompt_sent("s1", 10 * SEC, &seen(0, 0)));
    // A retry, a resume and a nudge in the same session are prompts of that session: no new clock, and the baseline stays.
    assert!(!w.prompt_sent("s1", 20 * SEC, &seen(1, 1)));
    assert!(!w.prompt_sent("s1", 25 * SEC, &seen(1, 1)));
    let due = w.judge_due(40 * SEC, &seen(1, 1));
    assert_eq!(
        due,
        [("s1".to_string(), Connect::Ok)],
        "judged from the baseline of the first prompt"
    );
    // A fresh session has its own clock and its own baseline.
    assert!(w.prompt_sent("s2", 50 * SEC, &seen(1, 1)));
    assert_eq!(
        w.judge_due(80 * SEC, &seen(1, 1)),
        [("s2".to_string(), Connect::None)]
    );
    assert_eq!(
        w.worst(),
        Some(Connect::None),
        "the worst word over the sessions"
    );
}

#[test]
fn a_run_that_ends_inside_the_window_is_judged_only_by_what_is_certain() {
    // s1 started 20 s before the end of the run and listed the tools; s2 started 10 s before it and its baseline already held the first
    // session's counts, so nothing new has arrived for it. A short run is not called a failure: s2 gets no word, s1 is ok.
    let mut w = Watch::new(DEFAULT_WINDOW);
    w.prompt_sent("s1", Duration::ZERO, &seen(0, 0));
    w.prompt_sent("s2", 10 * SEC, &seen(1, 1));
    let got = w.finish(20 * SEC, &seen(1, 1));
    assert_eq!(got, [("s1".to_string(), Connect::Ok)]);
    assert_eq!(w.worst(), Some(Connect::Ok));
    // A clock whose window is over is judged like any other, so the same call at 31 s gives s2 its word.
    let mut w = Watch::new(DEFAULT_WINDOW);
    w.prompt_sent("s1", Duration::ZERO, &seen(0, 0));
    assert_eq!(
        w.finish(31 * SEC, &seen(0, 0)),
        [("s1".to_string(), Connect::None)]
    );
    // And one that has not had its window and saw only an initialize stays unjudged.
    let mut w = Watch::new(DEFAULT_WINDOW);
    w.prompt_sent("s1", Duration::ZERO, &seen(0, 0));
    assert!(w.finish(5 * SEC, &seen(1, 0)).is_empty());
    assert_eq!(w.worst(), None);
}

#[test]
fn an_engine_that_goes_away_before_its_window_is_not_blamed() {
    let mut w = Watch::new(DEFAULT_WINDOW);
    w.prompt_sent("s1", Duration::ZERO, &seen(0, 0));
    w.prompt_sent("s2", 10 * SEC, &seen(0, 0));
    // The first session ends at 5 s having heard nothing: it did not get its window, so it is closed without a word.
    assert!(w.close(Some("s1"), 5 * SEC, &seen(0, 0)).is_empty());
    // The second is still open; the engine's process is stopped at 20 s with a handshake and no list, which is not a failure either.
    assert!(w.close(None, 20 * SEC, &seen(1, 0)).is_empty());
    // Closed clocks are never judged later, whatever the counters say and however late the timer fires.
    assert!(w.judge_due(60 * SEC, &seen(0, 0)).is_empty());
    assert!(w.judge_due(600 * SEC, &seen(9, 9)).is_empty());
    assert_eq!(
        w.worst(),
        None,
        "no word was given for an engine that went away"
    );
    // Closing one session leaves the other open and judged on its own clock.
    let mut w = Watch::new(DEFAULT_WINDOW);
    w.prompt_sent("s1", Duration::ZERO, &seen(0, 0));
    w.prompt_sent("s2", 10 * SEC, &seen(0, 0));
    assert!(w.close(Some("s1"), 5 * SEC, &seen(0, 0)).is_empty());
    assert_eq!(
        w.judge_due(40 * SEC, &seen(0, 0)),
        [("s2".to_string(), Connect::None)]
    );
    // A session that had listed the tools when it ended gets its ok.
    let mut w = Watch::new(DEFAULT_WINDOW);
    w.prompt_sent("s1", Duration::ZERO, &seen(0, 0));
    assert_eq!(
        w.close(Some("s1"), 4 * SEC, &seen(1, 1)),
        [("s1".to_string(), Connect::Ok)]
    );
}

// ---- the same, through the hook, with a real client and a short window -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn connect_classes_come_from_what_the_server_saw() {
    for (class, calls) in [
        ("none", None),
        ("handshake-only", Some(false)),
        ("ok", Some(true)),
    ] {
        let mut rig = Rig::new().await;
        let (echo, _) = Echo::new();
        let hook = hook_with_window(Duration::from_millis(300));
        rig.start(&hook, vec![echo]).await;
        session_started(&hook, "s1", 1);
        prompt_sent(&hook, "s1", 1);
        let first_prompt = Instant::now();
        if let Some(list) = calls {
            connect_as_the_engine(rig.addr(), list).await;
        }
        let judged = until(5 * SEC, || hook.connect().is_some())
            .await
            .unwrap_or_else(|| panic!("{class}: no verdict"));
        let _ = judged;
        assert_eq!(hook.connect().unwrap().as_str(), class);
        assert!(
            first_prompt.elapsed() >= Duration::from_millis(290),
            "{class}: the verdict is the window after the prompt, not before ({:?})",
            first_prompt.elapsed()
        );
        assert!(
            rig.tools_log()
                .iter()
                .any(|l| l.contains(&format!("connect {class}"))),
            "{class}: one log line says it: {:?}",
            rig.tools_log()
        );
        hook.stop_for_tests().await;
        assert_eq!(hook.note().unwrap().connect.as_deref(), Some(class));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_clock_starts_at_the_first_prompt_not_at_session_new() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = hook_with_window(Duration::from_millis(300));
    rig.start(&hook, vec![echo]).await;
    // `session/new` returns long before the engine connects; two windows pass with no prompt.
    session_started(&hook, "s1", 1);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(hook.connect(), None, "no clock, no verdict");
    prompt_sent(&hook, "s1", 1);
    connect_as_the_engine(rig.addr(), true).await;
    assert!(until(5 * SEC, || hook.connect().is_some()).await.is_some());
    assert_eq!(
        hook.connect(),
        Some(Connect::Ok),
        "judged from the prompt, not from session/new"
    );
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_session_connects_again() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = hook_with_window(Duration::from_millis(250));
    rig.start(&hook, vec![echo]).await;
    for (id, number) in [("s1", 1), ("s2", 2)] {
        session_started(&hook, id, number);
        prompt_sent(&hook, id, number);
        connect_as_the_engine(rig.addr(), true).await;
    }
    // Both clocks are judged by their own timers.
    let both = until(5 * SEC, || {
        rig.tools_log()
            .iter()
            .filter(|l| l.contains("connect ok"))
            .count()
            == 2
    })
    .await;
    assert!(both.is_some(), "{:?}", rig.tools_log());
    let stats = hook.stats().unwrap();
    assert_eq!(
        (stats.initializes, stats.lists),
        (2, 2),
        "two sessions, two handshakes, two lists"
    );
    assert_eq!(hook.connect(), Some(Connect::Ok), "the class stays ok");
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_did_not_connect_again_is_not_hidden_by_the_first() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = hook_with_window(Duration::from_millis(250));
    rig.start(&hook, vec![echo]).await;
    session_started(&hook, "s1", 1);
    prompt_sent(&hook, "s1", 1);
    connect_as_the_engine(rig.addr(), true).await;
    assert!(
        until(5 * SEC, || hook.connect() == Some(Connect::Ok))
            .await
            .is_some()
    );
    // The fresh session reads a damaged file and never connects.
    session_started(&hook, "s2", 2);
    prompt_sent(&hook, "s2", 2);
    assert!(
        until(5 * SEC, || hook.connect() == Some(Connect::None))
            .await
            .is_some(),
        "the second session's silence is the word"
    );
    hook.stop_for_tests().await;
    assert_eq!(hook.note().unwrap().connect.as_deref(), Some("none"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_ends_inside_the_window_after_listing_the_tools_is_ok_and_one_that_did_not_list_has_no_word()
 {
    // Listed, and the run is over before 30 s: the connect is known, so it is recorded.
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(Settings {
        port: 0,
        ..Settings::default()
    });
    rig.start(&hook, vec![echo]).await;
    session_started(&hook, "s1", 1);
    prompt_sent(&hook, "s1", 1);
    connect_as_the_engine(rig.addr(), true).await;
    hook.stop_for_tests().await;
    assert_eq!(hook.note().unwrap().connect.as_deref(), Some("ok"));

    // Not listed and the window not over: the module does not call a short run a failure.
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(Settings {
        port: 0,
        ..Settings::default()
    });
    rig.start(&hook, vec![echo]).await;
    session_started(&hook, "s1", 1);
    prompt_sent(&hook, "s1", 1);
    connect_as_the_engine(rig.addr(), false).await;
    hook.stop_for_tests().await;
    assert_eq!(hook.note().unwrap().connect, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_goes_away_inside_the_window_leaves_no_failure_behind() {
    use agentd::direct::hooks::{EngineStop, SessionEnd};
    use hook_support::{engine_stopped, session_ended};
    // Two ways to go away before the window is over and before the engine connected: its session is replaced (the engine's process goes on), and
    // its process is stopped (no session end is raised for a process that died). Each closes the clock without a word, on its own.
    for (way, session_ends) in [("session-ended", true), ("engine-stopped", false)] {
        let mut rig = Rig::new().await;
        let (echo, _) = Echo::new();
        let hook = hook_with_window(Duration::from_millis(300));
        rig.start(&hook, vec![echo]).await;
        session_started(&hook, "s1", 1);
        prompt_sent(&hook, "s1", 1);
        if session_ends {
            session_ended(&hook, "s1", 1, SessionEnd::Replaced);
        } else {
            engine_stopped(&hook, EngineStop::Restarted);
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            hook.connect(),
            None,
            "{way}: the timer of a closed clock gives no word"
        );
        assert!(
            !rig.tools_log().iter().any(|l| l.contains("connect none")),
            "{way}: {:?}",
            rig.tools_log()
        );
        // The fresh session that follows is judged on its own.
        session_started(&hook, "s2", 2);
        prompt_sent(&hook, "s2", 2);
        assert!(
            until(5 * SEC, || hook.connect().is_some()).await.is_some(),
            "{way}"
        );
        assert_eq!(hook.connect(), Some(Connect::None), "{way}");
        hook.stop_for_tests().await;
    }
}
