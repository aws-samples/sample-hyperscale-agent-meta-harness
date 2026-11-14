//! With: what the client shows of the notices about the copies of the work in a run whose session falls silent, is lost and
//! is restarted.
//!
//! The worker tells the run when a copy is late and when it could not be saved (`turn.progress`, kind `notify`, reason
//! `copy_not_saved`, the worker's own sentence in `detail`). The service tells the run when a session was lost (`run.waiting`, kind
//! `session-lost`). The client reads both from the run's events:
//!
//! * each notice about a copy is shown once in the status block and once in the conversation, also in a run that lost a session and
//!   whose next session says the same sentence again: that is one more notice, of the new session;
//! * a notice about a copy is word from the session, so a session that says a copy is late is not called silent, and the silence
//!   that follows names the copy that was stored last, not the one that is late.
#![allow(clippy::unwrap_used)]

use ham_core::chat::{BlockKind, ChatInputs, ChatModel};
use ham_core::lost_session::{
    CAUSE_NO_HEARTBEAT, FROM_COPY, KIND, SessionLost, THEN_RESTART, notice_after_s,
};
use ham_core::state::RunState;
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// 2026-11-14T07:00:00Z in Unix seconds.
const T0: i64 = 1_794_639_600;
/// The worker's sentences (`agentd::direct`: `COPY_LATE_STORE`, `COPY_NOT_SAVED`), as the run's events carry them.
const LATE: &str = "A copy of the work is taking long: storing it has not ended. ham keeps trying.";
const NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";

fn ulid(n: u64) -> String {
    format!("01J9ZZZZZZ00000000000000{n:02}")
}

/// `07:MM:SS` on 2026-11-14 as an RFC 3339 UTC time, for a second count after 07:00:00.
fn at(secs: i64) -> String {
    format!("2026-11-14T07:{:02}:{:02}Z", secs / 60, secs % 60)
}

fn ev(n: u64, secs: i64, t: EventType, authority: Authority, payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: ulid(n),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq: n,
        ts: at(secs),
        event_type: t,
        authority,
        payload: Payload::Inline { value: payload },
    }
}

fn h(n: u64, secs: i64, t: EventType, p: Value) -> Envelope {
    ev(n, secs, t, Authority::Harness, p)
}

fn a(n: u64, secs: i64, t: EventType, p: Value) -> Envelope {
    ev(n, secs, t, Authority::Agent, p)
}

fn session_started(n: u64, secs: i64, session: u32) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnStarted,
        json!({"turn": 1, "engine": "cline", "model": "us.anthropic.claude-opus-5-5", "role": "direct", "session": session, "beatS": 60}),
    )
}

fn copy(n: u64, secs: i64, count: u32, session: u32) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnProgress,
        json!({"turn": 1, "savedCopy": {"at": at(secs), "count": count, "headSha": "29a235e2ea242b5e494b8dca989a4ea841132055", "session": session}}),
    )
}

fn copy_notice(n: u64, secs: i64, sentence: &str) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null, "detail": sentence}),
    )
}

/// A run whose first session started at 07:03:10 and saved copy 1 at 07:03:31.
fn working_run() -> Vec<Envelope> {
    vec![
        h(1, 188, EventType::RunCreated, json!({"target": "s3-only"})),
        h(
            2,
            189,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
        h(
            3,
            189,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": "us.anthropic.claude-opus-5-5"}),
        ),
        session_started(4, 190, 1),
        copy(5, 211, 1, 1),
    ]
}

fn lost(session: u32, decided: i64) -> SessionLost {
    SessionLost {
        kind: KIND.to_string(),
        session,
        decided_at: at(decided),
        last_heartbeat_at: Some(at(decided - 600)),
        cause: CAUSE_NO_HEARTBEAT.to_string(),
        silence_s: SESSION_LOST_AFTER_S,
        lost_in_a_row: 1,
        limit: MAX_LOST_SLICES,
        then: THEN_RESTART.to_string(),
        next_session: Some(session + 1),
        from: Some(FROM_COPY.to_string()),
        saved_at: Some(at(211)),
        delivers: None,
    }
}

fn state_of(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

fn count(texts: &[String], sentence: &str) -> usize {
    texts.iter().filter(|t| t.as_str() == sentence).count()
}

/// Session 1 says that a copy is late and then that it could not be saved, falls silent and is lost. Session 2 starts from copy 1 and
/// later says once that a copy could not be saved.
fn run_with_a_lost_session() -> Vec<Envelope> {
    let mut events = working_run();
    events.push(copy_notice(6, 300, LATE));
    events.push(copy_notice(7, 330, NOT_SAVED));
    events.push(h(
        8,
        960,
        EventType::RunWaiting,
        serde_json::to_value(lost(1, 960)).unwrap(),
    ));
    events.push(session_started(9, 990, 2));
    events.push(copy_notice(10, 1_100, NOT_SAVED));
    events.push(copy(11, 1_160, 2, 2));
    events
}

#[test]
fn b842_b823_each_notice_about_a_copy_is_shown_once_in_a_run_that_lost_a_session() {
    let state = state_of(run_with_a_lost_session());
    let v = state.view_at(Some(T0 + 1_170));
    let notices: Vec<String> = v.notices.iter().map(|n| n.text.clone()).collect();
    assert_eq!(count(&notices, LATE), 1, "{notices:?}");
    // One from each session: the restart neither drops the first nor says it again.
    assert_eq!(count(&notices, NOT_SAVED), 2, "{notices:?}");
    // The service's word about the lost session stands beside them, once.
    assert_eq!(v.lost.len(), 1, "{:?}", v.lost);
    // The copy session 2 stored is the second of the run, and it is the newest the view holds.
    let saved = v.saved.as_ref().expect("a saved copy");
    assert_eq!((saved.count, saved.session), (2, 2), "{saved:?}");

    let chat = ChatModel::from_progress_feed(&state, &ChatInputs::default());
    let said: Vec<String> = chat
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Notice(_)))
        .map(|b| b.body.clone())
        .collect();
    assert_eq!(count(&said, LATE), 1, "{said:?}");
    assert_eq!(count(&said, NOT_SAVED), 2, "{said:?}");
    assert_eq!(count(&said, &lost(1, 960).sentence()), 1, "{said:?}");
    // In the order the run said them: late, not saved, the lost session, not saved again.
    let order: Vec<&str> = said
        .iter()
        .map(String::as_str)
        .filter(|t| *t == LATE || *t == NOT_SAVED || t.starts_with("Session 1 was lost"))
        .collect();
    assert_eq!(
        order,
        [LATE, NOT_SAVED, lost(1, 960).sentence().as_str(), NOT_SAVED]
    );
}

#[test]
fn b842_b823_a_notice_about_a_copy_is_word_from_the_session_and_the_silence_names_the_copy_stored_last()
 {
    let mut events = working_run();
    // The session says at 07:05:00 that a copy is late: an event of the session like any other.
    events.push(copy_notice(6, 300, LATE));
    let state = state_of(events);
    let after = i64::try_from(notice_after_s(60)).unwrap();
    // Two beats and half a minute after the copy of 07:03:31 the session is not silent: it spoke at 07:05:00.
    assert!(
        state.view_at(Some(T0 + 211 + after)).silence().is_none(),
        "a session that says a copy is late has been heard"
    );
    assert!(
        state
            .view_at(Some(T0 + 300 + after - 1))
            .silence()
            .is_none()
    );
    let v = state.view_at(Some(T0 + 300 + after));
    let s = v.silence().expect("two beats are missing after the notice");
    assert_eq!(s.since_unix, T0 + 300);
    // A restart would start from the copy that was stored, not from the one that is late.
    assert_eq!(s.saved_at.as_deref(), Some(at(211).as_str()), "{s:?}");
    assert!(
        s.sentence().contains("(saved 2026-11-14 07:03 UTC)"),
        "{}",
        s.sentence()
    );
    // The late notice is still shown, once, beside the silence.
    let notices: Vec<String> = v.notices.iter().map(|n| n.text.clone()).collect();
    assert_eq!(count(&notices, LATE), 1, "{notices:?}");
}

/// A surface without a conversation (`ham watch` without a terminal) has no place where the run's notices stand, so the view keeps the
/// notices about copies apart for it: in the order the run said them, each with its event, and none of the run's other notices.
#[test]
fn b842_b823_the_view_keeps_the_notices_about_copies_for_a_surface_without_a_conversation() {
    let mut events = run_with_a_lost_session();
    // Without the copy session 2 stored at the end: the run still says that a copy could not be saved.
    let stored = events.pop().unwrap();
    assert!(ham_core::saved_copy::parse(&stored).is_some());
    // Another notice of the run: every model route is down.
    events.push(a(
        12,
        1_170,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "notify", "reason": "outage", "outageMinutes": 30}),
    ));
    let v = state_of(events).view_at(Some(T0 + 1_180));
    let texts: Vec<&str> = v.copy_notices.iter().map(|n| n.text.as_str()).collect();
    assert_eq!(
        texts,
        [LATE, NOT_SAVED, NOT_SAVED],
        "the notices about copies, in the order the run said them"
    );
    // Each names its event, so a surface can tell a notice it has printed from a new one with the same words.
    let mut ids: Vec<&str> = v.copy_notices.iter().map(|n| n.event_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "{ids:?}");
    // They stay among the run's notices as well, beside the one about the routes.
    assert_eq!(v.notices.len(), 4, "{:?}", v.notices);
    // A run that said nothing about its copies has none.
    assert!(state_of(working_run()).view().copy_notices.is_empty());
}

/// What the run says about a copy holds until a copy is stored again (the worker says it once for each streak of copies that are
/// late or fail, and a stored copy ends the streak). The view keeps the notices since the newest stored copy, so a surface that starts
/// reading a run later is not told of trouble that is over as if the run still said so. The conversation keeps every notice.
#[test]
fn b842_b823_a_stored_copy_ends_what_the_view_keeps_of_the_notices_about_copies() {
    let mut events = working_run();
    events.push(copy_notice(6, 300, LATE));
    events.push(copy_notice(7, 330, NOT_SAVED));
    let said: Vec<String> = state_of(events.clone())
        .view()
        .copy_notices
        .iter()
        .map(|n| n.text.clone())
        .collect();
    assert_eq!(
        said,
        [LATE, NOT_SAVED],
        "TRIGGER PRESENT: the run says that a copy is late and could not be saved"
    );
    // A copy is stored again.
    events.push(copy(8, 400, 2, 1));
    let state = state_of(events.clone());
    let v = state.view();
    assert!(v.copy_notices.is_empty(), "{:?}", v.copy_notices);
    assert_eq!(v.saved.as_ref().map(|c| c.count), Some(2));
    // The run's notices and the conversation still hold both.
    assert_eq!(v.notices.len(), 2, "{:?}", v.notices);
    let chat = ChatModel::from_progress_feed(&state, &ChatInputs::default());
    let in_chat: Vec<String> = chat.blocks.iter().map(|b| b.body.clone()).collect();
    assert_eq!(count(&in_chat, LATE), 1, "{in_chat:?}");
    assert_eq!(count(&in_chat, NOT_SAVED), 1, "{in_chat:?}");
    // The next trouble is kept again, alone.
    events.push(copy_notice(9, 500, NOT_SAVED));
    let v = state_of(events).view();
    let said: Vec<&str> = v.copy_notices.iter().map(|n| n.text.as_str()).collect();
    assert_eq!(said, [NOT_SAVED]);
}
