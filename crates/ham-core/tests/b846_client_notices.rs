//! The small points about what the client says that the live proofs of two releases left, at the level of the view.
//!
//! * The sentence about a silent session names the moment it measures from for what it is: the newest event of the run that was read.
//!   The sentence about a lost session names the other moment, the last heartbeat the service received; the two can be a minute apart.
//! * The card's `Lost` row and the past-tense sentence of the run screen name the last heartbeat and the time the service called the
//!   session lost, which is ten minutes later.
//! * The questions of a run are numbered across its sessions: a new worker starts its own count at 1.
//! * A stored conversation whose last session closed it does not end with the notice of an open tail when the open tail of the earlier
//!   session was said where it happened.
//! * A run that goes on without a session that answers says so beside its status: its session was asked to end, is silent, or the run
//!   waits for the session the service starts.
//! * The run's word that its session was asked to end is kept in the view, with what is true about the copies and the open question.
//! * The tokens of the prompts that answered are added up from the stored conversation, and the card has a row for the tokens the
//!   engine used outside them.
#![allow(clippy::unwrap_used)]

use ham_core::chat::GapCause;
use ham_core::lost_session::{
    CAUSE_NO_HEARTBEAT, FROM_COPY, KIND, SessionLost, THEN_RESTART, notice_after_s,
};
use ham_core::render::{CardUsage, FinalCard, final_card};
use ham_core::state::{RunState, SessionGap};
use ham_core::stored::{Read, StoredChat};
use ham_core::transcript::{
    CompletenessEntry, Dir, GapKind, TranscriptPage, TranscriptRecord, TranscriptState,
};
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// 2026-11-14T07:00:00Z in Unix seconds.
const T0: i64 = 1_794_639_600;
/// The worker's sentence for a request to end whose copy could not be saved (`agentd::direct::STOP_COPY_NOT_SAVED`).
const STOP: &str = "The session was asked to end, and a copy of the work could not be saved.";
const NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";
const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";

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

fn started(n: u64, secs: i64, session: u32) -> Envelope {
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
        json!({"turn": 1, "savedCopy": {"at": at(secs), "count": count, "headSha": HEAD, "session": session}}),
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

/// A question of the run. `own` is the number the worker gave it: every worker starts its own count at 1.
fn question(n: u64, secs: i64, own: u32) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnProgress,
        json!({
            "turn": 1, "kind": "needs-input", "text": "Needs your input: Cline ended without changing files",
            "requestId": ulid(n), "n": own, "reason": "no_changes", "title": "Cline ended without changing files",
            "stopOutcome": "stopped_no_changes",
            "evidence": {"attempts": 1, "toolCalls": 0, "filesChanged": 0, "identicalCalls": null, "failedTurns": null,
                         "lastError": null, "runS": 30, "sessions": 1},
            "lastMessage": "Which design do you want?", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "finish"], "models": [], "recommended": null, "canContinue": true,
            "waitS": 1800, "expiresAt": at(secs + 1800)
        }),
    )
}

/// A run whose first session started at 07:03:10.
fn begun() -> Vec<Envelope> {
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
        started(4, 190, 1),
    ]
}

/// The service's word that `session` was lost at `decided`, with the last heartbeat it holds and what the next session starts from.
fn lost(session: u32, decided: i64, heartbeat: Option<i64>, saved: Option<i64>) -> SessionLost {
    SessionLost {
        cancelled_at: None,
        kind: KIND.to_string(),
        session,
        decided_at: at(decided),
        last_heartbeat_at: heartbeat.map(at),
        cause: CAUSE_NO_HEARTBEAT.to_string(),
        silence_s: SESSION_LOST_AFTER_S,
        lost_in_a_row: 1,
        limit: MAX_LOST_SLICES,
        then: THEN_RESTART.to_string(),
        next_session: Some(session + 1),
        from: Some(if saved.is_some() { FROM_COPY } else { "start" }.to_string()),
        saved_at: saved.map(at),
        delivers: None,
        ended_at: None,
        recovery: None,
        read_back_session: None,
        run_id: String::new(),
    }
}

fn lost_event(n: u64, l: &SessionLost, secs: i64) -> Envelope {
    h(
        n,
        secs,
        EventType::RunWaiting,
        serde_json::to_value(l).unwrap(),
    )
}

fn state_of(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

// ---- the two last moments of a session (point 4) and the date of a loss (point 5) -------------------------------------------------

/// The silence is measured from the newest event of the run that was read, and the sentence says so. The sentence of the lost session
/// names the last heartbeat the service received, 55 s later in the proof: two moments, each named for what it is.
#[test]
fn b846_the_silence_sentence_names_the_newest_event_and_the_lost_sentence_the_last_heartbeat() {
    let mut events = begun();
    events.push(copy(5, 211, 1, 1));
    // The newest event of the session is written at 07:09:58; its last heartbeat reaches the service at 07:10:53.
    events.push(copy_notice(6, 598, STOP));
    let quiet_after = i64::try_from(notice_after_s(60)).unwrap();
    let v = state_of(events.clone()).view_at(Some(T0 + 598 + quiet_after + 5));
    let silence = v
        .silence()
        .expect("TRIGGER PRESENT: the session is silent")
        .sentence();
    assert!(
        silence.starts_with(
            "No event from the run's session since 2026-11-14 07:09 UTC (2 min). When the service receives no heartbeat from a \
             session for 600 seconds, it starts a new session from the last saved copy (saved 2026-11-14 07:03 UTC)"
        ),
        "{silence}"
    );
    assert!(!silence.contains("No word"), "{silence}");
    let l = lost(1, 653 + 600, Some(653), Some(211));
    assert_eq!(
        l.sentence(),
        "Session 1 was lost: the service received no heartbeat from it for 600 seconds (the last one came at 2026-11-14 07:10 UTC). \
         Session 2 starts from the copy saved at 2026-11-14 07:03 UTC; file changes after that copy are lost."
    );
}

/// The card's row and the past-tense sentence of the run screen dated the loss at the service's decision, ten minutes after the last
/// heartbeat. They name both times: the heartbeat's and the decision's.
#[test]
fn b846_the_card_row_and_the_past_tense_sentence_name_the_last_heartbeat_and_the_decision() {
    let l = lost(1, 653 + 600, Some(653), Some(211));
    assert_eq!(
        l.card_row(),
        "Lost      session 1: last heartbeat 2026-11-14 07:10 UTC, called lost 07:20 UTC; session 2 started from the copy saved at \
         2026-11-14 07:03 UTC; file changes after that copy were lost"
    );
    assert_eq!(
        l.sentence_started(),
        "Session 1 was lost: its last heartbeat came at 2026-11-14 07:10 UTC, and the service called it lost at 07:20 UTC after 600 \
         seconds without one. Session 2 started from the copy saved at 2026-11-14 07:03 UTC; file changes after that copy were lost."
    );
    // The card of a finished run carries the row.
    let mut events = begun();
    events.push(copy(5, 211, 1, 1));
    events.push(lost_event(6, &l, 1_253));
    let card = final_card(&state_of(events).view(), &FinalCard::default());
    assert!(card.contains(&l.card_row()), "{card}");
    assert!(!card.contains("session 1 at "), "{card}");
}

/// A service that holds no heartbeat of the session gives the decision's time alone, named as the decision; a decision on another day
/// than the heartbeat is written with its date.
#[test]
fn b846_a_loss_without_a_heartbeat_time_is_dated_by_the_decision_named_as_such() {
    let l = lost(2, 1_300, None, None);
    assert_eq!(
        l.card_row(),
        "Lost      session 2, called lost 2026-11-14 07:21 UTC; session 3 started from the files the run started with; the work of \
         session 2 was lost"
    );
    assert_eq!(
        l.sentence_started(),
        "Session 2 was lost: the service called it lost at 2026-11-14 07:21 UTC after 600 seconds without a heartbeat. Session 3 \
         started from the files the run started with: no copy had been saved, so the work of session 2 was lost."
    );
    let mut over_midnight = lost(1, 1_253, Some(653), Some(211));
    over_midnight.last_heartbeat_at = Some("2026-11-14T23:55:10Z".into());
    over_midnight.decided_at = "2026-11-15T00:05:11Z".into();
    assert!(
        over_midnight.card_row().starts_with(
            "Lost      session 1: last heartbeat 2026-11-14 23:55 UTC, called lost 2026-11-15 00:05 UTC; "
        ),
        "{}",
        over_midnight.card_row()
    );
}

// ---- the questions of a run are numbered across its sessions (point 10) --------------------------------------------------------------

/// Session 1 asks its first question and is lost; session 2 asks its first and its second; session 3 asks its first. Every worker
/// numbered its own from 1. The run's view numbers them 1, 2, 3, 4.
#[test]
fn b846_the_questions_of_a_run_are_numbered_across_its_sessions() {
    let mut events = begun();
    events.push(question(5, 220, 1));
    let v = state_of(events.clone()).view();
    assert_eq!(v.input.open.as_ref().map(|q| q.n), Some(1));
    events.push(lost_event(6, &lost(1, 900, Some(300), None), 900));
    events.push(started(7, 905, 2));
    events.push(question(8, 930, 1));
    let v = state_of(events.clone()).view();
    assert_eq!(
        v.input
            .history
            .iter()
            .map(|q| (q.n, q.how.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "session-lost")],
        "TRIGGER PRESENT: the first question was closed when its session was lost"
    );
    assert_eq!(
        v.input.open.as_ref().map(|q| q.n),
        Some(2),
        "the second question of the run is number 2, whatever number its worker gave it"
    );
    // Session 2 asks again, with its own number 2; a later session starts over at 1.
    events.push(question(9, 960, 2));
    events.push(lost_event(10, &lost(2, 1_700, Some(1_100), None), 1_700));
    events.push(started(11, 1_705, 3));
    events.push(question(12, 1_730, 1));
    let v = state_of(events).view();
    let closed: Vec<u32> = v.input.history.iter().map(|q| q.n).collect();
    assert_eq!(closed, vec![1, 2, 3], "{:?}", v.input.history);
    assert_eq!(v.input.open.as_ref().map(|q| q.n), Some(4));
}

/// A run of one session keeps the numbers its worker gave.
#[test]
fn b846_a_run_of_one_session_keeps_the_numbers_of_its_worker() {
    let mut events = begun();
    events.push(question(5, 220, 1));
    events.push(question(6, 260, 2));
    let v = state_of(events).view();
    assert_eq!(
        v.input.history.iter().map(|q| q.n).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(v.input.open.as_ref().map(|q| q.n), Some(2));
}

// ---- a run without a session that answers (points 6 and 8) ---------------------------------------------------------------------------

/// A run stays `active (green)` while its session is asked to end, silent or lost. The view says which it is, until the next session
/// has started.
#[test]
fn b846_a_run_whose_session_does_not_answer_says_what_it_waits_for() {
    let mut events = begun();
    events.push(copy(5, 211, 1, 1));
    let v = state_of(events.clone()).view_at(Some(T0 + 230));
    assert_eq!(
        v.session_gap_at(Some(T0 + 230)),
        None,
        "a session that works"
    );
    // The worker says that it was asked to end.
    events.push(copy_notice(6, 598, STOP));
    let now = T0 + 610;
    let v = state_of(events.clone()).view_at(Some(now));
    assert_eq!(v.session_gap_at(Some(now)), Some(SessionGap::Ending));
    assert_eq!(SessionGap::Ending.words(), "session asked to end");
    // Two beats and half a minute later it is silent.
    let now = T0 + 598 + i64::try_from(notice_after_s(60)).unwrap() + 1;
    let v = state_of(events.clone()).view_at(Some(now));
    assert_eq!(v.session_gap_at(Some(now)), Some(SessionGap::Silent));
    assert_eq!(SessionGap::Silent.words(), "session silent");
    // The service calls it lost: the run waits for session 2.
    events.push(lost_event(7, &lost(1, 1_253, Some(653), Some(211)), 1_253));
    let now = T0 + 1_255;
    let v = state_of(events.clone()).view_at(Some(now));
    assert_eq!(v.session_gap_at(Some(now)), Some(SessionGap::Awaiting(2)));
    assert_eq!(SessionGap::Awaiting(2).words(), "waiting for session 2");
    // Session 2 has started: nothing is missing.
    events.push(started(8, 1_257, 2));
    let now = T0 + 1_260;
    let v = state_of(events.clone()).view_at(Some(now));
    assert_eq!(v.session_gap_at(Some(now)), None);
    // A run that has ended waits for nothing.
    events.push(h(
        9,
        1_300,
        EventType::GoalStatus,
        json!({"status": "blocked", "phase": "green"}),
    ));
    let v = state_of(events).view_at(Some(T0 + 2_000));
    assert_eq!(v.session_gap_at(Some(T0 + 2_000)), None);
}

/// The run's word that its session was asked to end is kept until the service calls the session lost or a later session starts. The
/// note beside it says what is true about the copies: the last saved copy is from before the request.
#[test]
fn b846_the_request_to_end_is_kept_with_the_copy_from_before_it() {
    let mut events = begun();
    events.push(copy(5, 211, 1, 1));
    assert_eq!(state_of(events.clone()).view().end_asked, None);
    assert_eq!(state_of(events.clone()).view().end_request_note(), None);
    // Another notice about a copy is no request to end.
    events.push(copy_notice(6, 400, NOT_SAVED));
    assert_eq!(state_of(events.clone()).view().end_asked, None);
    events.push(copy_notice(7, 598, STOP));
    let v = state_of(events.clone()).view();
    assert_eq!(v.end_asked.as_deref(), Some("2026-11-14T07:09:58Z"));
    assert_eq!(
        v.end_request_note().as_deref(),
        Some(
            "The session was asked to end at 2026-11-14 07:09 UTC. The last saved copy is from before that request: 2026-11-14 07:03 \
             UTC; ham pull 01JABCDEFGHJKMNPQRSTVWXYZ0 --snapshot downloads it."
        )
    );
    // The service's word about the session takes its place.
    events.push(lost_event(8, &lost(1, 1_253, Some(653), Some(211)), 1_253));
    let v = state_of(events.clone()).view();
    assert_eq!(v.end_asked, None);
    assert_eq!(v.end_request_note(), None);
}

/// A run that waited for an answer when its session was asked to end had changed no file and saved no copy. The note says that no copy
/// existed, and that the open question belongs to the session that was asked to end.
#[test]
fn b846_a_request_to_end_while_a_question_is_open_says_whose_question_it_is() {
    let mut events = begun();
    events.push(question(5, 220, 1));
    events.push(copy_notice(6, 300, STOP));
    let v = state_of(events.clone()).view();
    assert!(
        v.input.waiting().is_some(),
        "TRIGGER PRESENT: the question is still open"
    );
    assert_eq!(
        v.end_request_note().as_deref(),
        Some(
            "The session was asked to end at 2026-11-14 07:05 UTC. No copy of the work had been saved before that request: a run \
             saves a copy only after its files change. The open question was asked by that session: once it has ended, no answer \
             reaches it. When the run has time left, a new session starts and asks again when it needs your answer."
        )
    );
    assert_eq!(v.session_gap_at(Some(T0 + 310)), Some(SessionGap::Ending));
    // A new session started: the request to end was the earlier session's.
    events.push(lost_event(7, &lost(1, 900, Some(300), None), 900));
    events.push(started(8, 905, 2));
    events.push(question(9, 930, 1));
    let v = state_of(events).view();
    assert_eq!(v.end_asked, None);
    assert_eq!(v.end_request_note(), None);
}

/// The sentence about the open question of a session that was asked to end promises what the service does, as the notice of a silent
/// session does. A new session starts only when the run has time left, and none starts when the session would be the third lost in a
/// row: the run ends then, and no session asks again.
#[test]
fn b846_the_sentence_about_the_open_question_promises_a_new_session_only_when_the_service_starts_one()
 {
    // Sessions 1 and 2 were lost in a row; session 3 asks a question and is then asked to end.
    let mut events = begun();
    events.push(lost_event(5, &lost(1, 900, Some(300), None), 900));
    events.push(started(6, 905, 2));
    let mut second = lost(2, 1_700, Some(1_100), None);
    second.lost_in_a_row = 2;
    events.push(lost_event(7, &second, 1_700));
    events.push(started(8, 1_705, 3));
    events.push(question(9, 1_730, 1));
    events.push(copy_notice(10, 1_790, STOP));
    let v = state_of(events).view();
    assert_eq!(
        v.lost_in_a_row,
        MAX_LOST_SLICES - 1,
        "TRIGGER PRESENT: one more lost session in a row ends the run"
    );
    assert!(
        v.input.waiting().is_some(),
        "TRIGGER PRESENT: the question of session 3 is open"
    );
    let note = v.end_request_note().unwrap();
    assert!(
        note.ends_with(
            "The open question was asked by that session: once it has ended, no answer reaches it. It would be lost session 3 in \
             a row, so the run would end and no new session would ask again."
        ),
        "{note}"
    );
    assert!(!note.contains("a new session starts"), "{note}");
    assert!(!note.contains("a new session asks again"), "{note}");
}

/// The worker tries the copy of a request to end again every few seconds while its process runs on. A copy that is stored after the
/// run's word that its session was asked to end is no copy "from before that request": the note says that it was saved after it.
#[test]
fn b846_a_copy_stored_after_the_request_to_end_is_not_called_a_copy_from_before_it() {
    let mut events = begun();
    events.push(copy(5, 211, 1, 1));
    events.push(copy_notice(6, 598, STOP));
    let v = state_of(events.clone()).view();
    assert!(
        v.end_request_note()
            .is_some_and(|n| n
                .contains("The last saved copy is from before that request: 2026-11-14 07:03 UTC")),
        "TRIGGER PRESENT: the request to end stands beside the copy from before it: {:?}",
        v.end_request_note()
    );
    // Five seconds later the worker's next try stores a copy.
    events.push(copy(7, 603, 2, 1));
    let v = state_of(events.clone()).view();
    assert!(
        v.copy_notices.is_empty(),
        "TRIGGER PRESENT: a stored copy ends what the run said about its copies"
    );
    assert_eq!(
        v.saved.as_ref().map(|c| c.at.as_str()),
        Some("2026-11-14T07:10:03Z"),
        "TRIGGER PRESENT: the view holds the copy stored after the request"
    );
    assert_eq!(
        v.end_asked.as_deref(),
        Some("2026-11-14T07:09:58Z"),
        "the session is still asked to end"
    );
    assert_eq!(
        v.end_request_note().as_deref(),
        Some(
            "The session was asked to end at 2026-11-14 07:09 UTC. A copy of the work was saved after that request: 2026-11-14 \
             07:10 UTC; ham pull 01JABCDEFGHJKMNPQRSTVWXYZ0 --snapshot downloads it."
        )
    );
    assert_eq!(v.session_gap_at(Some(T0 + 610)), Some(SessionGap::Ending));
    // A request to end that comes after a stored copy has that copy from before it again.
    events.push(copy_notice(8, 640, STOP));
    let v = state_of(events).view();
    assert_eq!(
        v.end_request_note().as_deref(),
        Some(
            "The session was asked to end at 2026-11-14 07:10 UTC. The last saved copy is from before that request: 2026-11-14 \
             07:10 UTC; ham pull 01JABCDEFGHJKMNPQRSTVWXYZ0 --snapshot downloads it."
        )
    );
}

/// A session whose copies fail from its first change says so, is asked to end, and the copy of that request fails too. The run has
/// no copy, and files changed: the note does not explain the missing copy by unchanged files.
#[test]
fn b846_a_request_to_end_after_failed_copies_does_not_say_that_no_file_had_changed() {
    let mut events = begun();
    events.push(copy_notice(5, 400, NOT_SAVED));
    events.push(copy_notice(6, 598, STOP));
    let v = state_of(events).view();
    assert_eq!(v.saved, None, "TRIGGER PRESENT: the run saved no copy");
    assert_eq!(
        v.copy_notices
            .iter()
            .map(|n| n.text.as_str())
            .collect::<Vec<_>>(),
        vec![NOT_SAVED, STOP],
        "TRIGGER PRESENT: the run said that a copy could not be saved before the request to end"
    );
    assert_eq!(
        v.end_request_note().as_deref(),
        Some(
            "The session was asked to end at 2026-11-14 07:09 UTC. No copy of the work has been saved in this run."
        )
    );
}

/// The words the client reads the request to end by are the first words of the worker's own sentence.
#[test]
fn b846_the_words_of_a_request_to_end_are_the_workers() {
    let worker = include_str!("../../agentd/src/direct/mod.rs");
    assert!(
        worker.contains(&format!("\"{STOP}\"")),
        "the worker's sentence for a request to end changed: change ham_core::notice::END_REQUEST_WORDS with it"
    );
    assert!(STOP.starts_with(ham_core::notice::END_REQUEST_WORDS));
    assert!(ham_core::notice::is_end_request(STOP));
    assert!(!ham_core::notice::is_end_request(NOT_SAVED));
}

// ---- the stored conversation of a run with a lost session (point 9) -----------------------------------------------------------------

const INC1: &str = "01M5A2R7K3C9XJ4D8E6F000001";
const INC2: &str = "01M5A2R7K3C9XJ4D8E6F000002";

fn rec(seq: u64, inc: &str, n: u64, kind: &str, data: Value) -> TranscriptRecord {
    TranscriptRecord {
        seq,
        n,
        incarnation: inc.to_string(),
        origin: ham_core::transcript::Origin::Engine,
        t: Some(format!("2026-11-14T07:{:02}:00Z", 3 + seq)),
        dir: Dir::In,
        kind: kind.to_string(),
        data,
    }
}

fn msg(seq: u64, inc: &str, n: u64, text: &str) -> TranscriptRecord {
    rec(
        seq,
        inc,
        n,
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": format!("m{seq}"), "content": {"type": "text", "text": text}}),
    )
}

fn page(
    state: TranscriptState,
    records: Vec<TranscriptRecord>,
    completeness: Vec<CompletenessEntry>,
) -> TranscriptPage {
    TranscriptPage {
        last_session_invoked_at: None,
        run_id: RUN.into(),
        state,
        records,
        gaps: vec![],
        completeness,
        published_through: None,
        listed_through: None,
        next: None,
        prev: None,
        more: false,
        skipped_keys: 0,
    }
}

fn open_tail() -> CompletenessEntry {
    CompletenessEntry {
        kind: GapKind::TailUnknown,
        from_seq: None,
        to_seq: None,
        count: None,
    }
}

fn stored(p: TranscriptPage) -> StoredChat {
    let mut c = StoredChat::new();
    c.absorb(&p, Read::First);
    c
}

fn bodies(p: TranscriptPage) -> Vec<String> {
    stored(p).blocks().into_iter().map(|b| b.body).collect()
}

/// The bodies of the conversation of a run whose events say that its last session began at `began` (Unix seconds).
fn bodies_for(p: TranscriptPage, began: Option<i64>) -> Vec<String> {
    stored(p)
        .blocks_for(began)
        .into_iter()
        .map(|b| b.body)
        .collect()
}

/// The records of a run whose session 1 was lost with its conversation open and whose session 2 said so in its first record (07:04)
/// and closed its own conversation (the `end` record of 07:05).
fn lost_then_closed() -> Vec<TranscriptRecord> {
    vec![
        msg(0, INC1, 0, "session one works"),
        rec(
            1,
            INC2,
            0,
            "gap",
            json!({"kind": "tail-unknown", "afterSeq": 0, "reason": "a previous writer ended without a final flush"}),
        ),
        msg(1, INC2, 1, "session two goes on"),
        rec(2, INC2, 2, "end", json!({})),
    ]
}

const EARLIER: &str =
    "[an earlier session stopped without closing its conversation: its last lines may be missing]";

/// Session 1 was lost with its conversation open; session 2 said so in its first record and closed its own conversation. The read
/// route of a run that ended without a summary of the conversation still names an open tail with no chunk. The end of the screen showed
/// "the conversation was closed cleanly" and, on the next line, that the last session's conversation was not closed. The run's events
/// say that session 2, which began at 07:04, is the run's last: the `end` record of 07:05 is its own.
#[test]
fn b846_a_conversation_its_last_session_closed_does_not_end_with_the_notice_of_an_open_tail() {
    let shown = bodies_for(
        page(
            TranscriptState::Incomplete,
            lost_then_closed(),
            vec![open_tail()],
        ),
        Some(T0 + 240),
    );
    assert_eq!(
        shown.iter().filter(|b| b.starts_with(EARLIER)).count(),
        1,
        "TRIGGER PRESENT: the open tail of session 1 is said where it happened: {shown:#?}"
    );
    assert_eq!(
        shown.last().map(String::as_str),
        Some("the conversation was closed cleanly"),
        "{shown:#?}"
    );
    assert!(
        !shown.iter().any(|b| b == GapCause::TailUnknown.words()),
        "the last session closed its conversation: {shown:#?}"
    );
}

/// A run of one session: the session closed its conversation, and the run ended without a summary of it (the worker stopped after
/// its last record), so the read route declares an open tail. The screen showed "the conversation was closed cleanly" and, on the
/// next line, that the last session's conversation was not closed. The `end` record of 07:04 is the run's only session's own: its
/// conversation was closed, with or without an earlier lost session.
#[test]
fn b846_a_conversation_closed_by_the_runs_only_session_has_no_notice_of_an_open_tail() {
    let shown = bodies_for(
        page(
            TranscriptState::Incomplete,
            vec![
                msg(0, INC1, 0, "session one works"),
                rec(1, INC1, 1, "end", json!({})),
            ],
            vec![open_tail()],
        ),
        Some(T0 + 190),
    );
    assert!(
        !shown.iter().any(|b| b.starts_with(EARLIER)),
        "TRIGGER PRESENT: the run lost no session: {shown:#?}"
    );
    assert_eq!(
        shown.last().map(String::as_str),
        Some("the conversation was closed cleanly"),
        "{shown:#?}"
    );
    assert!(
        !shown.iter().any(|b| b == GapCause::TailUnknown.words()),
        "the run's only session closed its conversation: {shown:#?}"
    );
}

/// The notice stays where it is true or may be true: the last record is no end of a conversation, or the end cannot be tied to the
/// run's last session (here nothing is known of the run's sessions: a last session that stored nothing may have followed the one
/// that closed its conversation).
#[test]
fn b846_an_open_tail_that_may_be_the_last_sessions_is_still_said_at_the_end() {
    let last = GapCause::TailUnknown.words();
    let open = bodies(page(
        TranscriptState::Incomplete,
        vec![msg(0, INC1, 0, "session one works")],
        vec![open_tail()],
    ));
    assert_eq!(open.last().map(String::as_str), Some(last), "{open:#?}");
    let closed_then_nothing = bodies(page(
        TranscriptState::Incomplete,
        vec![
            msg(0, INC1, 0, "session one works"),
            rec(1, INC1, 1, "end", json!({})),
        ],
        vec![open_tail()],
    ));
    assert_eq!(
        closed_then_nothing.last().map(String::as_str),
        Some(last),
        "{closed_then_nothing:#?}"
    );
}

/// Session 1 is lost with its conversation open, session 2 says so and closes its own conversation when its session is renewed, and
/// session 3 stops before it stores a chunk. The last record held is the `end` of session 2, and the open tail the read route
/// declares is session 3's. The notice about the last session stays when the run's events show a session that began after that
/// record, and when they say nothing about the last session.
#[test]
fn b846_an_end_record_of_an_earlier_session_does_not_hide_the_open_tail_of_the_last_session() {
    let last = GapCause::TailUnknown.words();
    let conversation = || {
        page(
            TranscriptState::Incomplete,
            lost_then_closed(),
            vec![open_tail()],
        )
    };
    // Session 3 began at 07:06, a minute after the `end` record.
    let later = bodies_for(conversation(), Some(T0 + 360));
    assert_eq!(
        later.iter().filter(|b| b.starts_with(EARLIER)).count(),
        1,
        "TRIGGER PRESENT: the open tail of session 1 is said where it happened: {later:#?}"
    );
    assert!(
        later
            .iter()
            .any(|b| b == "the conversation was closed cleanly"),
        "TRIGGER PRESENT: session 2 closed its conversation: {later:#?}"
    );
    assert_eq!(
        later.last().map(String::as_str),
        Some(last),
        "the last session closed nothing: {later:#?}"
    );
    // Events that do not say when the last session began: the record cannot be tied to the last session.
    let unknown = bodies_for(conversation(), None);
    assert_eq!(
        unknown.last().map(String::as_str),
        Some(last),
        "{unknown:#?}"
    );
    let plain = bodies(conversation());
    assert_eq!(plain.last().map(String::as_str), Some(last), "{plain:#?}");
    // A record without a time cannot be tied to it either.
    let mut untimed = lost_then_closed();
    if let Some(end) = untimed.last_mut() {
        end.t = None;
    }
    let untimed = bodies_for(
        page(TranscriptState::Incomplete, untimed, vec![open_tail()]),
        Some(T0 + 240),
    );
    assert_eq!(
        untimed.last().map(String::as_str),
        Some(last),
        "{untimed:#?}"
    );
}

/// The view says when the run's last session began: the time of the newest start a session wrote. It says nothing when the service
/// named a later session that never wrote its start, because a record of the conversation can then not be tied to the last session.
#[test]
fn b846_the_view_says_when_the_last_session_began_unless_a_later_one_never_wrote_its_start() {
    let mut events = begun();
    assert_eq!(
        state_of(events.clone()).view().last_session_began(),
        Some(T0 + 190)
    );
    // Session 1 is lost and the service starts session 2, which has not written its start yet.
    events.push(lost_event(5, &lost(1, 900, Some(300), None), 900));
    assert_eq!(state_of(events.clone()).view().last_session_began(), None);
    events.push(started(6, 905, 2));
    assert_eq!(
        state_of(events.clone()).view().last_session_began(),
        Some(T0 + 905)
    );
    // Session 3 was started by the service after a renewal and is lost before it writes its start.
    let mut third = lost(3, 2_600, None, None);
    third.then = "end".to_string();
    third.next_session = None;
    events.push(lost_event(7, &third, 2_600));
    assert_eq!(state_of(events).view().last_session_began(), None);
    // A run whose worker names no session says nothing.
    let unnamed = vec![
        h(1, 188, EventType::RunCreated, json!({"target": "s3-only"})),
        a(
            2,
            190,
            EventType::TurnStarted,
            json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct"}),
        ),
    ];
    assert_eq!(state_of(unnamed).view().last_session_began(), None);
}

// ---- the tokens outside the answered prompts (point 12) ------------------------------------------------------------------------------

fn answered(seq: u64, n: u64, tokens: u64) -> TranscriptRecord {
    rec(
        seq,
        INC1,
        n,
        "prompt_result",
        json!({"result": {"stopReason": "end_turn", "usage": {"totalTokens": tokens}}}),
    )
}

/// The proof's run: two prompts answered with 813,995 and 913,896 tokens, the card showed 1,941,678: the goal-check round used the
/// other 213,787. The stored conversation gives the sum of the prompts that answered, when it is whole.
#[test]
fn b846_the_stored_conversation_adds_up_the_tokens_of_the_prompts_that_answered() {
    let records = vec![
        msg(0, INC1, 0, "work"),
        answered(1, 1, 813_995),
        // A prompt that failed reported no usage.
        rec(
            2,
            INC1,
            2,
            "prompt_result",
            json!({"error": {"code": -32603}}),
        ),
        answered(3, 3, 913_896),
        rec(4, INC1, 4, "end", json!({})),
    ];
    let whole = stored(page(TranscriptState::Complete, records.clone(), vec![]));
    assert_eq!(whole.answered_tokens(), Some(1_727_891));
    // A conversation that still goes on, or one with a loss, gives no sum: a prompt's record may be missing.
    assert_eq!(
        stored(page(TranscriptState::Live, records.clone(), vec![])).answered_tokens(),
        None
    );
    assert_eq!(
        stored(page(
            TranscriptState::Incomplete,
            records,
            vec![open_tail()]
        ))
        .answered_tokens(),
        None
    );
    // No prompt reported usage: no sum.
    let none = stored(page(
        TranscriptState::Complete,
        vec![msg(0, INC1, 0, "work"), rec(1, INC1, 1, "end", json!({}))],
        vec![],
    ));
    assert_eq!(none.answered_tokens(), None);
}

/// The card has a row for the tokens the engine used outside the prompts that answered, under its `Tokens` row; a card without the
/// number has no such row.
#[test]
fn b846_the_card_has_a_row_for_the_tokens_outside_the_answered_prompts() {
    let mut events = begun();
    events.push(h(
        5,
        900,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    let v = state_of(events).view();
    let usage = Some(CardUsage {
        tokens: 1_941_678,
        usd: Some(2.320885),
    });
    let with = final_card(
        &v,
        &FinalCard {
            usage,
            outside_prompts: Some(213_787),
            ..FinalCard::default()
        },
    );
    let lines: Vec<&str> = with.lines().collect();
    let tokens = lines
        .iter()
        .position(|l| *l == "Tokens    1941678  USD 2.32")
        .unwrap_or_else(|| panic!("{with}"));
    assert_eq!(
        lines.get(tokens + 1).copied(),
        Some(
            "Outside   213787 of these tokens were used outside the prompts that answered (goal-check rounds, failed prompts)"
        ),
        "{with}"
    );
    let without = final_card(
        &v,
        &FinalCard {
            usage,
            ..FinalCard::default()
        },
    );
    assert!(!without.contains("Outside"), "{without}");
    // No usage, no row.
    let no_usage = final_card(
        &v,
        &FinalCard {
            outside_prompts: Some(213_787),
            ..FinalCard::default()
        },
    );
    assert!(!no_usage.contains("Outside"), "{no_usage}");
}

// ---- the request to end that the service itself reports (with the read of a stopped session's storage) -----------------------------

/// The service's own word that a session was asked to end is the lost-session event with the cause `end-requested` and the time of
/// the worker's notice. The view keeps it among the lost sessions, whether a copy was stored after the worker's word or not, and the
/// notice, the card's row and the card of the finished run say it. The worker's own word is no longer needed then.
#[test]
fn b846_a_request_to_end_is_always_said() {
    use ham_core::lost_session::{CAUSE_END_REQUESTED, FROM_RECOVERED, LostRecovery};
    for copy_after in [false, true] {
        let mut events = begun();
        events.push(copy(5, 211, 1, 1));
        events.push(copy_notice(6, 598, STOP));
        if copy_after {
            // The worker's next try stores a copy: what the run said about its copies ends, the request to end stays.
            events.push(copy(7, 603, 2, 1));
        }
        let v = state_of(events.clone()).view();
        assert_eq!(
            v.end_asked.as_deref(),
            Some("2026-11-14T07:09:58Z"),
            "TRIGGER PRESENT: the worker said that the session was asked to end (copy after: {copy_after})"
        );
        // The service: the session was asked to end at 07:09:58, and session 2 starts from the files read back.
        let mut l = lost(1, 673, Some(593), None);
        l.cause = CAUSE_END_REQUESTED.to_string();
        l.ended_at = Some(at(598));
        l.from = Some(FROM_RECOVERED.to_string());
        l.recovery = Some(LostRecovery {
            tried: true,
            used: true,
            kept: true,
            ..LostRecovery::default()
        });
        l.read_back_session = Some(1);
        events.push(lost_event(8, &l, 673));
        let v = state_of(events).view();
        assert_eq!(
            v.end_asked, None,
            "the service's word takes the worker's place"
        );
        assert_eq!(v.lost.len(), 1);
        assert_eq!(v.lost[0].cause, CAUSE_END_REQUESTED);
        assert_eq!(v.lost[0].ended_at.as_deref(), Some("2026-11-14T07:09:58Z"));
        let said = v.lost[0].sentence();
        assert!(
            said.starts_with(
                "Session 1 was asked to end from outside at 2026-11-14 07:09 UTC. Session 2 starts from the files the service read \
                 back from session 1's storage after it ended."
            ),
            "{said}"
        );
        let row = v.lost[0].card_row();
        assert!(
            row.starts_with("Lost      session 1: asked to end 2026-11-14 07:09 UTC; session 2 started from the files read back"),
            "{row}"
        );
        let card = final_card(&v, &FinalCard::default());
        assert!(card.contains(&row), "{card}");
    }
}
