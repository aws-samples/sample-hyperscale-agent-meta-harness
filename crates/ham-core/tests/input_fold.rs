//! The "Cline needs you" state of a run, folded from its event log: the open question, the ones
//! that ended, the sessions, and the plain card. Events are built as the golden files of `fixtures/direct-input/` write them.
#![allow(clippy::unwrap_used)]

use ham_core::input::{
    InputRequest, check_text, ignored_phrase, left_text, outcome_phrase, resolve_offered,
};
use ham_core::render::status_line;
use ham_core::state::{RunState, RunView};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const EXPIRES: &str = "2026-10-15T14:11:07Z";

/// ULIDs that sort by `n`.
fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, agent: Option<&str>, value: Value) -> Envelope {
    let mut e = json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "seq": n, "ts": "2026-10-15T13:41:07Z",
        "type": ty, "authority": authority, "payload": {"kind": "inline", "value": value}
    });
    if let Some(a) = agent {
        e["agentId"] = json!(a);
    }
    serde_json::from_value(e).unwrap()
}

fn progress(n: u32, value: Value) -> Envelope {
    event(n, "turn.progress", "agent", Some("direct"), value)
}

fn payload(reason: &str, extra: Value) -> Value {
    let mut v = json!({
        "turn": 1, "kind": "needs-input", "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
        "requestId": id(10), "n": 1, "reason": reason, "title": "Cline stopped its own prompt",
        "stopOutcome": "stopped_engine_cancelled",
        "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                     "lastError": "the engine ended the prompt cancelled", "runS": 412, "sessions": 1},
        "lastMessage": "I will keep polling the build until it finishes, then append step 4 to PROGRESS.md.",
        "files": ["PROGRESS.md", "src/app.py", "tests/test_app.py"], "filesTotal": 3, "blocking": true,
        "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
        "models": [{"id": "us.anthropic.claude-opus-5-5", "name": "Claude Opus 5.5", "contextTokens": 1000000},
                   {"id": "global.anthropic.claude-sonnet-5-5", "name": "Claude Sonnet 5.5", "contextTokens": 1000000}],
        "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": EXPIRES
    });
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    v
}

fn question(n: u32, reason: &str, extra: Value) -> Envelope {
    let mut p = payload(reason, extra);
    p["requestId"] = json!(id(n));
    // The event id is the request id.
    let mut e = progress(n, p);
    e.event_id = id(n);
    e
}

fn view_of(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn received(n: u32, request: u32, action: &str) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "input-received", "text": "Your message reached Cline (same session).",
               "requestId": id(request), "action": action, "origin": "user", "textBytes": 88, "waitedS": 93}),
    )
}

fn closed(n: u32, request: u32, how: &str) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "input-closed", "text": "closed", "requestId": id(request), "how": how, "waitedS": 1800}),
    )
}

#[test]
fn a_run_that_never_asked_has_an_empty_input_view_and_one_session() {
    let v = view_of(vec![event(
        1,
        "session.started",
        "harness",
        None,
        json!({"agentId": "direct", "engine": "cline", "modelId": "global.anthropic.claude-sonnet-5-5"}),
    )]);
    assert!(v.input.is_empty());
    assert!(v.input.waiting().is_none());
    assert_eq!(v.sessions, 1);
    assert_eq!(v.restarted, None);
}

#[test]
fn an_open_question_carries_the_reason_the_words_and_the_numbers() {
    let v = view_of(vec![question(10, "engine_stop", json!({}))]);
    let q = v.input.waiting().expect("a waiting question");
    assert_eq!(q.request_id, id(10));
    assert_eq!((q.n, q.reason.as_str()), (1, "engine_stop"));
    assert_eq!(q.title, "Cline stopped its own prompt");
    assert_eq!(q.headline(), "Cline stopped its own prompt.");
    assert!(
        q.words.starts_with("Cline ended the prompt itself, twice."),
        "{}",
        q.words
    );
    assert_eq!(q.numbers(), "41 tool calls, 3 files changed");
    assert_eq!(
        q.files_line().as_deref(),
        Some("Files: PROGRESS.md src/app.py tests/test_app.py")
    );
    assert!(q.blocking && q.can_continue);
    assert_eq!(q.options.len(), 4);
    assert_eq!(q.models.len(), 2);
    assert_eq!(
        q.last_message.as_deref(),
        Some(
            "I will keep polling the build until it finishes, then append\nstep 4 to PROGRESS.md."
        ),
        "wrapped at 64 columns so the label \"Last words: \" and the text fit a card line"
    );
    assert_eq!(v.input.history.len(), 0);
}

#[test]
fn one_tool_call_and_one_file_are_singular() {
    let v = view_of(vec![question(
        10,
        "no_changes",
        json!({"evidence": {"attempts": 1, "toolCalls": 1, "filesChanged": 1, "runS": 3, "sessions": 1}, "filesTotal": 1, "files": ["a.py"]}),
    )]);
    assert_eq!(
        v.input.waiting().unwrap().numbers(),
        "1 tool call, 1 file changed"
    );
}

#[test]
fn every_string_from_the_agent_side_is_sanitized() {
    let v = view_of(vec![question(
        10,
        "engine_stop",
        json!({
            "title": "Cline \u{1b}]0;title\u{7}stopped",
            "lastMessage": "line one \u{1b}[31mred\u{1b}[0m\nline\u{202e} two",
            "files": ["a\u{1b}[2Jb.py"],
            "evidence": {"attempts": 1, "toolCalls": 2, "filesChanged": 1, "lastError": "bad\u{1b}[0m thing", "runS": 1, "sessions": 1},
            "models": [{"id": "us.x\u{7}.y", "name": "N\u{1b}[0mame", "contextTokens": 1}],
            "reason": "engine_stop"
        }),
    )]);
    let q = v.input.waiting().unwrap();
    assert_eq!(q.title, "Cline stopped");
    assert_eq!(q.last_message.as_deref(), Some("line one red\nline two"));
    assert_eq!(q.files, vec!["ab.py".to_string()]);
    assert_eq!(q.evidence.last_error.as_deref(), Some("bad thing"));
    assert_eq!(q.models[0].id, "us.x.y");
    assert_eq!(q.models[0].name, "Name");
    for line in q.card_lines() {
        assert!(
            !line.contains('\u{1b}') && !line.contains('\u{202e}'),
            "{line:?}"
        );
    }
}

#[test]
fn last_words_are_cut_to_twelve_lines_of_at_most_sixty_four_columns() {
    let long: String = (1..=30).map(|n| format!("line {n}\n")).collect();
    let v = view_of(vec![question(
        10,
        "engine_stop",
        json!({ "lastMessage": long }),
    )]);
    let q = v.input.waiting().unwrap();
    let lines = q.last_words();
    assert_eq!(lines.len(), 12);
    assert_eq!(lines[11], "line 12\u{2026}");
}

#[test]
fn an_unknown_reason_code_shows_the_title_of_the_event() {
    let v = view_of(vec![question(
        10,
        "brand_new_reason",
        json!({ "title": "Something new happened" }),
    )]);
    let q = v.input.waiting().unwrap();
    assert_eq!(q.reason, "brand_new_reason");
    assert_eq!(q.title, "Something new happened");
    assert_eq!(q.words, "Something new happened");
}

#[test]
fn seconds_left_come_from_the_expiry_and_the_clients_clock_never_from_a_clock_time() {
    let mut s = RunState::new(RUN);
    s.apply(question(10, "engine_stop", json!({})));
    let end = parse_utc_unix(EXPIRES).unwrap();
    assert_eq!(s.view().input.waiting().unwrap().secs_left, None);
    let v = s.view_at(Some(end - 1620));
    let q = v.input.waiting().unwrap();
    assert_eq!(q.secs_left, Some(1620));
    assert_eq!(
        q.left_text().as_deref(),
        Some("about 27 min left to answer")
    );
    assert_eq!(q.secs_left_at(end - 59), Some(59));
    // Past the end, and a client clock ahead of the server's, read zero rather than negative.
    assert_eq!(
        s.view_at(Some(end + 5)).input.waiting().unwrap().secs_left,
        Some(0)
    );
    assert_eq!(left_text(0), "no time left to answer");
    assert_eq!(left_text(59), "less than a minute left to answer");
    assert_eq!(left_text(60), "about 1 min left to answer");
    assert_eq!(left_text(89), "about 1 min left to answer");
    assert_eq!(left_text(90), "about 2 min left to answer");
}

#[test]
fn an_answer_closes_the_question_and_leaves_a_history_line() {
    let v = view_of(vec![
        question(10, "engine_stop", json!({})),
        received(11, 10, "continue_with_prompt"),
    ]);
    assert!(v.input.open.is_none());
    let h = &v.input.history;
    assert_eq!(h.len(), 1);
    assert_eq!((h[0].request_id.as_str(), h[0].n), (id(10).as_str(), 1));
    assert_eq!(h[0].how, "answered:continue_with_prompt");
    assert_eq!(h[0].phrase, "answered with a message");
}

#[test]
fn finish_is_received_then_closed_as_finish() {
    let v = view_of(vec![
        question(10, "engine_stop", json!({})),
        received(11, 10, "finish"),
        closed(12, 10, "finish"),
    ]);
    assert!(v.input.open.is_none());
    assert_eq!(v.input.history.len(), 1);
    assert_eq!(v.input.history[0].how, "finish");
    assert_eq!(v.input.history[0].phrase, "answered with finish");
}

#[test]
fn nobody_answering_closes_the_question_as_expired() {
    let v = view_of(vec![
        question(10, "no_changes", json!({})),
        closed(11, 10, "expired"),
    ]);
    assert!(v.input.open.is_none());
    assert_eq!(
        v.input.history[0].phrase,
        "closed when nobody answered in time"
    );
}

#[test]
fn a_newer_question_replaces_the_open_one() {
    let v = view_of(vec![
        question(10, "engine_stop", json!({})),
        question(20, "no_changes", json!({"n": 2})),
    ]);
    assert_eq!(v.input.open.as_ref().unwrap().request_id, id(20));
    assert_eq!(v.input.history.len(), 1);
    assert_eq!(v.input.history[0].how, "superseded");
}

#[test]
fn the_end_of_the_run_closes_the_question() {
    let v = view_of(vec![
        question(10, "engine_stop", json!({})),
        event(
            30,
            "run.completed",
            "harness",
            None,
            json!({"status": "complete", "outcome": "stopped_no_changes"}),
        ),
    ]);
    assert!(v.input.open.is_none());
    assert_eq!(v.input.history[0].how, "ended");
}

#[test]
fn an_ignored_answer_changes_nothing() {
    let v = view_of(vec![
        question(10, "engine_stop", json!({})),
        progress(
            11,
            json!({"turn": 1, "kind": "input-ignored", "text": "no", "requestId": id(10), "why": "bad-model"}),
        ),
    ]);
    assert_eq!(v.input.open.as_ref().unwrap().request_id, id(10));
    assert!(v.input.history.is_empty());
}

#[test]
fn the_outage_notice_does_not_block_and_has_no_keys() {
    let v = view_of(vec![question(
        10,
        "provider_outage",
        json!({"blocking": false, "options": [], "models": [], "canContinue": false, "waitS": 0, "expiresAt": null,
               "title": "Every model route is down", "lastMessage": null, "files": [], "filesTotal": 9}),
    )]);
    assert!(v.input.waiting().is_none());
    let q = v.input.open.as_ref().expect("the notice is still shown");
    assert!(!q.blocking);
    assert_eq!(q.secs_left, None);
    assert!(q.commands(RUN).is_empty());
    assert!(
        !status_line(&v).contains("needs input"),
        "{}",
        status_line(&v)
    );
}

#[test]
fn fresh_sessions_are_counted_and_the_latest_is_named() {
    let ns = |n: u32, session: u32, trigger: &str| {
        progress(
            n,
            json!({"turn": 1, "kind": "new-session", "text": "Restarted from the work tree: fresh Cline session 2.",
                   "requestId": null, "session": session, "trigger": trigger, "reason": "replay_rejected",
                   "restartedProcess": false, "digestBytes": 9210, "promptBytes": 11540}),
        )
    };
    let v = view_of(vec![ns(10, 2, "harness")]);
    assert_eq!(v.sessions, 2);
    let r = v.restarted.as_ref().unwrap();
    assert_eq!((r.session, r.trigger.as_str()), (2, "harness"));
    assert_eq!(r.line(), "Session 2 \u{b7} restarted from the work tree");
    assert_eq!(r.watch_line(), "restarted from the work tree: session 2");
    let v = view_of(vec![ns(10, 2, "harness"), ns(20, 3, "user")]);
    assert_eq!(v.sessions, 3);
    assert_eq!(v.restarted.as_ref().unwrap().session, 3);
}

#[test]
fn corrupt_input_events_are_skipped() {
    let v = view_of(vec![
        progress(10, json!({"turn": 1, "kind": "needs-input", "reason": 7})),
        progress(11, json!({"turn": 1, "kind": "input-received"})),
        progress(
            12,
            json!({"turn": 1, "kind": "new-session", "session": "two"}),
        ),
        question(20, "engine_stop", json!({})),
    ]);
    assert_eq!(v.input.open.as_ref().unwrap().request_id, id(20));
    assert_eq!(v.sessions, 1);
}

#[test]
fn the_status_line_names_a_question_that_waits() {
    let v = view_of(vec![question(10, "engine_stop", json!({}))]);
    assert!(
        status_line(&v).ends_with(" needs input: Cline stopped its own prompt"),
        "{}",
        status_line(&v)
    );
}

fn request_with(options: &[&str], can_continue: bool) -> InputRequest {
    let v = view_of(vec![question(
        10,
        "engine_died",
        json!({ "options": options, "canContinue": can_continue }),
    )]);
    v.input.open.unwrap()
}

#[test]
fn a_question_offers_only_what_it_lists_and_a_dead_session_takes_no_message() {
    let q = request_with(
        &[
            "continue_with_prompt",
            "new_session_with_prompt",
            "try_model",
            "finish",
        ],
        true,
    );
    for a in [
        InstructAction::ContinueWithPrompt,
        InstructAction::NewSessionWithPrompt,
        InstructAction::TryModel,
        InstructAction::Finish,
    ] {
        assert!(q.offers(a), "{a:?}");
    }
    let q = request_with(&["new_session_with_prompt", "finish"], false);
    assert!(!q.offers(InstructAction::ContinueWithPrompt));
    assert!(!q.offers(InstructAction::TryModel));
    assert!(q.offers(InstructAction::NewSessionWithPrompt) && q.offers(InstructAction::Finish));
    // `canContinue: false` withdraws a message and a model switch even when the list still names them.
    let q = request_with(&["continue_with_prompt", "try_model", "finish"], false);
    assert!(!q.offers(InstructAction::ContinueWithPrompt) && !q.offers(InstructAction::TryModel));
    assert!(q.offers(InstructAction::Finish));
}

#[test]
fn the_commands_listed_are_the_offered_actions() {
    let q = request_with(
        &[
            "continue_with_prompt",
            "new_session_with_prompt",
            "try_model",
            "finish",
        ],
        true,
    );
    let cmds = q.commands(RUN);
    assert_eq!(cmds.len(), 4, "{cmds:?}");
    assert!(cmds[0].starts_with(&format!("ham say {RUN} '<message>'")));
    assert!(cmds[1].starts_with(&format!("ham new-session {RUN} ['<message>']")));
    assert!(cmds[2].starts_with(&format!("ham try-model {RUN} <model> ['<message>']")));
    assert!(
        cmds[2].contains("opus") && cmds[2].contains("sonnet"),
        "{}",
        cmds[2]
    );
    assert!(cmds[3].starts_with(&format!("ham finish {RUN}")));
    let q = request_with(&["new_session_with_prompt", "finish"], false);
    assert_eq!(q.commands(RUN).len(), 2);
}

#[test]
fn a_typed_model_is_matched_to_a_route_the_question_offers() {
    let q = request_with(&["try_model", "finish"], true);
    // An alias maps to the offered route of that model.
    assert_eq!(
        resolve_offered("opus", &q.models).unwrap().id,
        "us.anthropic.claude-opus-5-5"
    );
    assert_eq!(
        resolve_offered("Sonnet", &q.models).unwrap().id,
        "global.anthropic.claude-sonnet-5-5"
    );
    // An offered id is taken as typed; an id of the same model's other route finds the offered one.
    assert_eq!(
        resolve_offered("us.anthropic.claude-opus-5-5", &q.models)
            .unwrap()
            .id,
        "us.anthropic.claude-opus-5-5"
    );
    assert_eq!(
        resolve_offered("global.anthropic.claude-opus-5-5", &q.models)
            .unwrap()
            .id,
        "us.anthropic.claude-opus-5-5"
    );
    // A model the question does not offer, an unknown name and an empty list say what to type.
    let e = resolve_offered("sol", &q.models).unwrap_err();
    assert!(e.contains("opus") && e.contains("sonnet"), "{e}");
    let e = resolve_offered("gpt-9", &q.models).unwrap_err();
    assert!(e.contains("gpt-9") && e.contains("opus"), "{e}");
    assert!(
        resolve_offered("opus", &[])
            .unwrap_err()
            .contains("no other model")
    );
}

#[test]
fn the_text_of_an_answer_follows_the_servers_limits() {
    assert!(check_text("go on").is_ok());
    // The old limits were 8,000 characters and 32,000 bytes; the text now has a limit in bytes only: 262,144.
    assert!(check_text(&"x".repeat(8_001)).is_ok());
    assert!(check_text(&"x".repeat(262_144)).is_ok());
    assert!(
        check_text(&"x".repeat(262_145))
            .unwrap_err()
            .contains("262,145 bytes")
    );
    assert!(check_text("   ").unwrap_err().contains("empty"));
    // 4 bytes per character: 65,536 characters are 262,144 bytes, and one more byte is over.
    assert!(check_text(&"\u{1f600}".repeat(65_536)).is_ok());
    assert!(check_text(&format!("{}a", "\u{1f600}".repeat(65_536))).is_err());
    assert!(check_text(&"\u{1f600}".repeat(65_537)).is_err());
}

#[test]
fn the_phrases_cover_every_way_a_question_ends_and_every_ignored_reason() {
    for (how, want) in [
        ("answered:continue_with_prompt", "answered with a message"),
        (
            "answered:new_session_with_prompt",
            "answered with a fresh session",
        ),
        ("answered:try_model", "answered with another model"),
        ("finish", "answered with finish"),
        ("expired", "closed when nobody answered in time"),
        ("cancelled", "closed by your cancel"),
        ("deadline", "closed at the run's time limit"),
        ("superseded", "replaced by a newer question"),
        ("recovered", "closed when a model route answered again"),
        ("ended", "closed when the run ended"),
        ("something_new", "closed (something_new)"),
    ] {
        assert_eq!(outcome_phrase(how), want, "{how}");
    }
    for (why, want) in [
        ("window-closed", "the answer window is closed"),
        ("unknown-request", "that question is not open"),
        ("already-answered", "that question was already answered"),
        ("session-dead", "its session is gone"),
        ("bad-action", "it cannot do that now"),
        ("bad-model", "that model is not one of this run's routes"),
        (
            "limit",
            "the limit of questions or fresh sessions is reached",
        ),
        ("odd", "refused (odd)"),
    ] {
        assert_eq!(ignored_phrase(why), want, "{why}");
    }
}

#[test]
fn every_card_line_fits_seventy_six_columns_and_the_file_count_is_honest() {
    let files: Vec<String> = (0..12)
        .map(|i| format!("src/some/long/path/number{i}/file_{i}.py"))
        .collect();
    let v = view_of(vec![question(
        10,
        "engine_stop",
        json!({ "lastMessage": "word ".repeat(80), "files": files, "filesTotal": 40 }),
    )]);
    let q = v.input.waiting().unwrap();
    let lines = q.card_lines();
    assert!(lines.len() > 6, "{lines:?}");
    for l in &lines {
        assert!(l.chars().count() <= 76, "{l:?}");
    }
    assert!(q.files_line().unwrap().ends_with("(+28 more)"));
    assert_eq!(lines[0], "Cline stopped its own prompt.");
    assert!(lines.iter().any(|l| l.starts_with("Last words: word word")));
}

#[test]
fn the_analyzers_question_is_a_card_line_when_the_event_carries_one() {
    let v = view_of(vec![question(
        10,
        "engine_stop",
        json!({ "question": "Should Cline keep polling the build or start the tests?", "recommended": "new_session_with_prompt" }),
    )]);
    let q = v.input.waiting().unwrap();
    assert_eq!(
        q.question.as_deref(),
        Some("Should Cline keep polling the build or start the tests?")
    );
    assert_eq!(q.recommended, Some(InstructAction::NewSessionWithPrompt));
    assert!(
        q.card_lines()
            .iter()
            .any(|l| l == "Should Cline keep polling the build or start the tests?")
    );
}
