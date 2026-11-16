//! In the client: the small points about what `ham` prints and shows that the live proofs of two releases left.
//!
//! * `ham runs --json` writes the phase of a finished run as `ham runs` and `ham watch --json` write it: `delivered`.
//! * `ham doctor` prints the entries of the allowlist under the line of the allowlist.
//! * Line mode says what is true about the copies after the run's word that its session was asked to end: the last saved copy is
//!   from before the request, or there was none, and an open question belongs to the session that was asked to end. No `saved:`
//!   line names the copy from before as if it were news.
//! * "The run's session answers again." is printed whenever a silence or a lost session was told and a session is open again.
//! * The JSON view of `ham watch` carries the run's notices about its copies as `copyNotices`.
//! * The run screen says beside the status of a run that goes on that its session was asked to end, is silent, or that the run
//!   waits for the session the service starts, and the card of an open question says when the session that asked was asked to end.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::doctor::{Check, Doctor, report};
use ham::output::Output;
use ham::tui::{App, DirectSettings, Msg};
use ham::watch::{InputMemo, LineMemo, view_json};
use ham_core::lost_session::{CAUSE_NO_HEARTBEAT, FROM_COPY, KIND, SessionLost, THEN_RESTART};
use ham_core::state::RunState;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";
const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
/// The worker's sentences about a copy (`agentd::direct`: `STOP_COPY_NOT_SAVED`, `COPY_NOT_SAVED`), as the run's events carry them.
const STOP: &str = "The session was asked to end, and a copy of the work could not be saved.";
const COPY_NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";
const AGAIN: &str = "The run's session answers again.";

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

fn started(seq: u64, ts: &str, session: u32) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnStarted,
        Authority::Agent,
        json!({"turn": 1, "engine": "cline", "model": "m", "role": "direct", "session": session, "beatS": 60}),
    )
}

fn copy(seq: u64, ts: &str, count: u32, session: u32) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "savedCopy": {"at": ts, "count": count, "headSha": HEAD, "session": session}}),
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

fn copy_notice(seq: u64, ts: &str, sentence: &str) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnProgress,
        Authority::Agent,
        json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null, "detail": sentence}),
    )
}

fn question(seq: u64, ts: &str, expires: &str) -> Envelope {
    event(
        seq,
        ts,
        EventType::TurnProgress,
        Authority::Agent,
        json!({
            "turn": 1, "kind": "needs-input", "text": "Needs your input: Cline ended without changing files",
            "requestId": format!("01K6A00000000000000000{seq:04}"), "n": 1, "reason": "no_changes",
            "title": "Cline ended without changing files", "stopOutcome": "stopped_no_changes",
            "evidence": {"attempts": 1, "toolCalls": 0, "filesChanged": 0, "identicalCalls": null, "failedTurns": null,
                         "lastError": null, "runS": 30, "sessions": 1},
            "lastMessage": "Which design do you want?", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "finish"], "models": [], "recommended": null, "canContinue": true,
            "waitS": 1800, "expiresAt": expires
        }),
    )
}

/// A run whose session started at 08:15:10, saved a copy at 08:15:31 and wrote its last event (a beat) at 08:16:48.
fn working() -> Vec<Envelope> {
    let mut events = base_events();
    events.push(started(10, "2026-10-16T08:15:10Z", 1));
    events.push(copy(11, "2026-10-16T08:15:31Z", 1, 1));
    events.push(beat(12, "2026-10-16T08:16:48Z", 1));
    events
}

/// A run whose session started at 08:15:10 and asked a question at 08:15:40, with no file changed and no copy saved.
fn waiting() -> Vec<Envelope> {
    let mut events = base_events();
    events.push(started(10, "2026-10-16T08:15:10Z", 1));
    events.push(question(11, "2026-10-16T08:15:40Z", "2026-10-16T08:45:40Z"));
    events
}

fn lost(saved: bool) -> SessionLost {
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
        from: Some(if saved { FROM_COPY } else { "start" }.to_string()),
        saved_at: saved.then(|| "2026-10-16T08:15:31Z".to_string()),
        delivers: None,
    }
}

fn lost_event(saved: bool) -> Envelope {
    event(
        20,
        "2026-10-16T08:26:10Z",
        EventType::RunWaiting,
        Authority::Harness,
        serde_json::to_value(lost(saved)).unwrap(),
    )
}

fn unix(at: &str) -> i64 {
    parse_utc_unix(at).unwrap()
}

fn view_at(events: Vec<Envelope>, now: &str) -> ham_core::state::RunView {
    let mut state = RunState::new(RUN);
    for e in events {
        state.apply(e);
    }
    state.view_at(Some(unix(now)))
}

/// The words of a screen as one line of text: the card wraps a long sentence, and a row is padded.
fn flat(screen: &str) -> String {
    screen.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The run on screen with `events` read at the clock `now`.
fn ready_at(events: Vec<Envelope>, now: &str) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(unix(now));
    let _ = app.open_run(RUN);
    feed(&mut app, events);
    let mut clock = Clock::early();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    app
}

// ---- point 1: the phase of a finished run in `ham runs --json` ----------------------------------------------------------------------

fn row(status: &str, phase: &str) -> ham_core::run_index::RunRow {
    serde_json::from_value(json!({"runId": "01M6C3D78J6WJM6ADMEPMRMJ38", "status": status, "phase": phase,
        "target": {"mode": "s3-only"}, "createdAt": "2026-11-01T12:00:00Z", "ownerPrincipalArn": ARN}))
    .unwrap()
}

/// `ham runs` printed `complete (delivered)`, `ham watch --json` wrote `"phase": "delivered"`, and `ham runs --json` wrote
/// `"phase": "delivering"` with `"status": "complete"` for the same run. The three agree.
#[test]
fn b846_ham_runs_json_writes_the_phase_of_a_finished_run_as_the_line_writes_it() {
    let finished = ham::cli::run_json(&row("complete", "delivering"));
    assert_eq!(finished["status"], "complete");
    assert_eq!(finished["phase"], "delivered", "{finished}");
    let line = ham::cli::runs_line(&row("complete", "delivering").summary);
    assert!(line.contains("complete (delivered)"), "{line}");
    // Every other pair is written as stored, as the line writes it.
    for (status, phase) in [
        ("active", "delivering"),
        ("active", "green"),
        ("blocked", "delivering"),
        ("cleared", "green"),
        ("complete", "green"),
    ] {
        let v = ham::cli::run_json(&row(status, phase));
        assert_eq!(v["status"], status);
        assert_eq!(v["phase"], phase, "{v}");
    }
}

// ---- point 2: the allowlist entries of `ham doctor` ------------------------------------------------------------------------------------

fn check(name: &'static str, detail: &str) -> Check {
    Check {
        name,
        pass: true,
        warn: false,
        detail: detail.to_string(),
    }
}

fn entry(target: &str) -> harness_protocol::api::v1::RepoAllowView {
    serde_json::from_value(
        json!({"target": target, "addedBy": ARN, "addedAt": "2026-11-03T23:05:08Z"}),
    )
    .unwrap()
}

fn doctor_lines(d: &Doctor) -> Vec<String> {
    let out = Output::capture();
    let code = report(d, false, &out);
    assert_eq!(code, 0);
    out.finish(code)
        .stdout_text()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The entry of the allowlist was printed after the last check, under the line `PASS provider-data-sharing`. It belongs under the
/// line of the allowlist.
#[test]
fn b846_ham_doctor_prints_the_allowlist_entries_under_the_allowlist_line() {
    let d = Doctor {
        checks: vec![
            check("git-host", "cc: iam"),
            check("allowlist", "1 entries"),
            check(
                "provider-data-sharing",
                "run.provider_sharing is off; the Bedrock data retention mode in us-east-1 is inherit",
            ),
        ],
        allowlist: vec![entry("cc:hamh-e2e-tdd-bugfix")],
    };
    assert_eq!(
        doctor_lines(&d),
        vec![
            "PASS git-host cc: iam".to_string(),
            "PASS allowlist 1 entries".to_string(),
            format!("  cc:hamh-e2e-tdd-bugfix added by {ARN} at 2026-11-03T23:05:08Z"),
            "PASS provider-data-sharing run.provider_sharing is off; the Bedrock data retention mode in us-east-1 is inherit"
                .to_string(),
        ]
    );
}

/// With several backends each has its own allowlist line, led by its alias, and the entries shown are those of the first backend
/// that has any: they follow that backend's line.
#[test]
fn b846_ham_doctor_with_two_backends_prints_the_entries_under_the_line_they_belong_to() {
    let d = Doctor {
        checks: vec![
            check("allowlist", "work: 0 entries"),
            check("versions", "work: ham v5.21 | backend v5.21"),
            check("allowlist", "lab: 2 entries"),
            check("versions", "lab: ham v5.21 | backend v5.21"),
        ],
        allowlist: vec![entry("cc:hamh-e2e-a"), entry("cc:hamh-e2e-b")],
    };
    let lines = doctor_lines(&d);
    assert_eq!(lines[2], "PASS allowlist lab: 2 entries", "{lines:#?}");
    assert!(
        lines[3].starts_with("  cc:hamh-e2e-a added by "),
        "{lines:#?}"
    );
    assert!(
        lines[4].starts_with("  cc:hamh-e2e-b added by "),
        "{lines:#?}"
    );
    assert_eq!(lines[5], "PASS versions lab: ham v5.21 | backend v5.21");
    assert_eq!(lines.len(), 6, "{lines:#?}");
}

/// With several backends the doctor holds the entries of the first backend that has any: a second backend's line gives its own
/// count and no entry follows it. The guide says whose entries are listed, and promises no entries under every backend's line.
#[test]
fn b846_the_guide_says_whose_allowlist_entries_ham_doctor_lists_with_several_backends() {
    let d = Doctor {
        checks: vec![
            check("allowlist", "a: 2 entries"),
            check("versions", "a: ham v5.21 | backend v5.21"),
            check("allowlist", "b: 3 entries"),
            check("versions", "b: ham v5.21 | backend v5.21"),
        ],
        allowlist: vec![entry("cc:hamh-e2e-a1"), entry("cc:hamh-e2e-a2")],
    };
    let lines = doctor_lines(&d);
    assert_eq!(
        lines
            .iter()
            .map(|l| l.split(" added by ").next().unwrap_or(l).to_string())
            .collect::<Vec<_>>(),
        vec![
            "PASS allowlist a: 2 entries",
            "  cc:hamh-e2e-a1",
            "  cc:hamh-e2e-a2",
            "PASS versions a: ham v5.21 | backend v5.21",
            "PASS allowlist b: 3 entries",
            "PASS versions b: ham v5.21 | backend v5.21",
        ],
        "TRIGGER PRESENT: the entries of the second backend are not listed"
    );
    let guide = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/user-guide.md"
    ))
    .unwrap();
    let all = guide.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        !all.contains("each entry sits under the `allowlist` line for its backend"),
        "the guide promises the entries of every backend under its own line"
    );
    for words in [
        "With several backends, `ham doctor` lists the entries of the first backend that has any, under that backend's `allowlist` line.",
        "The entries of the other backends are not listed.",
        "Each `allowlist` line still gives the count of its own backend.",
    ] {
        assert!(all.contains(words), "{words:?} is missing from the guide");
    }
}

/// A report without an allowlist line (a backend that did not answer) still prints the entries it holds, last.
#[test]
fn b846_ham_doctor_without_an_allowlist_line_prints_the_entries_last() {
    let d = Doctor {
        checks: vec![check("git-host", "cc: iam")],
        allowlist: vec![entry("cc:hamh-e2e-a")],
    };
    let lines = doctor_lines(&d);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(
        lines[1].starts_with("  cc:hamh-e2e-a added by "),
        "{lines:#?}"
    );
}

// ---- point 11: "The run's session answers again." -------------------------------------------------------------------------------------

/// The watch told the silence and the lost session; session 2 starts in a later read. The start command printed the sentence in the
/// proof only because one read brought the service's word and the start of session 2 together.
#[test]
fn b846_ham_watch_says_that_the_session_answers_again_after_a_lost_session_whenever_the_new_one_starts()
 {
    let mut memo = InputMemo::new();
    let silent = memo.lines(&view_at(working(), "2026-10-16T08:20:00Z"));
    assert_eq!(
        silent.len(),
        1,
        "TRIGGER PRESENT: the silence is told: {silent:?}"
    );
    let mut events = working();
    events.push(lost_event(true));
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:26:20Z")),
        vec![lost(true).sentence()],
        "TRIGGER PRESENT: the lost session is told in a read of its own"
    );
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:26:45Z")),
        vec![AGAIN.to_string()]
    );
    // It is said once.
    assert!(
        memo.lines(&view_at(events, "2026-10-16T08:27:00Z"))
            .is_empty()
    );
}

/// The same when one read brings the service's word and the start of session 2 together: the lost session, then the sentence.
#[test]
fn b846_ham_watch_says_it_too_when_one_read_brings_the_lost_session_and_the_new_one() {
    let mut memo = InputMemo::new();
    let _ = memo.lines(&view_at(working(), "2026-10-16T08:20:00Z"));
    let mut events = working();
    events.push(lost_event(true));
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    assert_eq!(
        memo.lines(&view_at(events, "2026-10-16T08:26:45Z")),
        vec![lost(true).sentence(), AGAIN.to_string()]
    );
}

/// A session that waited for an answer is never called silent. Its loss is told, and so is the start of the session after it.
#[test]
fn b846_ham_watch_says_it_after_a_lost_session_that_was_never_called_silent() {
    let mut memo = InputMemo::new();
    let _ = memo.lines(&view_at(waiting(), "2026-10-16T08:16:00Z"));
    let mut events = waiting();
    events.push(lost_event(false));
    let told = memo.lines(&view_at(events.clone(), "2026-10-16T08:26:20Z"));
    assert!(told.contains(&lost(false).sentence()), "{told:?}");
    assert!(!told.contains(&AGAIN.to_string()), "{told:?}");
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    assert_eq!(
        memo.lines(&view_at(events, "2026-10-16T08:26:45Z")),
        vec![AGAIN.to_string()]
    );
}

/// A watch that starts on a run whose lost session is history is told of it, and not that a session answers again: it saw no silence.
#[test]
fn b846_a_watch_that_starts_after_the_restart_is_not_told_that_the_session_answers_again() {
    let mut events = working();
    events.push(lost_event(true));
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    events.push(beat(31, "2026-10-16T08:40:00Z", 2));
    let mut memo = InputMemo::new();
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:40:10Z")),
        vec![lost(true).sentence()]
    );
    assert!(
        memo.lines(&view_at(events, "2026-10-16T08:40:20Z"))
            .is_empty()
    );
}

// ---- points 3, 7 and 8 in line mode: after the run's word that its session was asked to end ------------------------------------------

/// The proof's lines: `saved: ... 03:49` at 03:49:18, the request to end at 03:49:59, and at 03:50:19 `saved: ... 03:49` again, for
/// the copy of 03:49:42 that the once-a-minute rule had held back. A reader took it for a copy saved after the request.
#[test]
fn b846_no_saved_line_names_the_copy_from_before_a_request_to_end_as_if_it_were_news() {
    let mut memo = LineMemo::new();
    let first = memo.lines(&view_at(working(), "2026-10-16T08:16:50Z"), 0);
    assert_eq!(
        first,
        vec![format!(
            "saved: a copy of the work at 2026-10-16 08:15 UTC; ham pull {RUN} --snapshot downloads it"
        )]
    );
    // A second copy, within the minute: line mode holds its line back.
    let mut events = working();
    events.push(copy(13, "2026-10-16T08:16:55Z", 2, 1));
    assert!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:16:58Z"), 8)
            .is_empty(),
        "TRIGGER PRESENT: a copy within the minute has no line yet"
    );
    // The session is asked to end and cannot save a copy.
    events.push(copy_notice(14, "2026-10-16T08:17:05Z", STOP));
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:17:08Z"), 18),
        vec![
            STOP.to_string(),
            format!(
                "The last saved copy is from before that request: 2026-10-16 08:16 UTC; ham pull {RUN} --snapshot downloads it."
            ),
        ]
    );
    // The minute is over: the copy from before the request gets no line of its own.
    let later = memo.lines(&view_at(events.clone(), "2026-10-16T08:18:10Z"), 80);
    assert!(later.is_empty(), "{later:?}");
    // Nor after the service has called the session lost.
    events.push(lost_event(true));
    let after = memo.lines(&view_at(events.clone(), "2026-10-16T08:26:20Z"), 570);
    assert_eq!(after, vec![lost(true).sentence()]);
    // A copy of session 2 is news again.
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    events.push(copy(31, "2026-10-16T08:27:10Z", 3, 2));
    let news = memo.lines(&view_at(events, "2026-10-16T08:27:12Z"), 622);
    assert_eq!(
        news,
        vec![
            AGAIN.to_string(),
            format!(
                "saved: a copy of the work at 2026-10-16 08:27 UTC; ham pull {RUN} --snapshot downloads it"
            ),
        ]
    );
}

/// Another notice about a copy holds the `saved:` line of the copy from before it back too: that line read as a copy saved since.
/// In its place line mode says once, under the notice, that the last saved copy is from before the notice and which command
/// downloads it: a reader who is told that a copy could not be saved learns where the last kept copy is.
#[test]
fn b846_a_notice_that_a_copy_could_not_be_saved_dates_the_copy_from_before_it_and_prints_no_saved_line()
 {
    let mut memo = LineMemo::new();
    let _ = memo.lines(&view_at(working(), "2026-10-16T08:16:50Z"), 0);
    let mut events = working();
    events.push(copy(13, "2026-10-16T08:16:55Z", 2, 1));
    events.push(copy_notice(14, "2026-10-16T08:17:05Z", COPY_NOT_SAVED));
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:17:08Z"), 18),
        vec![
            COPY_NOT_SAVED.to_string(),
            format!(
                "The last saved copy is from before that notice: 2026-10-16 08:16 UTC; ham pull {RUN} --snapshot downloads it."
            ),
        ]
    );
    // The minute is over: the copy from before the notice gets no `saved:` line, and the sentence is not said again.
    assert!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:18:10Z"), 80)
            .is_empty()
    );
    // The next stored copy is said.
    events.push(copy(15, "2026-10-16T08:18:20Z", 3, 1));
    assert_eq!(
        memo.lines(&view_at(events, "2026-10-16T08:18:22Z"), 92),
        vec![format!(
            "saved: a copy of the work at 2026-10-16 08:18 UTC; ham pull {RUN} --snapshot downloads it"
        )]
    );
}

/// Session 2 starts from the copy of session 1 and says that it cannot save copies; that notice stands for the whole session. A
/// watch that starts then is told so, and learns once where the last kept copy is and which command downloads it.
#[test]
fn b846_a_watch_that_starts_during_a_notice_about_copies_names_the_last_saved_copy_once() {
    const COPIES_OFF: &str = "Copies of the work cannot be saved in this session. The work is kept only if the run delivers it.";
    let mut events = working();
    events.push(lost_event(true));
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    events.push(copy_notice(31, "2026-10-16T08:26:50Z", COPIES_OFF));
    let v = view_at(events.clone(), "2026-10-16T08:27:00Z");
    assert_eq!(
        v.saved.as_ref().map(|c| (c.session, c.at.as_str())),
        Some((1, "2026-10-16T08:15:31Z")),
        "TRIGGER PRESENT: the run holds the copy of session 1"
    );
    assert_eq!(
        v.copy_notices.len(),
        1,
        "TRIGGER PRESENT: the notice of session 2 about its copies stands"
    );
    let mut memo = LineMemo::new();
    assert_eq!(
        memo.lines(&v, 0),
        vec![
            lost(true).sentence(),
            COPIES_OFF.to_string(),
            format!(
                "The last saved copy is from before that notice: 2026-10-16 08:15 UTC; ham pull {RUN} --snapshot downloads it."
            ),
        ]
    );
    // It is said once, and no `saved:` line names that copy later.
    events.push(beat(32, "2026-10-16T08:28:30Z", 2));
    let later = memo.lines(&view_at(events.clone(), "2026-10-16T08:28:40Z"), 100);
    assert!(later.is_empty(), "{later:?}");
    // A run without a saved copy gets no such sentence: there is no copy to name.
    let mut none = base_events();
    none.push(started(10, "2026-10-16T08:15:10Z", 1));
    none.push(copy_notice(11, "2026-10-16T08:15:20Z", COPIES_OFF));
    assert_eq!(
        LineMemo::new().lines(&view_at(none, "2026-10-16T08:15:30Z"), 0),
        vec![COPIES_OFF.to_string()]
    );
}

/// The wait leg of the proof: the run waited for an answer, no file had changed and no copy existed when its session was asked to
/// end. Line mode says that there was no copy before the request and whose question the open one is.
#[test]
fn b846_a_request_to_end_while_the_run_waits_for_an_answer_says_that_no_copy_existed_and_whose_question_it_is()
 {
    let mut memo = LineMemo::new();
    let asked = memo.lines(&view_at(waiting(), "2026-10-16T08:15:45Z"), 0);
    assert!(
        asked
            .first()
            .is_some_and(|l| l.starts_with("Cline needs you: Cline ended without changing files")),
        "TRIGGER PRESENT: the question is told: {asked:?}"
    );
    let mut events = waiting();
    events.push(copy_notice(12, "2026-10-16T08:16:30Z", STOP));
    assert_eq!(
        memo.lines(&view_at(events, "2026-10-16T08:16:35Z"), 50),
        vec![
            STOP.to_string(),
            "No copy of the work had been saved before that request: a run saves a copy only after its files change."
                .to_string(),
            "The open question was asked by that session: once it has ended, no answer reaches it. When the run has time left, a \
             new session starts and asks again when it needs your answer."
                .to_string(),
        ]
    );
}

/// The sentence about the open question promises a new session only where the service starts one. Sessions 1 and 2 were lost in a
/// row; session 3 asks a question and is asked to end. One more lost session ends the run, and line mode says so.
#[test]
fn b846_line_mode_says_that_the_run_would_end_when_the_ending_session_would_be_the_third_lost_in_a_row()
 {
    let mut events = base_events();
    events.push(started(10, "2026-10-16T08:15:10Z", 1));
    events.push(lost_event(false));
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    let mut second = lost(false);
    second.session = 2;
    second.decided_at = "2026-10-16T08:40:00Z".into();
    second.last_heartbeat_at = Some("2026-10-16T08:30:00Z".into());
    second.lost_in_a_row = 2;
    second.next_session = Some(3);
    events.push(event(
        40,
        "2026-10-16T08:40:00Z",
        EventType::RunWaiting,
        Authority::Harness,
        serde_json::to_value(&second).unwrap(),
    ));
    events.push(started(50, "2026-10-16T08:40:30Z", 3));
    events.push(question(51, "2026-10-16T08:41:00Z", "2026-10-16T09:11:00Z"));
    let mut memo = LineMemo::new();
    let _ = memo.lines(&view_at(events.clone(), "2026-10-16T08:41:05Z"), 0);
    events.push(copy_notice(52, "2026-10-16T08:42:00Z", STOP));
    let v = view_at(events, "2026-10-16T08:42:05Z");
    assert_eq!(
        v.lost_in_a_row,
        MAX_LOST_SLICES - 1,
        "TRIGGER PRESENT: one more lost session in a row ends the run"
    );
    let told = memo.lines(&v, 60);
    assert_eq!(
        told.last().map(String::as_str),
        Some(
            "The open question was asked by that session: once it has ended, no answer reaches it. It would be lost session 3 in \
             a row, so the run would end and no new session would ask again."
        ),
        "{told:?}"
    );
    assert!(
        !told.iter().any(|l| l.contains("a new session starts")),
        "{told:?}"
    );
}

/// The guide quotes both sentences about the open question of a session that was asked to end, and promises a new session only
/// with the conditions the service applies.
#[test]
fn b846_the_guide_quotes_the_sentences_about_the_open_question_of_an_ending_session() {
    let guide = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/user-guide.md"
    ))
    .unwrap();
    let all = guide.split_whitespace().collect::<Vec<_>>().join(" ");
    for words in [
        "The open question was asked by that session: once it has ended, no answer reaches it. When the run has time left, a new \
         session starts and asks again when it needs your answer.",
        "The open question was asked by that session: once it has ended, no answer reaches it. It would be lost session 3 in a row, \
         so the run would end and no new session would ask again.",
    ] {
        assert!(all.contains(words), "{words:?} is missing from the guide");
    }
    assert!(
        !all.contains(
            "no answer reaches it, and a new session asks again when it needs your answer"
        ),
        "the guide still quotes the sentence that promises a new session without a condition"
    );
}

/// A session whose copies fail from its first change: the run said that a copy could not be saved, and then that its session was
/// asked to end. No copy exists although files changed, so line mode does not explain the missing copy by unchanged files.
#[test]
fn b846_line_mode_does_not_explain_a_missing_copy_by_unchanged_files_after_failed_copies() {
    let mut events = base_events();
    events.push(started(10, "2026-10-16T08:15:10Z", 1));
    events.push(copy_notice(11, "2026-10-16T08:16:00Z", COPY_NOT_SAVED));
    let mut memo = LineMemo::new();
    assert_eq!(
        memo.lines(&view_at(events.clone(), "2026-10-16T08:16:05Z"), 0),
        vec![COPY_NOT_SAVED.to_string()],
        "TRIGGER PRESENT: the run said that a copy could not be saved, and it holds no copy"
    );
    events.push(copy_notice(12, "2026-10-16T08:16:30Z", STOP));
    assert_eq!(
        memo.lines(&view_at(events, "2026-10-16T08:16:35Z"), 30),
        vec![
            STOP.to_string(),
            "No copy of the work has been saved in this run.".to_string(),
        ]
    );
}

// ---- the JSON view of `ham watch` -----------------------------------------------------------------------------------------------------

/// A script that follows `ham watch --json` learned of the request to end only with `silentSince`, more than two minutes later. The
/// view carries the notices about the copies since the newest stored copy as `copyNotices`; a view without one has no such member.
#[test]
fn b846_the_json_view_of_ham_watch_carries_the_notices_about_the_copies() {
    let plain = view_json(&view_at(working(), "2026-10-16T08:17:00Z"));
    assert!(plain.get("copyNotices").is_none(), "{plain}");
    let mut events = working();
    events.push(copy_notice(13, "2026-10-16T08:17:02Z", COPY_NOT_SAVED));
    events.push(copy_notice(14, "2026-10-16T08:17:05Z", STOP));
    let v = view_json(&view_at(events.clone(), "2026-10-16T08:17:08Z"));
    assert_eq!(
        v["copyNotices"],
        json!([
            {"at": "2026-10-16T08:17:02Z", "text": COPY_NOT_SAVED, "endRequest": false},
            {"at": "2026-10-16T08:17:05Z", "text": STOP, "endRequest": true},
        ]),
        "{v}"
    );
    // The member is the only difference.
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.retain(|k| k != "copyNotices");
    let plain_keys: Vec<String> = plain.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, plain_keys);
    // A stored copy ends what the run said: the member is gone.
    events.push(copy(15, "2026-10-16T08:17:30Z", 2, 1));
    let after = view_json(&view_at(events, "2026-10-16T08:17:32Z"));
    assert!(after.get("copyNotices").is_none(), "{after}");
}

// ---- points 6 and 8 on the run screen ---------------------------------------------------------------------------------------------------

/// The run stayed `active (green)` for the eleven minutes without a session. The title and the status block say what the run is
/// missing: its session is silent, then the run waits for the session the service starts.
#[test]
fn b846_the_run_screen_says_beside_the_status_that_the_session_is_silent_or_that_the_run_waits_for_a_new_one()
 {
    let mut app = ready_at(working(), "2026-10-16T08:17:00Z");
    let text = flat(&screen(&app, 160, 50));
    assert!(text.contains("active (green)"), "{text}");
    assert!(
        !text.contains("session silent"),
        "a session that works: {text}"
    );
    // Five minutes after the last event.
    app.set_now(unix("2026-10-16T08:21:48Z"));
    let _ = app.update(Msg::Pages(Ok(vec![])));
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("No event from the run's session since 2026-10-16 08:16 UTC (5 min)."),
        "TRIGGER PRESENT: the status block tells the silence: {text}"
    );
    assert!(text.contains("active (green) · session silent"), "{text}");
    assert!(text.contains("· active · session silent ·"), "{text}");
    // The service calls the session lost; session 2 has not started.
    let mut events = working();
    events.push(lost_event(true));
    let mut app = ready_at(events.clone(), "2026-10-16T08:26:20Z");
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("active (green) · waiting for session 2"),
        "{text}"
    );
    assert!(
        text.contains("· active · waiting for session 2 ·"),
        "{text}"
    );
    // Session 2 has started: the status is the plain one again.
    feed(&mut app, vec![started(30, "2026-10-16T08:26:40Z", 2)]);
    app.set_now(unix("2026-10-16T08:26:45Z"));
    let _ = app.update(Msg::Pages(Ok(vec![])));
    let text = flat(&screen(&app, 160, 50));
    assert!(!text.contains("waiting for session"), "{text}");
    assert!(!text.contains("session silent"), "{text}");
}

/// From the request to end to the service's decision the screen showed the open question with the keys that answer it and the
/// minutes left, for a session that had been asked to end. The card's title says so in place of the minutes, its first lines say
/// that no answer reaches a session that has ended and that a new session asks again, and the status block names the time.
#[test]
fn b846_the_card_of_an_open_question_says_that_its_session_was_asked_to_end() {
    let mut app = ready_at(waiting(), "2026-10-16T08:16:00Z");
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("Cline needs you · about 30 min left to answer"),
        "TRIGGER PRESENT: the question is open with its minutes: {text}"
    );
    feed(
        &mut app,
        vec![copy_notice(12, "2026-10-16T08:16:30Z", STOP)],
    );
    app.set_now(unix("2026-10-16T08:16:40Z"));
    let _ = app.update(Msg::Pages(Ok(vec![])));
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("Cline needs you · its session was asked to end"),
        "{text}"
    );
    assert!(!text.contains("min left to answer"), "{text}");
    assert!(
        text.contains(
            "The session was asked to end at 2026-10-16 08:16 UTC. No copy of the work had been saved before that request: a run \
             saves a copy only after its files change. The open question was asked by that session: once it has ended, no answer \
             reaches it. When the run has time left, a new session starts and asks again when it needs your answer."
        ),
        "{text}"
    );
    assert!(text.contains("needs you · session asked to end"), "{text}");
}

// ---- point 9 on the run screen: the last line of the stored conversation --------------------------------------------------------------

const INC2: &str = "01M5A2R7K3C9XJ4D8E6F000002";
const NOT_CLOSED: &str =
    "[the last session's conversation was not closed: its last lines may be missing]";

/// A record of the stored conversation with its writer and its time.
fn stored_rec(seq: u64, n: u64, inc: &str, t: &str, kind: &str, data: Value) -> Value {
    json!({"seq": seq, "n": n, "incarnation": inc, "origin": "engine", "t": t, "dir": "in", "kind": kind, "data": data})
}

/// The stored conversation of a run whose session 1 was lost with its conversation open and whose session 2 said so in its first
/// record and closed its own conversation at 08:30:00. The read route declares an open tail without a chunk.
fn lost_then_closed() -> ham_core::transcript::TranscriptPage {
    let text = |id: &str, words: &str| json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": words}});
    serde_json::from_value(json!({
        "runId": RUN, "state": "incomplete", "gaps": [], "completeness": [{"kind": "tail-unknown"}],
        "prev": null, "next": null, "more": false, "skippedKeys": 0,
        "records": [
            stored_rec(0, 0, INC, "2026-10-16T08:15:20Z", "agent_message_chunk", text("m1", "session one works")),
            stored_rec(1, 0, INC2, "2026-10-16T08:26:45Z", "gap",
                json!({"kind": "tail-unknown", "afterSeq": 0, "reason": "a previous writer ended without a final flush"})),
            stored_rec(1, 1, INC2, "2026-10-16T08:27:00Z", "agent_message_chunk", text("m2", "session two goes on")),
            stored_rec(2, 2, INC2, "2026-10-16T08:30:00Z", "end", json!({})),
        ]
    }))
    .unwrap()
}

/// The events of a run that lost session 1, went on in session 2 and has ended; `after` are events written between the start of
/// session 2 and the end of the run.
fn two_sessions_ended(after: Vec<Envelope>) -> Vec<Envelope> {
    let mut events = base_events();
    events.push(started(10, "2026-10-16T08:15:10Z", 1));
    events.push(lost_event(false));
    events.push(started(30, "2026-10-16T08:26:40Z", 2));
    events.extend(after);
    events.push(event(
        60,
        "2026-10-16T08:40:00Z",
        EventType::GoalStatus,
        Authority::Harness,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    events
}

/// The run screen gives the stored conversation the start of the run's last session. The `end` record of 08:30 is session 2's, the
/// run's last: the screen ends with "the conversation was closed cleanly" and no notice of an open tail. When the run's events show a
/// session 3 that began after that record and stored nothing, the notice about the last session stays.
#[test]
fn b846_the_run_screen_keeps_the_open_tail_notice_when_the_end_record_is_an_earlier_sessions() {
    let mut app = ready_at(two_sessions_ended(vec![]), "2026-10-16T08:41:00Z");
    app.update(answer(ham_core::stored::Read::First, lost_then_closed()));
    let text = flat(&screen(&app, 160, 60));
    assert!(
        text.contains("[an earlier session stopped without closing its conversation: its last lines may be missing]"),
        "TRIGGER PRESENT: the open tail of session 1 is said where it happened: {text}"
    );
    assert!(
        text.contains("the conversation was closed cleanly"),
        "TRIGGER PRESENT: session 2 closed its conversation: {text}"
    );
    assert!(
        !text.contains(NOT_CLOSED),
        "session 2 is the run's last session and closed its conversation: {text}"
    );
    // Session 3 began at 08:31, after the `end` record of session 2, and stored nothing.
    let mut app = ready_at(
        two_sessions_ended(vec![started(40, "2026-10-16T08:31:00Z", 3)]),
        "2026-10-16T08:41:00Z",
    );
    app.update(answer(ham_core::stored::Read::First, lost_then_closed()));
    let text = flat(&screen(&app, 160, 60));
    assert!(
        text.contains("the conversation was closed cleanly"),
        "{text}"
    );
    assert!(
        text.contains(NOT_CLOSED),
        "the last session closed nothing: {text}"
    );
}

// ---- point 12 on the run screen -------------------------------------------------------------------------------------------------------

/// The detail of the finished run: the stage reported 1,941,678 tokens in all.
fn detail() -> Msg {
    let d: harness_protocol::api::v1::RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "delivering", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-16T08:15:00Z", "ownerPrincipalArn": ARN},
        "tokensUsed": 0, "costUsd": 2.320885,
        "cost": {"pricesVersion": "2026-10-01", "turnsWithoutUsage": 0, "lines": [
            {"modelId": "us.anthropic.claude-opus-5-5", "engine": "cline", "usd": 2.320885, "usage": {
                "inputTokens": 86, "outputTokens": 42_514, "cacheReadInputTokens": 1_715_858, "cacheWriteInputTokens": 183_220}}]}
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

/// The zip of the finished run of `finished_events`, downloaded to `path`.
fn downloaded(path: std::path::PathBuf) -> Msg {
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(ham::pull::Pulled {
            path,
            artifact: harness_protocol::api::v1::ArtifactView {
                artifact_id: "01M5A2R7K3C9XJ4D8E6F0000AA".into(),
                kind: harness_protocol::store::ArtifactKind::Zip,
                s3_uri: format!("s3://b/runs/{RUN}/deliverable/calc.zip"),
                sha256: "ab".repeat(32),
                size_bytes: 1234,
            },
            url: harness_protocol::api::v1::DownloadUrlResponse {
                url: "https://b.s3.us-west-2.amazonaws.com/runs/R1/calc.zip?X-Amz-Expires=900"
                    .into(),
                expires_at: "2026-10-16T09:15:00Z".into(),
            },
            ranges: vec![(0, 1233)],
            resumed_from: 0,
        }),
    }
}

fn prompt_answered(n: u64, tokens: u64) -> Value {
    rec(
        0,
        n,
        "prompt_result",
        json!({"result": {"stopReason": "end_turn", "usage": {"totalTokens": tokens}}}),
    )
}

/// The proof's numbers: the card showed 1,941,678 tokens, the two prompts that answered used 813,995 and 913,896, and no line of
/// any surface showed the 213,787 tokens of the goal-check round. The delivery card of the run screen has a row for them once the
/// stored conversation is read whole.
#[test]
fn b846_the_delivery_card_of_the_run_screen_shows_the_tokens_outside_the_answered_prompts() {
    let dir = std::env::temp_dir().join(format!("b846-card-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("calc.zip");
    std::fs::write(&path, b"no zip: the card reads no test report from it").unwrap();
    let mut app = ready(finished_events());
    app.update(detail());
    app.update(downloaded(path));
    let text = flat(&screen(&app, 160, 60));
    assert!(
        text.contains("Tokens 1941678 USD 2.32"),
        "TRIGGER PRESENT: the card has its Tokens row: {text}"
    );
    assert!(
        !text.contains("Outside"),
        "no stored conversation read yet: {text}"
    );
    app.update(answer(
        ham_core::stored::Read::First,
        page(
            "complete",
            vec![
                msg(0, 0, "m0", "work"),
                prompt_answered(1, 813_995),
                prompt_answered(2, 913_896),
                rec(0, 3, "end", json!({})),
            ],
            None,
            None,
        ),
    ));
    let text = flat(&screen(&app, 160, 60));
    assert!(
        text.contains(
            "Outside 213787 of these tokens were used outside the prompts that answered (goal-check rounds, failed prompts)"
        ),
        "{text}"
    );
}

/// A long run: the run screen opens the stored conversation at its end with one page, and it read older chunks only when the view
/// was scrolled to its top. The sum of the answered prompts needs every chunk, so the card of such a run had no `Outside` row until
/// the user had scrolled to the first line. For a delivered run whose card has a `Tokens` row the screen reads the older chunks as
/// its timer fires, and the row appears without a key.
#[test]
fn b846_the_run_screen_reads_the_whole_conversation_of_a_delivered_run_for_the_outside_row() {
    let dir = std::env::temp_dir().join(format!("b846-whole-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("calc.zip");
    std::fs::write(&path, b"no zip: the card reads no test report from it").unwrap();
    let mut app = ready(finished_events());
    app.update(detail());
    app.update(downloaded(path));
    // The newest page: 80 messages in chunks 10 to 23, then the second answered prompt and the end. Chunks 0 to 9 are older.
    let mut newest: Vec<Value> = (0..80)
        .map(|n| {
            msg(
                10 + n / 6,
                100 + n,
                &format!("m{n}"),
                &format!("message number {n}"),
            )
        })
        .collect();
    newest.push(rec(
        24,
        200,
        "prompt_result",
        json!({"result": {"stopReason": "end_turn", "usage": {"totalTokens": 913_896}}}),
    ));
    newest.push(rec(24, 201, "end", json!({})));
    app.update(answer(
        ham_core::stored::Read::First,
        page("complete", newest, Some(10), None),
    ));
    let text = flat(&screen(&app, 160, 60));
    assert!(
        text.contains("Tokens 1941678 USD 2.32"),
        "TRIGGER PRESENT: the delivery card is on screen with its Tokens row: {text}"
    );
    assert!(
        !text.contains("message number 0 "),
        "TRIGGER PRESENT: the view is at the end of a conversation that does not fit the screen: {text}"
    );
    assert!(
        !text.contains("Outside"),
        "the older chunks are not read yet: {text}"
    );
    // No key is pressed: the timer asks for the chunks before chunk 10.
    let asked = (0..5).find_map(|_| transcript_job(&app.tick()));
    let (query, read) = asked.expect("the older chunks of a delivered run are read without a key");
    assert_eq!(read, ham_core::stored::Read::Older);
    assert_eq!(query.before, Some(ham_core::transcript::Before::Seq(10)));
    // One request at a time.
    for _ in 0..5 {
        assert!(transcript_job(&app.tick()).is_none());
    }
    app.update(answer(
        ham_core::stored::Read::Older,
        page(
            "complete",
            vec![
                msg(0, 0, "o0", "the first words"),
                prompt_answered(1, 813_995),
            ],
            None,
            None,
        ),
    ));
    let text = flat(&screen(&app, 160, 60));
    assert!(
        text.contains(
            "Outside 213787 of these tokens were used outside the prompts that answered (goal-check rounds, failed prompts)"
        ),
        "{text}"
    );
    assert!(
        !text.contains("the first words"),
        "the view stays at the end while the older chunks arrive: {text}"
    );
    // The conversation is read whole: nothing more is asked.
    for _ in 0..20 {
        assert!(transcript_job(&app.tick()).is_none());
    }
}

/// The background read is for the card alone. A run that delivered nothing, a run whose worker declared a loss and a run that
/// still works are read as before: older chunks only when the view is at its top.
#[test]
fn b846_the_run_screen_reads_no_older_chunks_in_the_background_without_a_card_that_needs_them() {
    let newest = || -> Vec<Value> {
        (0..80)
            .map(|n| {
                msg(
                    10 + n / 6,
                    100 + n,
                    &format!("m{n}"),
                    &format!("message number {n}"),
                )
            })
            .collect()
    };
    let quiet = |app: &mut App| {
        let _ = screen(app, 160, 60);
        (0..20).all(|_| {
            app.tick()
                .iter()
                .all(|e| !matches!(e, ham::tui::Effect::Ext(ham::tui::ext::Job::Transcript { read, .. }) if *read == ham_core::stored::Read::Older))
        })
    };
    // A finished run whose detail and zip are not on the card.
    let mut app = ready(finished_events());
    app.update(answer(
        ham_core::stored::Read::First,
        page("complete", newest(), Some(10), None),
    ));
    assert!(quiet(&mut app), "no card with a Tokens row");
    // A delivered run whose worker declared a loss: no sum is possible.
    let dir = std::env::temp_dir().join(format!("b846-nowhole-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("calc.zip");
    std::fs::write(&path, b"no zip: the card reads no test report from it").unwrap();
    let mut app = ready(finished_events());
    app.update(detail());
    app.update(downloaded(path));
    app.update(answer(
        ham_core::stored::Read::First,
        page("incomplete", newest(), Some(10), None),
    ));
    assert!(
        quiet(&mut app),
        "a conversation with a declared loss gives no sum"
    );
}

// ---- the JSON view of `ham watch`: the notices about the copies beside the members a script already reads ---------------------------

/// A script reads `savedCopy`, `silentSince` and `lostSessions` by name. The notices about the copies come as one more member and
/// take none of the others away: a run with a stored copy, a notice after it and a silent session carries all three, and the lost
/// session the service then names is listed as before.
#[test]
fn b846_b842_the_json_view_of_ham_watch_keeps_its_other_members_beside_the_copy_notices() {
    let mut events = working();
    events.push(copy_notice(13, "2026-10-16T08:17:02Z", COPY_NOT_SAVED));
    let silent = view_json(&view_at(events.clone(), "2026-10-16T08:22:02Z"));
    assert_eq!(
        silent["copyNotices"],
        json!([{"at": "2026-10-16T08:17:02Z", "text": COPY_NOT_SAVED, "endRequest": false}]),
        "TRIGGER PRESENT: the view carries the notice: {silent}"
    );
    assert_eq!(silent["savedCopy"]["count"], json!(1), "{silent}");
    assert_eq!(
        silent["savedCopy"]["at"],
        json!("2026-10-16T08:15:31Z"),
        "{silent}"
    );
    assert!(silent.get("silentSince").is_some(), "{silent}");
    // Without the notice the view has the same other members with the same values.
    let plain = view_json(&view_at(working(), "2026-10-16T08:22:02Z"));
    assert_eq!(silent["savedCopy"], plain["savedCopy"], "{silent}");
    assert!(plain.get("silentSince").is_some(), "{plain}");
    // The service calls the session lost: the list of lost sessions is there, and the copy is still named.
    events.push(lost_event(true));
    let lost = view_json(&view_at(events, "2026-10-16T08:26:20Z"));
    assert!(
        lost["lostSessions"]
            .as_array()
            .is_some_and(|l| l.len() == 1),
        "{lost}"
    );
    assert_eq!(lost["savedCopy"], plain["savedCopy"], "{lost}");
}
