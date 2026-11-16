//! In the terminal app and in `ham watch`: a session that has gone silent is named with the time nothing was heard since and with
//! what the service does next; a session the service lost is named with the session that starts and the copy it starts from.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::{App, DirectSettings, Msg};
use ham::watch::{InputMemo, view_json};
use ham_core::lost_session::{CAUSE_NO_HEARTBEAT, FROM_COPY, KIND, SessionLost, THEN_RESTART};
use ham_core::state::RunState;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";

fn event(
    seq: u64,
    ts: &str,
    event_type: EventType,
    authority: Authority,
    value: Value,
) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq,
        ts: ts.into(),
        event_type,
        authority,
        payload: Payload::Inline { value },
    }
}

/// A run whose session started at 08:15:10, saved a copy at 08:15:31 and wrote its last event (a beat) at 08:16:48.
fn working() -> Vec<Envelope> {
    let mut events = base_events();
    events.push(event(
        10,
        "2026-10-16T08:15:10Z",
        EventType::TurnStarted,
        Authority::Agent,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct", "session": 1, "beatS": 60}),
    ));
    events.push(event(
        11,
        "2026-10-16T08:15:31Z",
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "savedCopy": {"at": "2026-10-16T08:15:31Z", "count": 1, "headSha": HEAD, "session": 1}}),
    ));
    events.push(event(
        12,
        "2026-10-16T08:16:48Z",
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "alive": {"session": 1}}),
    ));
    events
}

fn lost() -> SessionLost {
    SessionLost {
        kind: KIND.to_string(),
        session: 1,
        decided_at: "2026-10-16T08:26:10Z".into(),
        last_heartbeat_at: Some("2026-10-16T08:16:10Z".into()),
        cause: CAUSE_NO_HEARTBEAT.to_string(),
        silence_s: SESSION_LOST_AFTER_S,
        lost_in_a_row: 1,
        limit: MAX_LOST_SLICES,
        then: THEN_RESTART.to_string(),
        next_session: Some(2),
        from: Some(FROM_COPY.to_string()),
        saved_at: Some("2026-10-16T08:15:31Z".into()),
        delivers: None,
    }
}

fn lost_event() -> Envelope {
    event(
        20,
        "2026-10-16T08:26:10Z",
        EventType::RunWaiting,
        Authority::Harness,
        serde_json::to_value(lost()).unwrap(),
    )
}

fn unix(at: &str) -> i64 {
    parse_utc_unix(at).unwrap()
}

/// The words of a screen as one line of text: the card wraps a long sentence, and a row is padded.
fn flat(screen: &str) -> String {
    screen.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn b842_the_run_screen_says_since_when_nothing_was_heard_and_what_the_service_does_next() {
    let mut app = ready(working());
    // One minute after the last event: a beat may be late, nothing is said.
    app.set_now(unix("2026-10-16T08:17:48Z"));
    let _ = app.update(Msg::Pages(Ok(vec![])));
    assert!(
        !flat(&screen(&app, 160, 50)).contains("No event from"),
        "one beat late is no silence"
    );
    // Five minutes after it.
    app.set_now(unix("2026-10-16T08:21:48Z"));
    let _ = app.update(Msg::Pages(Ok(vec![])));
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains(
            "No event from the run's session since 2026-10-16 08:16 UTC (5 min). When the service receives no heartbeat from a session \
             for 600 seconds, it starts a new session from the last saved copy (saved 2026-10-16 08:15 UTC) when the run has time left; \
             file changes after that copy are lost."
        ),
        "{text}"
    );
}

#[test]
fn b842_the_run_screen_says_which_session_was_lost_and_what_the_new_one_started_from() {
    let mut events = working();
    events.push(lost_event());
    let mut app = ready(events);
    app.set_now(unix("2026-10-16T08:26:30Z"));
    let text = flat(&screen(&app, 160, 50));
    assert!(text.contains(&lost().sentence()), "{text}");
    assert!(
        !text.contains("No event from"),
        "the service has spoken: {text}"
    );
}

fn view_at(events: Vec<Envelope>, now: &str) -> ham_core::state::RunView {
    let mut state = RunState::new(RUN);
    for e in events {
        state.apply(e);
    }
    state.view_at(Some(unix(now)))
}

#[test]
fn b842_ham_watch_prints_the_silence_once_and_the_lost_session_once() {
    let mut memo = InputMemo::new();
    assert!(
        memo.lines(&view_at(working(), "2026-10-16T08:17:00Z"))
            .is_empty()
    );
    let silent = memo.lines(&view_at(working(), "2026-10-16T08:20:00Z"));
    assert_eq!(silent.len(), 1, "{silent:?}");
    assert!(
        silent[0]
            .starts_with("No event from the run's session since 2026-10-16 08:16 UTC (3 min)."),
        "{silent:?}"
    );
    // The same silence a minute later is not printed again.
    assert!(
        memo.lines(&view_at(working(), "2026-10-16T08:21:00Z"))
            .is_empty()
    );

    // The service marks the session lost: its word is printed once.
    let mut events = working();
    events.push(lost_event());
    let told = memo.lines(&view_at(events.clone(), "2026-10-16T08:26:20Z"));
    assert_eq!(told, vec![lost().sentence()]);
    assert!(
        memo.lines(&view_at(events, "2026-10-16T08:26:40Z"))
            .is_empty()
    );
}

#[test]
fn b842_ham_watch_says_when_a_silent_session_answers_again() {
    let mut memo = InputMemo::new();
    assert_eq!(
        memo.lines(&view_at(working(), "2026-10-16T08:20:00Z"))
            .len(),
        1
    );
    let mut events = working();
    events.push(event(
        13,
        "2026-10-16T08:20:30Z",
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "alive": {"session": 1}}),
    ));
    assert_eq!(
        memo.lines(&view_at(events, "2026-10-16T08:20:40Z")),
        vec!["The run's session answers again.".to_string()]
    );
}

#[test]
fn b842_the_json_view_names_the_lost_sessions_and_the_silence_only_when_there_is_one() {
    let plain = view_json(&view_at(working(), "2026-10-16T08:17:00Z"));
    assert!(
        plain.get("lostSessions").is_none() && plain.get("silentSince").is_none(),
        "{plain}"
    );
    let silent = view_json(&view_at(working(), "2026-10-16T08:20:00Z"));
    assert_eq!(silent["silentSince"], "2026-10-16T08:16:48Z");
    let mut events = working();
    events.push(lost_event());
    let after = view_json(&view_at(events, "2026-10-16T08:26:20Z"));
    assert_eq!(after["lostSessions"][0]["session"], 1);
    assert_eq!(after["lostSessions"][0]["nextSession"], 2);
    assert_eq!(after["lostSessions"][0]["savedAt"], "2026-10-16T08:15:31Z");
    assert!(after.get("silentSince").is_none(), "{after}");
}

// ---- the review round ---------------------------------------------------------------------------------------------------------

/// The run on screen with `events` read at the clock `now`: the app knows the time before its first read, as in the event loop.
fn ready_at(events: Vec<Envelope>, now: &str) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(unix(now));
    let _ = app.open_run(RUN);
    feed(&mut app, events);
    let mut clock = Clock::early();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    app
}

#[test]
fn b842_a_client_that_cannot_read_the_run_does_not_call_the_session_silent() {
    // The last good read is 12 s after the session's newest event: nothing is wrong with the session.
    let mut app = ready_at(working(), "2026-10-16T08:17:00Z");
    assert!(!flat(&screen(&app, 160, 50)).contains("No event from"));
    // Ten minutes of clock in which every read fails (the laptop slept, the network or the sign-in is gone): the client has not
    // heard from the service, which says nothing about the session.
    for minute in 1..=10 {
        app.set_now(unix("2026-10-16T08:17:00Z") + 60 * minute);
        let _ = app.update(Msg::Pages(Err("the service did not answer".into())));
        let text = flat(&screen(&app, 160, 50));
        assert!(
            !text.contains("No event from"),
            "after {minute} min without a read the client calls the session silent: {text}"
        );
    }
    // A read that succeeds and brings nothing new: now the silence is the session's.
    let _ = app.update(Msg::Pages(Ok(vec![])));
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("No event from the run's session since 2026-10-16 08:16 UTC (10 min)."),
        "{text}"
    );
}

fn started(seq: u64, ts: &str, session: u32) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnStarted,
        Authority::Agent,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct", "session": session, "beatS": 60}),
    )
}

fn beat(seq: u64, ts: &str, session: u32) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "alive": {"session": session}}),
    )
}

const STARTS: &str = "Session 2 starts from the copy saved at 2026-10-16 08:15 UTC";
const STARTED: &str = "Session 1 was lost: its last heartbeat came at 2026-10-16 08:16 UTC, and the service called it lost at 08:26 UTC after 600 \
                       seconds without one. Session 2 \
                       started from the copy saved at 2026-10-16 08:15 UTC; file changes after that copy were lost.";

#[test]
fn b842_the_status_block_tells_a_lost_session_in_the_past_once_the_next_one_started_and_then_no_more()
 {
    // Before session 2 has started, the conversation and the status block both say that it starts.
    let mut events = working();
    events.push(lost_event());
    let app = ready_at(events.clone(), "2026-10-16T08:26:15Z");
    let before = flat(&screen(&app, 160, 50)).matches(STARTS).count();
    assert!(before >= 1, "the service's word is on the screen");
    // Session 2 started 13 s after the service's word: the status block says so in the past tense.
    events.push(started(21, "2026-10-16T08:26:23Z", 2));
    let app = ready_at(events.clone(), "2026-10-16T08:27:00Z");
    let text = flat(&screen(&app, 160, 50));
    assert!(text.contains(STARTED), "{text}");
    assert_eq!(
        text.matches(STARTS).count(),
        before - 1,
        "the status block still says that session 2 starts: {text}"
    );
    // Seven hours later session 2 still works: the status block says nothing of session 1 any more; the conversation keeps its line.
    events.push(beat(22, "2026-10-16T15:25:30Z", 2));
    let app = ready_at(events, "2026-10-16T15:26:00Z");
    let text = flat(&screen(&app, 160, 50));
    assert!(
        !text.contains("Session 2 started from"),
        "seven hours later the status block still tells the lost session: {text}"
    );
    assert_eq!(text.matches(STARTS).count(), before - 1, "{text}");
}

#[test]
fn b842_the_status_block_drops_a_lost_session_when_another_session_has_started_since() {
    // Session 2 could not restore the copy (the service announces no such failure) and session 3 works.
    let mut events = working();
    events.push(lost_event());
    let app = ready_at(events.clone(), "2026-10-16T08:26:15Z");
    let before = flat(&screen(&app, 160, 50)).matches(STARTS).count();
    events.push(started(21, "2026-10-16T08:26:23Z", 2));
    events.push(event(
        22,
        "2026-10-16T08:26:40Z",
        EventType::TurnEnded,
        Authority::Agent,
        json!({"turn": 1, "outcome": "failed", "directOutcome": "stopped_setup_failed", "label": "Cline did not start",
               "lastCommitSha": null, "stopReason": null}),
    ));
    events.push(started(23, "2026-10-16T08:27:10Z", 3));
    let app = ready_at(events, "2026-10-16T08:27:30Z");
    let text = flat(&screen(&app, 160, 50));
    assert!(
        !text.contains("Session 2 started from"),
        "the status block says that session 2 started from the copy while session 3 works: {text}"
    );
    assert_eq!(
        text.matches(STARTS).count(),
        before - 1,
        "the status block still says that session 2 starts while session 3 works: {text}"
    );
}

#[test]
fn b842_ham_watch_names_a_delivery_at_the_end_only_when_the_service_says_there_is_a_copy() {
    // Three sessions in a row died before any copy: the service's last word says that nothing is delivered.
    let mut third = lost();
    third.session = 3;
    third.lost_in_a_row = 3;
    third.then = "end".to_string();
    third.next_session = None;
    third.from = None;
    third.saved_at = None;
    let mut payload = serde_json::to_value(&third).unwrap();
    payload["delivers"] = json!("nothing");
    let mut events = base_events();
    events.push(started(10, "2026-10-16T08:15:10Z", 3));
    events.push(event(
        20,
        "2026-10-16T08:26:10Z",
        EventType::RunWaiting,
        Authority::Harness,
        payload,
    ));
    let mut memo = InputMemo::new();
    let told = memo.lines(&view_at(events, "2026-10-16T08:26:20Z"));
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].ends_with(
            "It was lost session 3 in a row, so the run ends. The run saved no copy of its work, so there is nothing to deliver."
        ),
        "{told:?}"
    );
}

// ---- the notices about a copy of the work, in line mode --------------------------------------------------------------------------

/// The worker's sentences about a copy of the work (`agentd::direct`: `COPY_LATE_STORE`, `COPY_NOT_SAVED`), as the run's events carry them.
const COPY_LATE: &str =
    "A copy of the work is taking long: storing it has not ended. ham keeps trying.";
const COPY_NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";

fn copy_notice(seq: u64, ts: &str, sentence: &str) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null, "detail": sentence}),
    )
}

/// With: line mode has no conversation, which is where the run screen shows what the run says about its copies. `ham watch`
/// without a terminal tells a silent session, a lost session and each saved copy; a reader of those lines must also learn that a copy
/// is late or could not be saved, because the last saved copy is what a new session starts from. Each sentence is printed once.
#[test]
fn b842_b823_ham_watch_prints_each_notice_about_a_copy_once_beside_the_session_lines() {
    let mut memo = InputMemo::new();
    let mut events = working();
    events.push(copy_notice(13, "2026-10-16T08:17:10Z", COPY_LATE));
    let late = memo.lines(&view_at(events.clone(), "2026-10-16T08:17:20Z"));
    assert_eq!(
        late,
        vec![COPY_LATE.to_string()],
        "TRIGGER PRESENT: the run said that a copy is late"
    );
    // The same notice is not printed again.
    assert!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:17:40Z"))
            .is_empty()
    );
    // The copy is given up: the run says that it could not be saved, and line mode prints that once too.
    events.push(copy_notice(14, "2026-10-16T08:17:50Z", COPY_NOT_SAVED));
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:18:00Z")),
        vec![COPY_NOT_SAVED.to_string()]
    );
    // Another notice of the run (every model route is down) is no line of line mode, as before.
    events.push(event(
        15,
        "2026-10-16T08:18:05Z",
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "kind": "notify", "reason": "outage", "outageMinutes": 30}),
    ));
    assert!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:18:10Z"))
            .is_empty()
    );
    // The session falls silent and is lost: its lines follow, and no notice about a copy is printed again.
    let silent = memo.lines(&view_at(events.clone(), "2026-10-16T08:21:00Z"));
    assert_eq!(silent.len(), 1, "{silent:?}");
    assert!(
        silent[0].starts_with("No event from the run's session since 2026-10-16 08:18 UTC"),
        "{silent:?}"
    );
    events.push(lost_event());
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:26:20Z")),
        vec![lost().sentence()]
    );
    // Session 2 says the same sentence about a copy of its own: that is one more notice, printed once.
    events.push(started(30, "2026-10-16T08:27:00Z", 2));
    events.push(copy_notice(31, "2026-10-16T08:29:00Z", COPY_NOT_SAVED));
    // The watch told that session 1 was lost, so it says first that a session answers again.
    let again = memo.lines(&view_at(events.clone(), "2026-10-16T08:29:10Z"));
    assert_eq!(
        again,
        vec![
            "The run's session answers again.".to_string(),
            COPY_NOT_SAVED.to_string()
        ]
    );
    assert!(
        memo.lines(&view_at(events, "2026-10-16T08:29:30Z"))
            .is_empty()
    );
}

/// A watch that starts on a run whose copies are stored again is told nothing of the trouble that is over: the run said it once for
/// that streak, and a stored copy ended the streak. The next trouble is told.
#[test]
fn b842_b823_ham_watch_that_starts_later_prints_no_notice_about_a_copy_from_before_the_newest_stored_copy()
 {
    let mut events = working();
    events.push(copy_notice(13, "2026-10-16T08:17:10Z", COPY_LATE));
    events.push(copy_notice(14, "2026-10-16T08:17:50Z", COPY_NOT_SAVED));
    assert_eq!(
        InputMemo::new().lines(&view_at(events.clone(), "2026-10-16T08:18:00Z")),
        vec![COPY_LATE.to_string(), COPY_NOT_SAVED.to_string()],
        "TRIGGER PRESENT: a watch that reads the run now is told both"
    );
    // A copy is stored again.
    events.push(event(
        15,
        "2026-10-16T08:18:40Z",
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "savedCopy": {"at": "2026-10-16T08:18:39Z", "count": 2, "headSha": HEAD, "session": 1}}),
    ));
    let mut later = InputMemo::new();
    assert!(
        later
            .lines(&view_at(events.clone(), "2026-10-16T08:19:00Z"))
            .is_empty(),
        "the copies are stored again: nothing of the earlier trouble is printed"
    );
    // The next copy that cannot be saved is told.
    events.push(copy_notice(16, "2026-10-16T08:20:10Z", COPY_NOT_SAVED));
    assert_eq!(
        later.lines(&view_at(events, "2026-10-16T08:20:20Z")),
        vec![COPY_NOT_SAVED.to_string()]
    );
}

/// The JSON view of `ham watch` carries the notices about a copy in one member, `copyNotices`, and is otherwise unchanged by
/// them: it carries the newest stored copy and the session facts. A view without such a notice has no such member.
#[test]
fn b842_b823_the_json_view_of_ham_watch_has_one_member_for_the_notices_about_a_copy() {
    let mut events = working();
    events.push(copy_notice(13, "2026-10-16T08:17:10Z", COPY_LATE));
    let with = view_json(&view_at(events, "2026-10-16T08:17:20Z"));
    let without = view_json(&view_at(working(), "2026-10-16T08:17:20Z"));
    let keys = |v: &Value| -> Vec<String> {
        v.as_object()
            .map(|o| o.keys().filter(|k| *k != "copyNotices").cloned().collect())
            .unwrap_or_default()
    };
    assert!(without.get("copyNotices").is_none(), "{without}");
    assert_eq!(with["copyNotices"][0]["text"], COPY_LATE);
    assert_eq!(keys(&with), keys(&without));
}
