//! What the client says about a session that stopped answering and about the new session the service started for the run.
//!
//! Everything is read from the run's events: the service writes one `run.waiting` event (harness authority, kind `session-lost`) when it
//! decides that a session is lost, and a session's worker says at its start how often it writes an event while it lives (`beatS`), so
//! that the age of the newest event tells a session that stopped answering from one that runs a long command.
#![allow(clippy::unwrap_used)]

use ham_core::chat::{BlockKind, ChatInputs, ChatModel, GapCause};
use ham_core::lost_session::{
    self, CAUSE_NO_HEARTBEAT, CAUSE_NO_RESULT, FROM_COPY, FROM_START, KIND, SessionLost, THEN_END,
    THEN_RESTART,
};
use ham_core::poll::PollPlan;
use ham_core::render::{FinalCard, final_card};
use ham_core::state::RunState;
use ham_core::stored::{Read, StoredChat};
use ham_core::transcript::{
    CompletenessEntry, Dir, GapKind, TranscriptPage, TranscriptRecord, TranscriptState,
};
use harness_protocol::api::v1::EventsPage;
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::store::event_shard;
use serde_json::{Value, json};
use std::time::Duration;

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// 2026-11-14T07:00:00Z in Unix seconds.
const T0: i64 = 1_794_639_600;

fn ulid(n: u64) -> String {
    const A: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut s = String::from("01J9ZZZZZZ");
    let mut v = n;
    let mut tail = [b'0'; 16];
    for i in (0..16).rev() {
        tail[i] = A[(v % 32) as usize];
        v /= 32;
    }
    s.push_str(std::str::from_utf8(&tail).unwrap());
    s
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

/// A run whose first session started at 07:03:10, saved a copy at 07:03:31 and wrote its last event at 07:04:48.
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
        a(
            4,
            190,
            EventType::TurnStarted,
            json!({"turn": 1, "engine": "cline", "model": "us.anthropic.claude-opus-5-5", "role": "direct", "session": 1, "beatS": 60}),
        ),
        a(
            5,
            211,
            EventType::TurnProgress,
            json!({"turn": 1, "savedCopy": {"at": at(211), "count": 1, "headSha": "29a235e2ea242b5e494b8dca989a4ea841132055", "session": 1}}),
        ),
        a(
            6,
            288,
            EventType::TurnProgress,
            json!({"turn": 1, "alive": {"session": 1}}),
        ),
    ]
}

fn state_of(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

fn lost(session: u32) -> SessionLost {
    SessionLost {
        kind: KIND.to_string(),
        session,
        decided_at: at(850),
        last_heartbeat_at: Some(at(250)),
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

fn lost_event(n: u64, l: &SessionLost) -> Envelope {
    h(
        n,
        850,
        EventType::RunWaiting,
        serde_json::to_value(l).unwrap(),
    )
}

// ---- the lost session -----------------------------------------------------------------------------------------------------------

#[test]
fn b842_the_sentence_of_a_lost_session_names_the_session_the_copy_and_what_is_lost() {
    assert_eq!(
        lost(1).sentence(),
        "Session 1 was lost: the service received no heartbeat from it for 600 seconds (the last one came at 2026-11-14 07:04 UTC). \
         Session 2 starts from the copy saved at 2026-11-14 07:03 UTC; file changes after that copy are lost."
    );
    let mut no_time = lost(1);
    no_time.saved_at = None;
    no_time.last_heartbeat_at = None;
    assert_eq!(
        no_time.sentence(),
        "Session 1 was lost: the service received no heartbeat from it for 600 seconds. \
         Session 2 starts from the last saved copy; file changes after that copy are lost."
    );
    let mut from_start = lost(1);
    from_start.from = Some(FROM_START.to_string());
    from_start.saved_at = None;
    assert!(
        from_start.sentence().ends_with(
            "Session 2 starts from the files the run started with: no copy had been saved, so the work of session 1 is lost."
        ),
        "{}",
        from_start.sentence()
    );
    let mut failed = lost(2);
    failed.cause = CAUSE_NO_RESULT.to_string();
    assert!(
        failed
            .sentence()
            .starts_with("Session 2 was lost: it ended without a result. Session 3 starts"),
        "{}",
        failed.sentence()
    );
    let mut third = lost(3);
    third.lost_in_a_row = 3;
    third.then = THEN_END.to_string();
    third.next_session = None;
    third.delivers = Some(lost_session::DELIVERS_COPY.to_string());
    assert!(
        third
            .sentence()
            .ends_with("It was lost session 3 in a row, so the run ends and the service delivers the last saved copy."),
        "{}",
        third.sentence()
    );
}

#[test]
fn b842_the_view_holds_each_lost_session_the_service_announced_and_only_the_services_word_counts() {
    let mut events = working_run();
    events.push(lost_event(7, &lost(1)));
    // The same payload from an agent is refused by the envelope check: `run.waiting` is a harness type.
    let mut forged = a(
        8,
        851,
        EventType::RunWaiting,
        serde_json::to_value(lost(2)).unwrap(),
    );
    forged.agent_id = Some("direct".into());
    events.push(forged);
    // A `run.waiting` of another kind says nothing here.
    events.push(h(
        9,
        852,
        EventType::RunWaiting,
        json!({"kind": "something-else"}),
    ));
    let v = state_of(events).view_at(Some(T0 + 860));
    assert_eq!(v.lost.len(), 1, "{:?}", v.lost);
    assert_eq!(v.lost[0], lost(1));
    assert_eq!(v.untrusted, 1, "the forged event is counted as ignored");
    // The run goes on: its status is what the service last said.
    assert_eq!(v.status, Some(harness_protocol::goal::GoalStatus::Active));
    assert!(
        v.result.is_none(),
        "a lost session is no outcome of the run"
    );
}

#[test]
fn b842_the_conversation_shows_the_lost_session_as_a_notice_of_the_harness() {
    let mut events = working_run();
    events.push(lost_event(7, &lost(1)));
    let state = state_of(events);
    let chat = ChatModel::from_progress_feed(&state, &ChatInputs::default());
    let notice = chat
        .blocks
        .iter()
        .find(|b| b.body.starts_with("Session 1 was lost"))
        .expect("a notice for the lost session");
    assert_eq!(notice.body, lost(1).sentence());
    assert!(matches!(notice.kind, BlockKind::Notice(_)));
    assert_eq!(notice.origin, ham_core::chat::Origin::Harness);
    assert!(notice.id.starts_with("n:"), "{}", notice.id);
}

#[test]
fn b842_the_card_of_a_run_that_lost_a_session_says_which_and_what_it_started_from() {
    let mut events = working_run();
    events.push(lost_event(7, &lost(1)));
    let v = state_of(events).view();
    let card = final_card(&v, &FinalCard::default());
    assert!(
        card.lines().any(|l| l
            == "Lost      session 1 at 2026-11-14 07:14 UTC; session 2 started from the copy saved at 2026-11-14 07:03 UTC; file changes after that copy were lost"),
        "{card}"
    );
    // A run that lost none has no such row.
    let plain = final_card(&state_of(working_run()).view(), &FinalCard::default());
    assert!(!plain.contains("Lost "), "{plain}");
}

// ---- the silent session ---------------------------------------------------------------------------------------------------------

#[test]
fn b842_a_session_that_says_it_beats_and_then_falls_silent_is_named_after_two_missed_beats() {
    let state = state_of(working_run());
    assert_eq!(lost_session::notice_after_s(60), 150);
    // 149 s after the newest event: one beat may be late.
    assert!(state.view_at(Some(T0 + 288 + 149)).silence().is_none());
    let v = state.view_at(Some(T0 + 288 + 150));
    let s = v.silence().expect("two beats are missing");
    assert_eq!(s.since_unix, T0 + 288);
    assert_eq!(s.secs, 150);
    assert_eq!(
        s.sentence(),
        "No word from the run's session since 2026-11-14 07:04 UTC (2 min). When the service receives no heartbeat from a session for \
         600 seconds, it starts a new session from the last saved copy (saved 2026-11-14 07:03 UTC) when the run has time left; file changes after that copy are lost."
    );
    // Without a clock nothing is said.
    assert!(state.view().silence().is_none());
}

#[test]
fn b842_a_silent_session_with_no_saved_copy_is_told_what_a_restart_would_start_from() {
    let mut events = working_run();
    events.remove(4);
    let v = state_of(events).view_at(Some(T0 + 288 + 400));
    let s = v.silence().expect("silent");
    assert!(
        s.sentence().ends_with(
            "it starts a new session from the files the run started with when the run has time left, because the run has saved no copy \
             yet."
        ),
        "{}",
        s.sentence()
    );
    assert!(s.sentence().contains("(6 min)"), "{}", s.sentence());
}

#[test]
fn b842_the_third_silence_in_a_row_says_that_the_run_would_end() {
    let mut events = working_run();
    let mut second = lost(2);
    second.lost_in_a_row = 2;
    events.push(lost_event(7, &second));
    events.push(a(
        8,
        870,
        EventType::TurnStarted,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct", "session": 3, "beatS": 60}),
    ));
    let v = state_of(events).view_at(Some(T0 + 870 + 200));
    let s = v.silence().expect("silent");
    assert!(
        s.sentence().ends_with(
            "When the service receives no heartbeat from a session for 600 seconds, the session is lost; it would be lost session 3 in a \
             row, so the run would end and the service would deliver the last saved copy."
        ),
        "{}",
        s.sentence()
    );
}

#[test]
fn b842_nothing_is_said_of_silence_when_the_events_cannot_show_it() {
    let late = T0 + 288 + 500;
    // A session of a worker that writes no beat: a long command looks the same as a session that is gone.
    let mut old_worker = working_run();
    old_worker[3] = a(
        4,
        190,
        EventType::TurnStarted,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct"}),
    );
    old_worker.pop();
    assert!(state_of(old_worker).view_at(Some(late)).silence().is_none());
    // A session that ended: the service works on the run now.
    let mut ended = working_run();
    ended.push(a(
        7,
        290,
        EventType::TurnEnded,
        json!({"turn": 1, "summary": "done"}),
    ));
    assert!(state_of(ended).view_at(Some(late)).silence().is_none());
    // A run that is delivering, and a run that is over.
    let mut delivering = working_run();
    delivering.push(h(
        7,
        290,
        EventType::GoalStatus,
        json!({"status": "active", "phase": "delivering"}),
    ));
    assert!(state_of(delivering).view_at(Some(late)).silence().is_none());
    let mut over = working_run();
    over.push(h(
        7,
        290,
        EventType::RunCompleted,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    ));
    assert!(state_of(over).view_at(Some(late)).silence().is_none());
    // The service has just announced the lost session: the newest word is its own.
    let mut announced = working_run();
    announced.push(lost_event(7, &lost(1)));
    assert!(
        state_of(announced)
            .view_at(Some(T0 + 860))
            .silence()
            .is_none()
    );
    // Before any session started.
    let before: Vec<Envelope> = working_run().into_iter().take(3).collect();
    assert!(state_of(before).view_at(Some(late)).silence().is_none());
}

#[test]
fn b842_a_beat_is_no_sign_that_the_run_is_busy_so_the_watcher_keeps_its_long_wait() {
    let base = Duration::from_secs(1);
    let shard = event_shard(Authority::Agent, Some("direct"));
    let page = |events: Vec<Envelope>| EventsPage {
        events,
        next_after: None,
    };
    let mut plan = PollPlan::new(base);
    // Six empty cycles: the wait has grown.
    for n in 0..6u64 {
        plan.record(Duration::from_secs(n), &[(shard, page(vec![]))]);
    }
    let grown = plan.interval();
    assert!(grown > base, "{grown:?}");
    plan.record(
        Duration::from_secs(7),
        &[(
            shard,
            page(vec![a(
                20,
                300,
                EventType::TurnProgress,
                json!({"turn": 1, "alive": {"session": 1}}),
            )]),
        )],
    );
    assert!(
        plan.interval() >= grown,
        "a beat alone does not bring the short wait back: {:?}",
        plan.interval()
    );
    plan.record(
        Duration::from_secs(8),
        &[(
            shard,
            page(vec![a(
                21,
                301,
                EventType::ToolSummary,
                json!({"title": "run_commands: pytest -q", "toolCallId": "c1"}),
            )]),
        )],
    );
    assert_eq!(plan.interval(), base, "a real event does");
}

// ---- the stored conversation of a run that lost a session --------------------------------------------------------------------------

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

fn tail(from: Option<u64>) -> CompletenessEntry {
    CompletenessEntry {
        kind: GapKind::TailUnknown,
        from_seq: from,
        to_seq: from,
        count: None,
    }
}

fn bodies(p: TranscriptPage) -> Vec<String> {
    let mut c = StoredChat::new();
    c.absorb(&p, Read::First);
    c.blocks().into_iter().map(|b| b.body).collect()
}

const EARLIER: &str =
    "[an earlier session stopped without closing its conversation: its last lines may be missing]";
const LAST: &str =
    "[the last session's conversation was not closed: its last lines may be missing]";

/// Session 1 wrote chunks 0 and 1 and was lost; session 2 found chunk 1 open, said so in its first record and closed its own conversation.
fn two_sessions() -> Vec<TranscriptRecord> {
    vec![
        msg(0, INC1, 0, "session one works"),
        msg(1, INC1, 1, "session one, last words that were stored"),
        rec(
            2,
            INC2,
            0,
            "gap",
            json!({"kind": "tail-unknown", "afterSeq": 1, "reason": "a previous writer ended without a final flush"}),
        ),
        msg(2, INC2, 1, "session two goes on"),
        rec(3, INC2, 2, "end", json!({})),
    ]
}

#[test]
fn b842_the_words_of_an_open_tail_are_about_a_session_and_not_about_the_end_of_the_run() {
    assert_eq!(GapCause::TailUnknown.words(), LAST);
    let shown = bodies(page(TranscriptState::Live, two_sessions(), vec![]));
    assert_eq!(shown[2], format!("{EARLIER} after chunk 1"), "{shown:#?}");
    assert!(
        !shown.iter().any(|b| b.contains("ended abnormally")),
        "{shown:#?}"
    );
}

#[test]
fn b842_a_run_that_ended_cleanly_does_not_end_with_the_notice_of_an_earlier_sessions_open_tail() {
    // The end summary of session 2 declares the open tail of session 1 with its chunk.
    let shown = bodies(page(
        TranscriptState::Incomplete,
        two_sessions(),
        vec![tail(Some(1))],
    ));
    assert_eq!(
        shown.last().map(String::as_str),
        Some("the conversation was closed cleanly"),
        "{shown:#?}"
    );
    assert_eq!(
        shown.iter().filter(|b| b.starts_with(EARLIER)).count(),
        1,
        "the open tail is said once, where it happened: {shown:#?}"
    );
}

#[test]
fn b842_a_declared_open_tail_whose_record_is_not_held_is_shown_where_it_happened() {
    let mut records = two_sessions();
    records.remove(2);
    let shown = bodies(page(
        TranscriptState::Incomplete,
        records,
        vec![tail(Some(1))],
    ));
    assert_eq!(shown[2], format!("{EARLIER} after chunk 1"), "{shown:#?}");
    assert_eq!(
        shown.last().map(String::as_str),
        Some("the conversation was closed cleanly")
    );
}

#[test]
fn b842_a_run_whose_last_session_never_closed_its_conversation_says_so_at_the_end() {
    // The read route names an open tail with no chunk for a run that ended without a summary of its conversation.
    let records = vec![msg(0, INC1, 0, "session one works")];
    let shown = bodies(page(TranscriptState::Incomplete, records, vec![tail(None)]));
    assert_eq!(shown.last().map(String::as_str), Some(LAST), "{shown:#?}");
}

// ---- the review round ---------------------------------------------------------------------------------------------------------

/// A blocking question the session asked at `secs`, open for 30 minutes.
fn question(n: u64, secs: i64) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnProgress,
        json!({
            "turn": 1, "kind": "needs-input", "text": "Needs your input: Cline stopped its own prompt",
            "requestId": ulid(n), "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
            "stopOutcome": "stopped_engine_cancelled",
            "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                         "lastError": null, "runS": 412, "sessions": 1},
            "lastMessage": "I will wait.", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "finish"], "models": [], "recommended": null, "canContinue": true,
            "waitS": 1800, "expiresAt": at(secs + 1800)
        }),
    )
}

fn started(n: u64, secs: i64, session: u32) -> Envelope {
    a(
        n,
        secs,
        EventType::TurnStarted,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct", "session": session, "beatS": 60}),
    )
}

#[test]
fn b842_a_session_that_waits_for_an_answer_is_not_called_silent() {
    // A worker writes no beat while its question is open (the run list finds the question in the newest events of the agent), so the
    // age of the newest event says nothing about a session that waits.
    let mut events = working_run();
    events.push(question(7, 300));
    let v = state_of(events).view_at(Some(T0 + 300 + 500));
    assert!(v.input.waiting().is_some(), "the question is open");
    assert!(
        v.silence().is_none(),
        "a session that waits for the user's answer is called silent: {:?}",
        v.silence().map(lost_session::Silence::sentence)
    );
}

#[test]
fn b842_the_silence_notice_counts_a_session_that_could_not_restore_the_work_among_the_lost_ones() {
    // Session 1 was lost (1 in a row). Session 2 could not restore the copy: the service counts it as the second and announces
    // nothing. Session 3 falls silent: losing it would end the run.
    let mut events = working_run();
    events.push(lost_event(7, &lost(1)));
    events.push(started(8, 870, 2));
    events.push(a(
        9,
        875,
        EventType::TurnEnded,
        json!({"turn": 1, "outcome": "failed", "directOutcome": "stopped_setup_failed", "label": "Cline did not start",
               "lastCommitSha": null, "stopReason": null}),
    ));
    events.push(started(10, 900, 3));
    let v = state_of(events).view_at(Some(T0 + 900 + 200));
    let s = v.silence().expect("silent");
    assert!(
        s.sentence().ends_with(
            "the session is lost; it would be lost session 3 in a row, so the run would end and the service would deliver the last \
             saved copy."
        ),
        "{}",
        s.sentence()
    );
    // A session that ended with a result of its own (a handover at its time limit) starts the count again.
    let mut events = working_run();
    events.push(lost_event(7, &lost(1)));
    events.push(started(8, 870, 2));
    events.push(a(
        9,
        2_000,
        EventType::TurnEnded,
        json!({"turn": 1, "outcome": "completed", "directOutcome": "stopped_timeout", "label": "Cline stopped: the run reached its time limit",
               "lastCommitSha": "29a235e2ea242b5e494b8dca989a4ea841132055", "stopReason": null}),
    ));
    events.push(started(10, 2_030, 3));
    let v = state_of(events).view_at(Some(T0 + 2_030 + 200));
    let s = v.silence().expect("silent");
    assert!(
        s.sentence()
            .contains("it starts a new session from the last saved copy"),
        "{}",
        s.sentence()
    );
}

#[test]
fn b842_a_silence_that_would_end_a_run_without_a_copy_promises_no_delivery() {
    let mut events = working_run();
    events.remove(4);
    let mut second = lost(2);
    second.lost_in_a_row = 2;
    second.from = Some(FROM_START.to_string());
    second.saved_at = None;
    events.push(lost_event(7, &second));
    events.push(started(8, 870, 3));
    let v = state_of(events).view_at(Some(T0 + 870 + 200));
    let s = v.silence().expect("silent");
    assert!(
        s.sentence().ends_with(
            "the session is lost; it would be lost session 3 in a row, so the run would end, and it has saved no copy that the \
             service could deliver."
        ),
        "{}",
        s.sentence()
    );
}

/// The service's word for the end of a run after three lost sessions, with what it says it delivers.
fn end_event(delivers: Option<&str>) -> Envelope {
    let mut third = lost(3);
    third.lost_in_a_row = 3;
    third.then = THEN_END.to_string();
    third.next_session = None;
    third.from = None;
    third.saved_at = None;
    let mut payload = serde_json::to_value(&third).unwrap();
    if let Some(d) = delivers {
        payload["delivers"] = json!(d);
    }
    h(7, 850, EventType::RunWaiting, payload)
}

#[test]
fn b842_the_end_after_lost_sessions_names_a_delivery_only_when_the_service_says_there_is_a_copy() {
    let sentence = |delivers: Option<&str>| {
        SessionLost::parse(&end_event(delivers))
            .expect("the client reads the payload")
            .sentence()
    };
    assert!(
        sentence(Some("copy")).ends_with(
            "It was lost session 3 in a row, so the run ends and the service delivers the last saved copy."
        ),
        "{}",
        sentence(Some("copy"))
    );
    assert!(
        sentence(Some("nothing")).ends_with(
            "It was lost session 3 in a row, so the run ends. The run saved no copy of its work, so there is nothing to deliver."
        ),
        "{}",
        sentence(Some("nothing"))
    );
    // An event that does not say what is delivered promises nothing.
    assert!(
        sentence(None).ends_with("It was lost session 3 in a row, so the run ends."),
        "{}",
        sentence(None)
    );
    // The conversation and `ham watch --json` carry the same word.
    let mut events = working_run();
    events.remove(4);
    events.push(end_event(Some("nothing")));
    let state = state_of(events);
    let chat = ChatModel::from_progress_feed(&state, &ChatInputs::default());
    assert!(
        chat.blocks
            .iter()
            .any(|b| b.body.ends_with("so there is nothing to deliver.")),
        "{:?}",
        chat.blocks.iter().map(|b| &b.body).collect::<Vec<_>>()
    );
}

#[test]
fn b842_the_question_of_a_lost_session_is_closed_and_the_client_says_why() {
    let mut events = working_run();
    events.push(question(7, 300));
    assert!(
        state_of(events.clone())
            .view_at(Some(T0 + 400))
            .input
            .waiting()
            .is_some()
    );
    // The service marks the session lost: no engine knows the question any more.
    events.push(lost_event(8, &lost(1)));
    let v = state_of(events).view_at(Some(T0 + 860));
    assert!(
        v.input.open.is_none(),
        "the question of the lost session is still open: {:?}",
        v.input.open
    );
    let closed = v
        .input
        .history
        .last()
        .expect("the question is in the history");
    assert_eq!(closed.how, "session-lost");
    assert_eq!(
        closed.phrase.to_string(),
        "closed when its session was lost; a new session asks again when it needs your answer"
    );
}
