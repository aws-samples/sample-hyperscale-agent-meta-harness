//! The send-message experience.
//! Enter in the pinned box sends the text by the run's state: it answers an open question, sends a
//! message that is delivered after the turn, or prepares a follow-up on a finished run. `/now` asks for a second Enter and interrupts.
//! A pending row above the box shows each message with the words of its state; `w` withdraws one nobody has claimed; `x` confirms a
//! message held after a session change with a new message that names it. A definite refusal gives the text back; an uncertain send keeps
//! its id and is asked again by that id. The follow-up start waits for the `continueFrom` request member of the image wave.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::ext::{Done, Job, SendFailure};
use ham::tui::keys::{self, Action, Mode};
use ham::tui::{App, Effect, Msg};
use ham_core::messages::{MessageResponse, MessagesPage, SendMessageResponse};
use harness_protocol::api::v1::InputAccepted;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::messages::{MessageMode, SendMessageRequest};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn mid(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6FM{n:05}")
}

fn record() -> Value {
    let path = format!(
        "{}/../ham-core/tests/fixtures/capability/first-engine.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A working run on an image that offers message control.
fn working() -> App {
    let mut events = base_events();
    events.push(capability_event(4, record()));
    ready(events)
}

fn finished() -> App {
    let mut events = finished_events();
    events.push(capability_event(6, record()));
    ready(events)
}

fn view(n: u32, state: &str, mode: &str, text: &str, extra: Value) -> Value {
    let mut v = json!({"messageId": mid(n), "seq": n, "mode": mode, "state": state, "text": text, "epoch": "s-1", "epochNo": 1,
                       "acceptedAt": "2026-10-16T08:15:10Z", "updatedAt": "2026-10-16T08:15:10Z"});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    v
}

fn page_of(messages: Vec<Value>, epoch_no: u32, epoch: &str) -> MessagesPage {
    serde_json::from_value(json!({"runId": RUN, "messages": messages, "admission": "open", "epoch": epoch, "epochNo": epoch_no, "watermark": 9}))
        .unwrap()
}

fn messages_done(messages: Vec<Value>, epoch_no: u32, epoch: &str) -> Msg {
    Msg::Ext(Done::Messages {
        run_id: RUN.into(),
        result: Ok(page_of(messages, epoch_no, epoch)),
    })
}

fn sent_ok(v: Value) -> Msg {
    let message: harness_protocol::messages::MessageView = serde_json::from_value(v).unwrap();
    Msg::Ext(Done::Sent {
        run_id: RUN.into(),
        message_id: message.message_id.clone(),
        result: Ok(SendMessageResponse {
            outcome: harness_protocol::messages::SendOutcome::Accepted,
            message,
        }),
    })
}

fn sent_err(id: &str, status: u16, code: &str) -> Msg {
    Msg::Ext(Done::Sent {
        run_id: RUN.into(),
        message_id: id.into(),
        result: Err(SendFailure {
            status,
            code: code.into(),
            message: "the server's words".into(),
        }),
    })
}

fn sends(effects: &[Effect]) -> Vec<SendMessageRequest> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Ext(Job::SendMessage { request, .. }) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn row_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no row has {needle:?}:\n{text}"))
}

/// The pending row of the message whose text holds `needle`: it starts with `you` and carries a status after an em dash (a block of the
/// conversation has the same word and no status).
fn pending_row<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with("you") && l.contains(needle) && l.contains('\u{2014}'))
        .unwrap_or_else(|| panic!("no pending row has {needle:?}:\n{text}"))
}

fn tick_until<F: FnMut(&[Effect]) -> bool>(
    app: &mut App,
    limit: usize,
    mut found: F,
) -> Option<usize> {
    (0..limit).find(|_| found(&app.tick()))
}

// ---- Enter by the state of the run ------------------------------------------------------------------------------------------------

#[test]
fn enter_on_a_working_run_sends_the_text_as_a_message_for_after_the_turn() {
    let mut app = working();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "please also add tests");
    let reqs = sends(&effects);
    assert_eq!(reqs.len(), 1, "{effects:?}");
    assert_eq!(reqs[0].text, "please also add tests");
    assert_eq!(reqs[0].mode, MessageMode::Next);
    assert!(reqs[0].confirms.is_none());
    assert_eq!(reqs[0].message_id.len(), 26);
    assert!(harness_protocol::common::is_ulid(&reqs[0].message_id));
    assert_eq!(app.chat_text().as_deref(), Some(""), "the box is emptied");
    let text = screen(&app, 100, 40);
    let row = row_with(&text, "please also add tests");
    assert!(row.starts_with("you"), "{row}");
    assert!(row.contains("\u{2014} sending"), "{row}");
}

#[test]
fn the_box_title_names_what_enter_does() {
    let app = working();
    let text = screen(&app, 120, 40);
    let title = row_with(&text, "Message to Cline");
    assert!(title.contains("Send next to run 01K6A000"), "{title}");
    // A question changes it.
    let mut events = base_events();
    events.push(capability_event(4, record()));
    // (the open question of the correction path)
    events.push(question_event());
    let app = ready(events);
    let title = row_with(&screen(&app, 120, 40), "Message to Cline").to_string();
    assert!(title.contains("Answer question"), "{title}");
    // A finished run offers the follow-up.
    let app = finished();
    let title = row_with(&screen(&app, 140, 40), "Message to Cline").to_string();
    assert!(title.contains("Prepare follow-up"), "{title}");
    assert!(title.contains("new billable run"), "{title}");
}

fn question_event() -> harness_protocol::envelope::Envelope {
    use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01M5A2R7K3C9XJ4D8E6FQ0WT1N".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 0,
        ts: T0.into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({
                "turn": 1, "kind": "needs-input",
                "text": "Needs your input: Cline stopped its own prompt",
                "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
                "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": "x", "runS": 412, "sessions": 1},
                "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
                "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
                "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-16T09:15:00Z"
            }),
        },
    }
}

#[test]
fn an_open_question_is_still_answered_through_the_input_route() {
    let mut events = base_events();
    events.push(capability_event(4, record()));
    events.push(question_event());
    let mut app = ready(events);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "use the other API");
    assert!(
        matches!(effects.as_slice(), [Effect::Answer { .. }]),
        "{effects:?}"
    );
    assert!(sends(&effects).is_empty());
}

#[test]
fn a_double_enter_sends_once() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = send(&mut app, &mut clock, "once");
    assert_eq!(sends(&first).len(), 1);
    let second = enter(&mut app, &mut clock, 400);
    assert!(second.is_empty(), "{second:?}");
}

// ---- what the server answers ------------------------------------------------------------------------------------------------------

#[test]
fn the_accepted_message_waits_for_the_turn_and_is_delivered_in_a_turn() {
    let mut app = working();
    let mut clock = Clock::new();
    let reqs = sends(&send(&mut app, &mut clock, "add a changelog"));
    let id = reqs[0].message_id.clone();
    let accepted = json!({"messageId": id, "seq": 1, "mode": "next", "state": "pending", "text": "add a changelog", "epoch": "s-1", "epochNo": 1,
                          "acceptedAt": T0, "updatedAt": T0});
    app.update(sent_ok(accepted.clone()));
    assert!(
        pending_row(&screen(&app, 100, 40), "add a changelog")
            .contains("waits for this turn to end")
    );
    // The row follows the server's list.
    let mut delivered = accepted;
    delivered["state"] = json!("delivered");
    delivered["deliveredTurn"] = json!(4);
    app.update(messages_done(vec![delivered], 1, "s-1"));
    assert!(pending_row(&screen(&app, 100, 40), "add a changelog").contains("delivered in turn 4"));
}

#[test]
fn a_definite_refusal_gives_the_text_back_and_says_why() {
    let mut app = working();
    let mut clock = Clock::new();
    let reqs = sends(&send(&mut app, &mut clock, "last minute thought"));
    app.update(sent_err(&reqs[0].message_id, 409, "run-sealed"));
    assert_eq!(app.chat_text().as_deref(), Some("last minute thought"));
    let text = screen(&app, 120, 40);
    assert!(text.contains("the run is ending"), "{text}");
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("you") && l.contains("last minute thought")),
        "no pending row is left: {text}"
    );
}

#[test]
fn an_uncertain_send_keeps_its_id_shows_sending_and_is_asked_again_by_that_id() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = sends(&send(&mut app, &mut clock, "did this arrive"));
    let id = first[0].message_id.clone();
    app.update(sent_err(&id, 0, ""));
    // The text is not put back as a new message; the row stays "sending".
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(row_with(&screen(&app, 100, 40), "did this arrive").contains("sending"));
    // It is asked again with the same id and the same payload.
    let mut again = None;
    tick_until(&mut app, 12, |e| {
        again = sends(e).into_iter().next();
        again.is_some()
    })
    .expect("the send is asked again");
    let again = again.unwrap();
    assert_eq!(again.message_id, id);
    assert_eq!(again.payload_sha256(), first[0].payload_sha256());
}

/// Asks for the send again until `limit` ticks have passed: the request, if one was made.
fn next_ask(app: &mut App, limit: usize) -> Option<SendMessageRequest> {
    let mut found = None;
    tick_until(app, limit, |e| {
        found = sends(e).into_iter().next();
        found.is_some()
    })?;
    found
}

#[test]
fn an_uncertain_send_is_never_given_up_and_its_text_is_never_put_back_as_a_fresh_message() {
    // "An uncertain send, such as a timeout, keeps the message id, shows the payload as sending and is reconciled by id; the
    // text is not restored as a fresh message."
    let mut app = working();
    let mut clock = Clock::new();
    let first = sends(&send(&mut app, &mut clock, "did this arrive"));
    let id = first[0].message_id.clone();
    app.update(sent_err(&id, 0, ""));
    // Far more tries than the old limit of 5: the service stays silent.
    for n in 0..12 {
        let again = next_ask(&mut app, 120)
            .unwrap_or_else(|| panic!("the send is asked again after {n} failed tries"));
        assert_eq!(again.message_id, id, "the same id every time");
        assert_eq!(again.payload_sha256(), first[0].payload_sha256());
        app.update(sent_err(&id, if n % 2 == 0 { 503 } else { 0 }, ""));
        let text = screen(&app, 120, 40);
        let row = row_with(&text, "did this arrive");
        assert!(row.starts_with("you"), "{row}");
        assert!(row.contains("\u{2014} sending"), "try {n}: {row}");
        assert!(
            !text.contains("not delivered") && !text.contains("was not delivered"),
            "try {n}: an uncertain send is never called not delivered:\n{text}"
        );
        assert_eq!(
            app.chat_text().as_deref(),
            Some(""),
            "try {n}: the text is not put back in the box"
        );
    }
}

#[test]
fn a_silent_service_is_asked_less_often_the_longer_it_stays_silent() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = sends(&send(&mut app, &mut clock, "slow service"));
    let id = first[0].message_id.clone();
    app.update(sent_err(&id, 503, ""));
    let mut waits = Vec::new();
    for _ in 0..9 {
        let mut ticks = 0usize;
        let found = loop {
            ticks += 1;
            if ticks > 200 {
                panic!("not asked again within 200 ticks: {waits:?}");
            }
            if let Some(r) = sends(&app.tick()).into_iter().next() {
                break r;
            }
        };
        assert_eq!(found.message_id, id);
        waits.push(ticks);
        app.update(sent_err(&id, 503, ""));
    }
    assert!(
        waits.windows(2).all(|w| w[0] <= w[1]),
        "the pauses never shrink: {waits:?}"
    );
    assert!(
        waits.last() > waits.first() && waits.iter().all(|w| *w <= 61),
        "the pauses grow and stay under a minute: {waits:?}"
    );
}

#[test]
fn an_uncertain_send_is_reconciled_by_its_id_when_the_service_holds_it() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = sends(&send(&mut app, &mut clock, "it did arrive"));
    let id = first[0].message_id.clone();
    app.update(sent_err(&id, 0, ""));
    // The answer was lost, but the message was stored: the list holds it under the same id.
    let mut stored = view(1, "pending", "next", "it did arrive", json!({}));
    stored["messageId"] = json!(id);
    app.update(messages_done(vec![stored], 1, "s-1"));
    let text = screen(&app, 120, 40);
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("it did arrive") && l.starts_with("you"))
        .collect();
    assert_eq!(rows.len(), 1, "one row, the server's: {text}");
    assert!(
        rows[0].contains("waits for this turn to end"),
        "{}",
        rows[0]
    );
    assert_eq!(app.chat_text().as_deref(), Some(""));
    // Nothing asks again: the send is settled.
    assert!(next_ask(&mut app, 120).is_none());
}

#[test]
fn only_a_definite_refusal_puts_the_text_back_even_after_uncertain_tries() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = sends(&send(&mut app, &mut clock, "refused in the end"));
    let id = first[0].message_id.clone();
    app.update(sent_err(&id, 0, ""));
    for _ in 0..7 {
        next_ask(&mut app, 120).expect("asked again");
        app.update(sent_err(&id, 503, ""));
    }
    assert_eq!(app.chat_text().as_deref(), Some(""));
    next_ask(&mut app, 120).expect("asked again");
    app.update(sent_err(&id, 409, "run-sealed"));
    assert_eq!(app.chat_text().as_deref(), Some("refused in the end"));
    let text = screen(&app, 120, 40);
    assert!(text.contains("the run is ending"), "{text}");
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("you") && l.contains("refused in the end")),
        "no row is left: {text}"
    );
}

#[test]
fn a_double_enter_while_the_service_is_silent_sends_one_message() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = send(&mut app, &mut clock, "once only");
    assert_eq!(sends(&first).len(), 1);
    let id = sends(&first)[0].message_id.clone();
    app.update(sent_err(&id, 0, ""));
    // The person presses Enter again on the emptied box, and again: nothing new is sent.
    for wait in [30, 400, 2000] {
        let e = enter(&mut app, &mut clock, wait);
        assert!(sends(&e).is_empty(), "{e:?}");
    }
    // Only the retry of the first message goes out, with its own id.
    let again = next_ask(&mut app, 120).expect("the retry");
    assert_eq!(again.message_id, id);
}

#[test]
fn a_run_on_an_older_image_keeps_the_text_and_says_cline_is_not_waiting() {
    let mut r = record();
    r["harness"]["controls"] = json!([]);
    let mut events = base_events();
    events.push(capability_event(4, r));
    let mut app = ready(events);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "are you there");
    assert!(
        sends(&effects).is_empty() && effects.is_empty(),
        "{effects:?}"
    );
    assert_eq!(app.chat_text().as_deref(), Some("are you there"));
    let text = screen(&app, 120, 40);
    assert!(
        text.contains("Cline is not waiting for an answer"),
        "{text}"
    );
    assert!(text.contains("/cancel"), "{text}");
    assert!(
        row_with(&text, "Message to Cline").contains("Not available"),
        "{text}"
    );
    // The same when the server says so: an image with no record at all.
    let mut app = ready(base_events());
    let mut clock = Clock::new();
    let reqs = sends(&send(&mut app, &mut clock, "hello"));
    app.update(sent_err(&reqs[0].message_id, 409, "no-message-control"));
    assert_eq!(app.chat_text().as_deref(), Some("hello"));
    assert!(screen(&app, 120, 40).contains("Cline is not waiting for an answer"));
}

// ---- the words of every state -----------------------------------------------------------------------------------------------------

#[test]
fn every_state_the_server_reports_has_words_on_the_pending_row() {
    // (state, mode, extra members, words the row must hold, words it must not hold)
    type Case = (
        &'static str,
        &'static str,
        Value,
        Vec<&'static str>,
        Vec<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "pending",
            "next",
            json!({}),
            vec!["waits for this turn to end"],
            vec![],
        ),
        (
            "pending",
            "now",
            json!({}),
            vec!["sending: waiting to interrupt Cline"],
            vec![],
        ),
        (
            "held",
            "next",
            json!({"epochNo": 2}),
            vec!["written to session 2, now session 3", "x"],
            vec![],
        ),
        (
            "dispatching",
            "next",
            json!({}),
            vec!["sending: Cline is taking it now"],
            vec![],
        ),
        (
            "delivered",
            "next",
            json!({"deliveredTurn": 4}),
            vec!["delivered in turn 4"],
            vec![],
        ),
        ("withdrawn", "next", json!({}), vec!["withdrawn"], vec![]),
        (
            "rejected",
            "next",
            json!({"reason": "run-sealed"}),
            vec!["not delivered", "the run was ending"],
            vec![],
        ),
        (
            "superseded",
            "next",
            json!({}),
            vec!["replaced by a confirmed copy"],
            vec![],
        ),
        (
            "failed",
            "next",
            json!({"detail": "the engine refused it"}),
            vec!["could not be sent", "the engine refused it"],
            vec![],
        ),
        (
            "delivery-unknown",
            "next",
            json!({}),
            vec!["may have reached Cline"],
            vec!["not delivered"],
        ),
    ];
    for (state, mode, extra, want, never) in cases {
        let mut app = working();
        app.update(messages_done(
            vec![view(1, state, mode, "state test message", extra)],
            3,
            "s-3",
        ));
        let text = screen(&app, 120, 40);
        let row = pending_row(&text, "state test message");
        assert!(row.starts_with("you"), "{state}: {row}");
        for w in &want {
            assert!(row.contains(w), "{state}: {w:?} not in {row}");
        }
        for w in &never {
            assert!(!row.contains(w), "{state}: {w:?} in {row}");
        }
    }
}

// ---- withdraw and confirm ---------------------------------------------------------------------------------------------------------

#[test]
fn w_withdraws_a_message_nobody_has_claimed() {
    let mut app = working();
    app.update(messages_done(
        vec![view(1, "pending", "next", "never mind this", json!({}))],
        1,
        "s-1",
    ));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let effects = press(&mut app, &mut clock, KeyCode::Char('w'), 50);
    let [Effect::Ext(Job::Withdraw { run_id, message_id })] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(
        (run_id.as_str(), message_id.as_str()),
        (RUN, mid(1).as_str())
    );
    let withdrawn: harness_protocol::messages::MessageView =
        serde_json::from_value(view(1, "withdrawn", "next", "never mind this", json!({}))).unwrap();
    app.update(Msg::Ext(Done::Withdrawn {
        run_id: RUN.into(),
        message_id: mid(1),
        result: Ok(MessageResponse { message: withdrawn }),
    }));
    assert!(row_with(&screen(&app, 100, 40), "never mind this").contains("withdrawn"));
}

#[test]
fn a_withdrawal_after_the_claim_shows_the_real_state() {
    let mut app = working();
    app.update(messages_done(
        vec![view(1, "pending", "next", "too late", json!({}))],
        1,
        "s-1",
    ));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('w'), 50);
    app.update(Msg::Ext(Done::Withdrawn {
        run_id: RUN.into(),
        message_id: mid(1),
        result: Err(SendFailure {
            status: 409,
            code: "already-dispatching".into(),
            message: "Cline has taken it".into(),
        }),
    }));
    let text = screen(&app, 100, 40);
    assert!(text.contains("already"), "the refusal is said: {text}");
    // The list is read again, and the row shows what it is now.
    let effects = app.tick();
    assert!(
        jobs(&effects)
            .iter()
            .any(|j| matches!(j, Job::Messages { .. }))
            || {
                let more: Vec<_> = (0..4).flat_map(|_| app.tick()).collect();
                jobs(&more)
                    .iter()
                    .any(|j| matches!(j, Job::Messages { .. }))
            }
    );
    app.update(messages_done(
        vec![view(1, "dispatching", "next", "too late", json!({}))],
        1,
        "s-1",
    ));
    assert!(
        row_with(&screen(&app, 100, 40), "too late").contains("sending: Cline is taking it now")
    );
}

#[test]
fn x_confirms_a_held_message_with_a_new_message_that_names_it_and_the_session_shown() {
    let mut app = working();
    app.update(messages_done(
        vec![view(
            1,
            "held",
            "next",
            "run the linter too",
            json!({"epochNo": 2}),
        )],
        3,
        "s-3",
    ));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let effects = press(&mut app, &mut clock, KeyCode::Char('x'), 50);
    assert!(
        sends(&effects).is_empty(),
        "a preview comes first: {effects:?}"
    );
    let text = screen(&app, 140, 40);
    assert!(
        text.contains("run the linter too"),
        "the full text is in the preview: {text}"
    );
    assert!(
        text.contains("session 2") && text.contains("session 3"),
        "{text}"
    );
    assert!(text.contains(RUN), "the run is named: {text}");
    // Esc cancels; x again, then Enter after the delay confirms.
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('x'), 400);
    let soon = press(&mut app, &mut clock, KeyCode::Enter, 100);
    assert!(
        sends(&soon).is_empty(),
        "not within 300 ms of the preview: {soon:?}"
    );
    let done = press(&mut app, &mut clock, KeyCode::Enter, 400);
    let reqs = sends(&done);
    assert_eq!(reqs.len(), 1, "{done:?}");
    assert_eq!(reqs[0].text, "run the linter too");
    assert_ne!(reqs[0].message_id, mid(1));
    let c = reqs[0]
        .confirms
        .as_ref()
        .expect("it names the held message");
    assert_eq!(
        (c.message_id.as_str(), c.epoch.as_str()),
        (mid(1).as_str(), "s-3")
    );
}

#[test]
fn a_message_that_may_have_reached_cline_warns_before_it_is_sent_again() {
    let mut app = working();
    app.update(messages_done(
        vec![view(1, "delivery-unknown", "next", "deploy it", json!({}))],
        1,
        "s-1",
    ));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('x'), 50);
    let text = screen(&app, 140, 40);
    assert!(text.contains("may already have run"), "{text}");
}

// ---- /now ----------------------------------------------------------------------------------------------------------------------

#[test]
fn now_asks_for_a_second_enter_and_then_interrupts() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = send(&mut app, &mut clock, "/now stop the sleep");
    assert!(sends(&first).is_empty(), "{first:?}");
    let text = screen(&app, 140, 40);
    assert!(text.contains("Interrupt"), "{text}");
    assert!(text.contains("stop the sleep"), "{text}");
    // Too soon, and Esc, send nothing.
    assert!(sends(&enter(&mut app, &mut clock, 100)).is_empty());
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(sends(&enter(&mut app, &mut clock, 400)).is_empty());
    // Again.
    let mut app = working();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/now stop the sleep");
    let done = enter(&mut app, &mut clock, 400);
    let reqs = sends(&done);
    assert_eq!(reqs.len(), 1, "{done:?}");
    assert_eq!(reqs[0].text, "stop the sleep");
    assert_eq!(reqs[0].mode, MessageMode::Now);
}

#[test]
fn now_is_refused_when_the_session_does_not_offer_it() {
    let mut r = record();
    r["harness"]["controls"] = json!(["message.next"]);
    let mut events = base_events();
    events.push(capability_event(4, r));
    let mut app = ready(events);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/now stop");
    let later = enter(&mut app, &mut clock, 400);
    assert!(sends(&later).is_empty(), "{later:?}");
    assert!(
        screen(&app, 120, 40).contains("does not take"),
        "{}",
        screen(&app, 120, 40)
    );
}

// ---- the context -------------------------------------------------------------------------------------------------------------

#[test]
fn a_change_of_context_just_before_enter_blocks_it_and_never_retargets_the_text() {
    let mut app = working();
    // A clock that starts now, so the Enter below is 200 ms after the last key and calm.
    let mut clock = Clock::early();
    type_text(&mut app, &mut clock, "x");
    // The run finishes at this instant; Enter follows 250 ms later.
    // The earlier events of the set-up are more than 300 ms old when the change happens, so only the change can block Enter.
    std::thread::sleep(Duration::from_millis(350));
    let t0 = Instant::now();
    feed(&mut app, completion_events());
    let at = t0 + Duration::from_millis(250);
    let blocked = app.handle_key_at(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), at);
    assert!(blocked.is_empty(), "{blocked:?}");
    assert_eq!(app.chat_text().as_deref(), Some("x"), "the text stays");
    assert!(sends(&blocked).is_empty());
    // After the pause Enter acts on what the box says now: a follow-up is prepared, nothing is sent as a message.
    let later = app.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        t0 + Duration::from_millis(900),
    );
    assert!(sends(&later).is_empty(), "{later:?}");
    assert!(
        screen(&app, 140, 40).contains("Prepare follow-up"),
        "{}",
        screen(&app, 140, 40)
    );
}

// ---- the follow-up ----------------------------------------------------------------------------------------------------------------

#[test]
fn enter_on_a_finished_run_prepares_the_follow_up_in_the_box_and_starts_nothing() {
    let mut app = finished();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "now add the docs");
    // Nothing starts. The one effect is the read of the engine file's settings hash (no network, nothing sent), for the confirmation's line about
    // changed engine settings.
    assert!(
        effects
            .iter()
            .all(|e| matches!(e, Effect::Ext(Job::EngineSettings { .. }))),
        "{effects:?}"
    );
    let text = screen(&app, 140, 40);
    assert!(
        text.contains("Cline is finished. Enter = start a follow-up run from this result."),
        "{text}"
    );
    assert!(text.contains("The new Cline starts a new session and sees a summary of this chat, not the whole history."), "{text}");
    let draft = app.chat_text().unwrap();
    assert!(draft.starts_with("now add the docs"), "{draft}");
    assert!(
        draft.contains("quoted record"),
        "the digest is a quoted record, not instructions: {draft}"
    );
    assert!(
        draft.contains("Cline reported done"),
        "the outcome label is in it: {draft}"
    );
    // The panel after the first Enter names what the second one starts: a new billable run.
    assert!(
        text.contains("new billable run"),
        "the confirmation is on the screen before the second Enter: {text}"
    );
}

fn starts(effects: &[Effect]) -> Vec<(String, String)> {
    jobs(effects)
        .into_iter()
        .filter_map(|j| match j {
            Job::StartFollowUp { run_id, text, .. } => Some((run_id.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

fn flat(s: &str) -> String {
    s.chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_offer_shows_as_soon_as_the_box_holds_text_on_a_finished_run() {
    const OFFER: &str = "Cline is finished. Enter = start a follow-up run from this result.";
    const NOTE: &str = "The new Cline starts a new session and sees a summary of this chat, not the whole history.";
    let mut app = finished();
    let mut clock = Clock::new();
    let empty = flat(&screen(&app, 100, 40));
    assert!(
        !empty.contains(OFFER),
        "no offer with an empty box: {empty}"
    );
    type_text(&mut app, &mut clock, "thanks, please also add a README");
    let typed = flat(&screen(&app, 100, 40));
    assert!(typed.contains(OFFER), "{typed}");
    assert!(typed.contains(NOTE), "{typed}");
    // A command is not a message: no offer while the box holds a slash command.
    let mut command = finished();
    let mut clock = Clock::new();
    type_text(&mut command, &mut clock, "/engines");
    assert!(!flat(&screen(&command, 100, 40)).contains(OFFER));
    // A working run offers nothing of the kind.
    let mut working = working();
    let mut clock = Clock::new();
    type_text(&mut working, &mut clock, "thanks, please also add a README");
    assert!(!flat(&screen(&working, 100, 40)).contains(OFFER));
}

#[test]
fn the_second_enter_starts_the_follow_up_with_exactly_the_text_in_the_box() {
    let mut app = finished();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    let early = enter(&mut app, &mut clock, 100);
    assert!(
        starts(&early).is_empty(),
        "a key within 300 ms of the preview is blocked: {early:?}"
    );
    // The person edits the prepared text before the second Enter.
    type_text(&mut app, &mut clock, " and a changelog");
    let draft = app.chat_text().unwrap();
    assert!(
        draft.contains("quoted record") && draft.ends_with("and a changelog"),
        "{draft}"
    );
    // The second Enter looks for a follow-up that is already going; none is, so the run starts with exactly this text.
    let second = enter(&mut app, &mut clock, 400);
    assert!(starts(&second).is_empty(), "{second:?}");
    let found = app.update(Msg::Ext(Done::FollowUpFound {
        run_id: RUN.into(),
        result: Ok(None),
    }));
    assert_eq!(starts(&found), vec![(RUN.to_string(), draft)], "{found:?}");
}

#[test]
fn a_confirmation_that_was_dismissed_asks_again_instead_of_starting() {
    let mut app = finished();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    let again = enter(&mut app, &mut clock, 400);
    assert!(starts(&again).is_empty(), "{again:?}");
    assert!(
        flat(&screen(&app, 140, 40)).contains("new billable run"),
        "the confirmation is shown again: {}",
        screen(&app, 140, 40)
    );
    let third = enter(&mut app, &mut clock, 400);
    let found = app.update(Msg::Ext(Done::FollowUpFound {
        run_id: RUN.into(),
        result: Ok(None),
    }));
    let all: Vec<Effect> = third.into_iter().chain(found).collect();
    assert_eq!(starts(&all).len(), 1, "{all:?}");
}

#[test]
fn a_follow_up_that_started_opens_the_new_run() {
    const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";
    let mut app = finished();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    enter(&mut app, &mut clock, 400);
    let done = app.update(Msg::Ext(Done::FollowUpStarted {
        run_id: RUN.into(),
        result: Ok(NEW.into()),
    }));
    assert!(
        jobs(&done)
            .iter()
            .any(|j| matches!(j, Job::Transcript { run_id, .. } if run_id == NEW)),
        "the new run is opened (its conversation is asked for): {done:?}"
    );
}

#[test]
fn a_follow_up_that_did_not_start_keeps_the_text_and_says_why() {
    let mut app = finished();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    let draft = app.chat_text().unwrap();
    enter(&mut app, &mut clock, 400);
    app.update(Msg::Ext(Done::FollowUpStarted {
        run_id: RUN.into(),
        result: Err("409 no-delivered-head: run delivered no code to continue from".into()),
    }));
    assert_eq!(app.chat_text().as_deref(), Some(draft.as_str()));
    let text = screen(&app, 140, 40);
    assert!(text.contains("no-delivered-head"), "{text}");
    assert!(text.contains("your text is kept"), "{text}");
}

// ---- the conversation shows what was said ------------------------------------------------------------------------------

#[test]
fn a_delivered_message_stands_in_the_conversation_as_the_users_own_block() {
    let mut app = working();
    app.update(answer(
        ham_core::stored::Read::First,
        page(
            "live",
            vec![
                msg(0, 0, "a", "Starting."),
                msg(0, 1, "b", "Second answer after the message."),
            ],
            None,
            Some("c"),
        ),
    ));
    app.update(messages_done(
        vec![view(
            1,
            "delivered",
            "next",
            "please add tests",
            json!({"deliveredTurn": 2, "acceptedAt": "2026-10-16T08:15:00Z"}),
        )],
        1,
        "s-1",
    ));
    let text = screen(&app, 100, 40);
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("please add tests"))
        .collect();
    assert!(!rows.is_empty(), "{text}");
    assert!(rows.iter().all(|r| r.starts_with("you")), "{rows:?}");
    // After the row has been shown for a while it goes, and the conversation still holds the message.
    for _ in 0..40 {
        app.tick();
    }
    let later = screen(&app, 100, 40);
    let n = later
        .lines()
        .filter(|l| l.contains("please add tests"))
        .count();
    assert_eq!(n, 1, "only the conversation block is left: {later}");
}

#[test]
fn reopening_a_run_shows_the_messages_that_are_still_on_their_way() {
    let (mut app, opened) = ready_with({
        let mut e = base_events();
        e.push(capability_event(4, record()));
        e
    });
    assert!(
        jobs(&opened)
            .iter()
            .any(|j| matches!(j, Job::Messages { run_id } if run_id == RUN))
    );
    app.update(messages_done(
        vec![view(
            1,
            "pending",
            "next",
            "sent before the kill",
            json!({}),
        )],
        1,
        "s-1",
    ));
    assert!(
        row_with(&screen(&app, 100, 40), "sent before the kill")
            .contains("waits for this turn to end")
    );
}

// ---- an answer to a question is a sent message too --------------------------------------------------

/// The request id of the question `question_event` opens.
const QUESTION: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

/// A run that waits for the person: one open question.
fn asking() -> App {
    let mut events = base_events();
    events.push(capability_event(4, record()));
    events.push(question_event());
    ready(events)
}

fn accepted_answer() -> Msg {
    Msg::Answered(Ok(InputAccepted {
        run_id: RUN.into(),
        request_id: QUESTION.into(),
        state: "queued".into(),
    }))
}

/// A `turn.progress` event of the direct agent whose id sorts after the question's.
fn progress_event(n: u32, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01M5C{n:021}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 0,
        ts: T0.into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline { value: v },
    }
}

/// Cline took the answer: the question is closed.
fn answer_received(n: u32) -> Envelope {
    progress_event(
        n,
        json!({"turn": 1, "kind": "input-received", "text": "Your message reached Cline.", "requestId": QUESTION,
               "action": "continue_with_prompt", "origin": "user", "textBytes": 19, "waitedS": 3}),
    )
}

/// The rows of `text` that hold `needle` and start with the word `you` (a pending row or a block of the conversation).
fn you_rows<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|l| l.starts_with("you") && l.contains(needle))
        .collect()
}

const ANSWER: &str = "use design A please";

#[test]
fn an_answer_typed_in_the_box_shows_on_a_pending_row_while_it_is_sent() {
    let mut app = asking();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, ANSWER);
    assert!(
        matches!(effects.as_slice(), [Effect::Answer { .. }]),
        "{effects:?}"
    );
    assert_eq!(app.chat_text().as_deref(), Some(""), "the box is emptied");
    let text = screen(&app, 100, 40);
    let rows = you_rows(&text, ANSWER);
    assert_eq!(rows.len(), 1, "one pending row: {text}");
    assert!(rows[0].contains("\u{2014} sending"), "{}", rows[0]);
}

#[test]
fn an_accepted_answer_waits_on_its_row_and_stands_in_the_conversation() {
    let mut app = asking();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, ANSWER);
    app.update(accepted_answer());
    let text = screen(&app, 100, 40);
    let rows = you_rows(&text, ANSWER);
    assert_eq!(
        rows.len(),
        2,
        "the pending row and the user's own block in the conversation: {text}"
    );
    let pending: Vec<&&str> = rows.iter().filter(|r| r.contains('\u{2014}')).collect();
    assert_eq!(pending.len(), 1, "{rows:?}");
    assert!(
        pending[0].contains("sending: waiting for Cline to take it"),
        "{}",
        pending[0]
    );
}

#[test]
fn once_cline_has_taken_the_answer_its_row_goes_and_the_conversation_keeps_it() {
    let mut app = asking();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, ANSWER);
    app.update(accepted_answer());
    feed(&mut app, vec![answer_received(1)]);
    let text = screen(&app, 100, 40);
    let rows = you_rows(&text, ANSWER);
    assert_eq!(rows.len(), 1, "only the conversation block is left: {text}");
    assert!(
        !rows[0].contains('\u{2014}'),
        "no status on a block: {}",
        rows[0]
    );
}

#[test]
fn a_refused_answer_gives_the_text_back_and_leaves_no_row() {
    let mut app = asking();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, ANSWER);
    let why = "this question was already answered with finish";
    app.update(Msg::Answered(Err(why.into())));
    assert_eq!(
        app.chat_text().as_deref(),
        Some(ANSWER),
        "a refusal keeps the typed text"
    );
    let text = screen(&app, 120, 40);
    assert!(text.contains(why), "{text}");
    assert!(
        you_rows(&text, ANSWER).is_empty(),
        "no pending row and no block for an answer nobody took: {text}"
    );
}

#[test]
fn an_accepted_answer_is_placed_by_its_time_among_the_stored_records() {
    let mut app = asking();
    app.set_now(parse_utc_unix("2026-10-16T08:15:30Z").unwrap());
    app.update(answer(
        ham_core::stored::Read::First,
        page(
            "live",
            vec![
                msg(0, 0, "a", "Early engine text."),
                msg(0, 45, "b", "Late engine text."),
            ],
            None,
            Some("c"),
        ),
    ));
    let mut clock = Clock::new();
    send(&mut app, &mut clock, ANSWER);
    app.update(accepted_answer());
    let text = screen(&app, 100, 40);
    let at = |needle: &str| {
        text.lines()
            .position(|l| l.contains(needle) && !l.contains('\u{2014}'))
            .unwrap_or_else(|| panic!("no row has {needle:?}:\n{text}"))
    };
    let (early, mine, late) = (
        at("Early engine text."),
        at(ANSWER),
        at("Late engine text."),
    );
    assert!(
        early < mine && mine < late,
        "08:15:00 < 08:15:30 < 08:15:45, rows {early} {mine} {late}:\n{text}"
    );
}

#[test]
fn an_accepted_answer_is_placed_by_its_time_in_a_conversation_that_is_only_the_progress_feed() {
    // The engine's later words carry a later time than the answer, so the answer stands before them. (The ULID of the event is its time.)
    let mut events = base_events();
    events.push(capability_event(4, record()));
    events.push(question_event());
    events.push(Envelope {
        event_id: "01M5C00000000000000000MA01".into(),
        ..progress_event(
            0,
            json!({"turn": 1, "kind": "message", "text": "Late engine words."}),
        )
    });
    let mut app = ready(events);
    app.set_now(parse_utc_unix("2026-10-16T08:15:30Z").unwrap());
    let mut clock = Clock::new();
    send(&mut app, &mut clock, ANSWER);
    app.update(accepted_answer());
    let text = screen(&app, 100, 50);
    let at = |needle: &str| {
        text.lines()
            .position(|l| l.contains(needle) && !l.contains('\u{2014}'))
            .unwrap_or_else(|| panic!("no row has {needle:?}:\n{text}"))
    };
    assert!(at(ANSWER) < at("Late engine words."), "{text}");
    // Without a clock the answer goes last.
    let mut events = base_events();
    events.push(capability_event(4, record()));
    events.push(question_event());
    events.push(Envelope {
        event_id: "01M5C00000000000000000MA01".into(),
        ..progress_event(
            0,
            json!({"turn": 1, "kind": "message", "text": "Late engine words."}),
        )
    });
    let mut app = ready(events);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, ANSWER);
    app.update(accepted_answer());
    let text = screen(&app, 100, 50);
    let at = |needle: &str| {
        text.lines()
            .position(|l| l.contains(needle) && !l.contains('\u{2014}'))
            .unwrap_or_else(|| panic!("no row has {needle:?}:\n{text}"))
    };
    assert!(at("Late engine words.") < at(ANSWER), "{text}");
}

// ---- Enter is bound to what the box showed ---------------------------------------------------------------------------

#[test]
fn the_title_names_the_run_and_the_interrupt_when_the_draft_is_a_now_command() {
    let mut app = working();
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/now stop the sleep");
    let text = screen(&app, 140, 40);
    let title = row_with(&text, "Message to Cline");
    assert!(
        title.contains("Enter: Interrupt run 01K6A000 (/now)"),
        "{title}"
    );
}

#[test]
fn the_title_of_a_finished_run_names_the_commit_once_it_is_known() {
    let mut app = finished();
    let title = row_with(&screen(&app, 140, 40), "Message to Cline").to_string();
    assert!(
        title.contains("Prepare follow-up from the delivered result: new billable run"),
        "no commit is known yet: {title}"
    );
    // The head route (another package) fills the head; a stand-in value shows what the header does with it.
    app.set_delivered_head(Some("9d2f1a7c4b5e6f708192a3b4c5d6e7f801234567".into()));
    let title = row_with(&screen(&app, 140, 40), "Message to Cline").to_string();
    assert!(
        title.contains("Prepare follow-up from commit 9d2f1a7: new billable run"),
        "{title}"
    );
}

/// A closed question and the next one, as one poll brings them.
fn question_replaced() -> Vec<Envelope> {
    const NEXT: &str = "01M5B8T4W2N6QJ3C7D9EH1FZ0A";
    let second = Envelope {
        event_id: NEXT.into(),
        ..progress_event(
            0,
            json!({"turn": 1, "kind": "needs-input", "text": "Needs your input: a second stop", "requestId": NEXT, "n": 2,
                   "reason": "engine_stop", "title": "Cline stopped again", "stopOutcome": "stopped_engine_cancelled",
                   "evidence": {"attempts": 3, "toolCalls": 9, "filesChanged": 1, "identicalCalls": 2, "failedTurns": null,
                                "lastError": "x", "runS": 500, "sessions": 1},
                   "lastMessage": "Waiting.", "files": [], "filesTotal": 0, "blocking": true,
                   "options": ["continue_with_prompt", "finish"], "models": [], "recommended": null, "canContinue": true,
                   "waitS": 1800, "expiresAt": "2026-10-16T09:45:00Z"}),
        )
    };
    vec![
        progress_event(
            1,
            json!({"turn": 1, "kind": "input-closed", "text": "closed", "requestId": QUESTION, "how": "expired", "waitedS": 1800}),
        ),
        second,
    ]
}

#[test]
fn a_question_that_changes_just_before_enter_blocks_it_and_never_retargets_the_text() {
    let mut app = asking();
    let mut clock = Clock::early();
    type_text(&mut app, &mut clock, "x");
    // The earlier events of the set-up are more than 300 ms old when the change happens, so only the change can block Enter.
    std::thread::sleep(Duration::from_millis(350));
    let t0 = Instant::now();
    feed(&mut app, question_replaced());
    let blocked = app.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        t0 + Duration::from_millis(250),
    );
    assert!(blocked.is_empty(), "{blocked:?}");
    assert_eq!(app.chat_text().as_deref(), Some("x"), "the text stays");
    let later = app.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        t0 + Duration::from_millis(900),
    );
    assert!(
        matches!(later.as_slice(), [Effect::Answer { answer, .. }] if answer.request_id != QUESTION),
        "after the pause Enter answers the question the box shows now: {later:?}"
    );
}

#[test]
fn a_new_session_just_before_enter_blocks_it_and_never_retargets_the_text() {
    let mut app = working();
    app.update(messages_done(vec![], 1, "s-1"));
    let mut clock = Clock::early();
    type_text(&mut app, &mut clock, "x");
    // The earlier events of the set-up are more than 300 ms old when the change happens, so only the change can block Enter.
    std::thread::sleep(Duration::from_millis(350));
    let t0 = Instant::now();
    // The session is replaced at this instant: the epoch moves from 1 to 2.
    app.update(messages_done(vec![], 2, "s-2"));
    let blocked = app.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        t0 + Duration::from_millis(250),
    );
    assert!(blocked.is_empty(), "{blocked:?}");
    assert_eq!(app.chat_text().as_deref(), Some("x"), "the text stays");
    let later = app.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        t0 + Duration::from_millis(900),
    );
    assert_eq!(sends(&later).len(), 1, "{later:?}");
}

#[test]
fn a_blocked_enter_flashes_the_title_for_a_moment() {
    let mut app = working();
    let mut clock = Clock::early();
    type_text(&mut app, &mut clock, "x");
    // The earlier events of the set-up are more than 300 ms old when the change happens, so only the change can block Enter.
    std::thread::sleep(Duration::from_millis(350));
    let t0 = Instant::now();
    feed(&mut app, completion_events());
    let title_now = |app: &App| row_with(&screen(app, 160, 40), "Message to Cline").to_string();
    assert!(!title_now(&app).contains("changed just now"));
    app.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        t0 + Duration::from_millis(250),
    );
    let flashed = title_now(&app);
    assert!(flashed.contains("changed just now"), "{flashed}");
    for _ in 0..4 {
        app.tick();
    }
    let calm = title_now(&app);
    assert!(!calm.contains("changed just now"), "{calm}");
}

// ---- held rows: one is focused at a time, and x confirms exactly that one --------------------------------------------------

fn two_held() -> App {
    let mut app = working();
    app.update(messages_done(
        vec![
            view(1, "held", "next", "first held text", json!({"epochNo": 2})),
            view(2, "held", "next", "second held text", json!({"epochNo": 2})),
        ],
        3,
        "s-3",
    ));
    app
}

fn browse(app: &mut App, clock: &mut Clock) {
    press(app, clock, KeyCode::Esc, 50);
}

#[test]
fn the_message_keys_have_rows_in_the_key_table() {
    let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    assert_eq!(keys::lookup(Mode::Browse, &k('w')), Some(Action::Withdraw));
    assert_eq!(
        keys::lookup(Mode::Browse, &k('x')),
        Some(Action::ConfirmHeld)
    );
    assert_eq!(
        keys::lookup(Mode::Browse, &k('h')),
        Some(Action::ChooseHeld)
    );
    let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
    for mode in [Mode::Browse, Mode::Typing] {
        assert_eq!(
            keys::lookup(mode, &ctrl_o),
            Some(Action::OpenAnything),
            "{mode:?}"
        );
    }
    // Letters stay text in typing mode.
    for c in ['w', 'x', 'h'] {
        assert_eq!(keys::lookup(Mode::Typing, &k(c)), None, "{c}");
    }
}

#[test]
fn with_two_held_messages_x_chooses_first_and_only_the_chosen_row_is_confirmed() {
    let mut app = two_held();
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    // Two rows are held and none is chosen: x never guesses. It marks the first and shows no preview.
    let first = press(&mut app, &mut clock, KeyCode::Char('x'), 400);
    assert!(sends(&first).is_empty(), "{first:?}");
    let text = screen(&app, 160, 40);
    assert!(!text.contains("Send this message again?"), "{text}");
    assert!(
        row_with(&text, "first held text").contains("chosen"),
        "{text}"
    );
    assert!(
        !row_with(&text, "second held text").contains("chosen"),
        "{text}"
    );
    // h moves the choice to the other row; exactly one row is chosen at any time.
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    let text = screen(&app, 160, 40);
    assert!(
        !row_with(&text, "first held text").contains("chosen"),
        "{text}"
    );
    assert!(
        row_with(&text, "second held text").contains("chosen"),
        "{text}"
    );
    let chosen_rows = text
        .lines()
        .filter(|l| l.starts_with("you") && l.contains("chosen"))
        .count();
    assert_eq!(chosen_rows, 1, "{text}");
    // x now shows the preview of the chosen row only, with its full text, the run and both sessions.
    press(&mut app, &mut clock, KeyCode::Char('x'), 100);
    let text = screen(&app, 160, 40);
    assert!(text.contains("Send this message again?"), "{text}");
    assert!(text.contains("Text: second held text"), "{text}");
    assert!(!text.contains("Text: first held text"), "{text}");
    assert!(text.contains(RUN), "{text}");
    assert!(
        text.contains("session 2") && text.contains("session 3"),
        "{text}"
    );
    let done = press(&mut app, &mut clock, KeyCode::Enter, 400);
    let reqs = sends(&done);
    assert_eq!(reqs.len(), 1, "{done:?}");
    assert_eq!(reqs[0].text, "second held text");
    let c = reqs[0].confirms.as_ref().expect("it names the held row");
    assert_eq!(
        (c.message_id.as_str(), c.epoch.as_str()),
        (mid(2).as_str(), "s-3")
    );
}

#[test]
fn h_wraps_around_the_held_rows() {
    let mut app = two_held();
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    let mut chosen = Vec::new();
    for _ in 0..3 {
        press(&mut app, &mut clock, KeyCode::Char('h'), 100);
        let text = screen(&app, 160, 40);
        chosen.push(if row_with(&text, "first held text").contains("chosen") {
            1
        } else {
            2
        });
    }
    assert_eq!(chosen, [1, 2, 1]);
}

#[test]
fn w_withdraws_the_chosen_row_and_without_a_choice_the_newest() {
    let mut app = two_held();
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    let effects = press(&mut app, &mut clock, KeyCode::Char('w'), 50);
    let [Effect::Ext(Job::Withdraw { message_id, .. })] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(message_id, &mid(2), "no choice: the newest");
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    let effects = press(&mut app, &mut clock, KeyCode::Char('w'), 50);
    let [Effect::Ext(Job::Withdraw { message_id, .. })] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(message_id, &mid(1), "the chosen row");
}

#[test]
fn w_on_a_message_that_is_still_on_its_way_says_so_and_sends_nothing() {
    let mut app = working();
    let mut clock = Clock::new();
    let first = send(&mut app, &mut clock, "not yet accepted");
    assert_eq!(sends(&first).len(), 1);
    browse(&mut app, &mut clock);
    let effects = press(&mut app, &mut clock, KeyCode::Char('w'), 400);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 160, 40);
    assert!(text.contains("still on its way"), "{text}");
}

#[test]
fn w_never_swaps_a_chosen_message_that_cannot_be_withdrawn_for_another() {
    let mut app = working();
    app.update(messages_done(
        vec![
            view(1, "held", "next", "held one", json!({"epochNo": 2})),
            view(2, "delivery-unknown", "next", "unknown one", json!({})),
            view(3, "pending", "next", "pending one", json!({})),
        ],
        3,
        "s-3",
    ));
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    // With no choice w takes the newest message nobody has claimed.
    let first = press(&mut app, &mut clock, KeyCode::Char('w'), 400);
    assert!(
        matches!(first.as_slice(), [Effect::Ext(Job::Withdraw { message_id, .. })] if message_id == &mid(3)),
        "{first:?}"
    );
    // The choice is the message that may have reached Cline: w refuses it and does not take the pending one instead.
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    let text = screen(&app, 160, 40);
    assert!(row_with(&text, "unknown one").contains("chosen"), "{text}");
    let refused = press(&mut app, &mut clock, KeyCode::Char('w'), 400);
    assert!(refused.is_empty(), "{refused:?}");
    let text = screen(&app, 160, 40);
    assert!(text.contains("cannot be withdrawn"), "{text}");
}

#[test]
fn a_single_held_message_needs_no_choice() {
    let mut app = working();
    app.update(messages_done(
        vec![view(
            1,
            "held",
            "next",
            "only held text",
            json!({"epochNo": 2}),
        )],
        3,
        "s-3",
    ));
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    press(&mut app, &mut clock, KeyCode::Char('x'), 400);
    assert!(screen(&app, 160, 40).contains("Send this message again?"));
    let text = screen(&app, 160, 40);
    assert!(
        !row_with(&text, "only held text").contains("chosen"),
        "{text}"
    );
}

#[test]
fn a_confirmation_after_another_session_change_sends_nothing() {
    let mut app = two_held();
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    press(&mut app, &mut clock, KeyCode::Char('x'), 100);
    assert!(screen(&app, 160, 40).contains("Send this message again?"));
    // While the preview is on screen the session is replaced again: the list now names session 4.
    app.update(messages_done(
        vec![
            view(1, "held", "next", "first held text", json!({"epochNo": 2})),
            view(2, "held", "next", "second held text", json!({"epochNo": 2})),
        ],
        4,
        "s-4",
    ));
    let done = press(&mut app, &mut clock, KeyCode::Enter, 400);
    assert!(sends(&done).is_empty(), "no text is sent: {done:?}");
    let text = screen(&app, 160, 40);
    assert!(text.contains("the session changed again"), "{text}");
    assert!(!text.contains("Send this message again?"), "{text}");
}

#[test]
fn a_confirmation_for_a_message_that_is_no_longer_waiting_sends_nothing() {
    let mut app = two_held();
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    press(&mut app, &mut clock, KeyCode::Char('x'), 100);
    assert!(screen(&app, 160, 40).contains("Send this message again?"));
    // The chosen message was withdrawn from another client while the preview was open; the session is the same.
    app.update(messages_done(
        vec![
            view(
                1,
                "withdrawn",
                "next",
                "first held text",
                json!({"epochNo": 2}),
            ),
            view(2, "held", "next", "second held text", json!({"epochNo": 2})),
        ],
        3,
        "s-3",
    ));
    let done = press(&mut app, &mut clock, KeyCode::Enter, 400);
    assert!(sends(&done).is_empty(), "{done:?}");
    assert!(
        screen(&app, 160, 40).contains("no longer waiting for you"),
        "{}",
        screen(&app, 160, 40)
    );
}

#[test]
fn the_message_keys_say_so_when_there_is_nothing_to_act_on() {
    let mut app = working();
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    for (key, words) in [
        ('w', "no message is waiting to be withdrawn"),
        ('x', "no message is waiting for your confirmation"),
        ('h', "no message is held"),
    ] {
        let effects = press(&mut app, &mut clock, KeyCode::Char(key), 400);
        assert!(effects.is_empty(), "{key}: {effects:?}");
        let text = screen(&app, 160, 40);
        assert!(text.contains(words), "{key}: {text}");
    }
    // One held message needs no choice: h says what x does.
    app.update(messages_done(
        vec![view(
            1,
            "held",
            "next",
            "only held text",
            json!({"epochNo": 2}),
        )],
        3,
        "s-3",
    ));
    let effects = press(&mut app, &mut clock, KeyCode::Char('h'), 400);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 160, 40);
    assert!(text.contains("one message is held"), "{text}");
}

#[test]
fn messages_that_may_have_reached_cline_are_chosen_like_held_ones() {
    let mut app = working();
    app.update(messages_done(
        vec![
            view(1, "delivery-unknown", "next", "unknown one", json!({})),
            view(2, "delivery-unknown", "next", "unknown two", json!({})),
        ],
        1,
        "s-1",
    ));
    let mut clock = Clock::new();
    browse(&mut app, &mut clock);
    let text = screen(&app, 160, 40);
    assert!(
        pending_row(&text, "unknown one").contains("(h chooses)"),
        "{text}"
    );
    assert!(
        pending_row(&text, "unknown two").contains("(h chooses)"),
        "{text}"
    );
    press(&mut app, &mut clock, KeyCode::Char('h'), 100);
    let text = screen(&app, 160, 40);
    assert!(
        pending_row(&text, "unknown one").contains("may have reached Cline, chosen"),
        "{text}"
    );
    assert!(
        pending_row(&text, "unknown two").contains("(h chooses)"),
        "{text}"
    );
}

#[test]
fn the_footer_names_the_message_keys_while_a_message_can_be_acted_on() {
    let mut clock = Clock::new();
    // Nothing waits: the footer is the old one.
    let mut app = working();
    browse(&mut app, &mut clock);
    let plain = screen(&app, 160, 40);
    assert!(
        !plain.contains("w withdraw") && !plain.contains("x confirm"),
        "{plain}"
    );
    // A pending message: w.
    let mut app = working();
    app.update(messages_done(
        vec![view(1, "pending", "next", "still waiting", json!({}))],
        1,
        "s-1",
    ));
    browse(&mut app, &mut clock);
    let text = screen(&app, 160, 40);
    let footer = text.lines().last().unwrap().to_string();
    assert!(footer.contains("w withdraw"), "{footer}");
    assert!(!footer.contains("x confirm"), "{footer}");
    // In typing mode the keys are behind Esc, and the footer says so.
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    let typing = screen(&app, 160, 40);
    let footer = typing.lines().last().unwrap().to_string();
    assert!(footer.contains("Esc, then w withdraw"), "{footer}");
    // Two held messages: x and h.
    let mut app = two_held();
    browse(&mut app, &mut clock);
    let text = screen(&app, 160, 40);
    let footer = text.lines().last().unwrap().to_string();
    assert!(
        footer.contains("x confirm")
            && footer.contains("h choose")
            && footer.contains("w withdraw"),
        "{footer}"
    );
}

/// The footer row of the typing screen at `width` columns: the last row of a 40-row screen.
fn typing_footer_at(app: &App, width: u16) -> String {
    screen(app, width, 40).lines().last().unwrap().to_string()
}

/// `app` in typing mode: Esc to browse mode, then `i`.
fn into_typing(mut app: App, clock: &mut Clock) -> App {
    browse(&mut app, clock);
    press(&mut app, clock, KeyCode::Char('i'), 50);
    app
}

// Words `Esc browse` with the hotkeys that follow it adds ` · Esc, then w withdraw` (and `, x confirm, h choose`) at the end of
// the typing footer while a message can be acted on. The wording is chosen for the line as it is drawn, so the message keys are not cut off
// (the end of a line that is too wide is replaced by an ellipsis). With one message the usual line (67 columns) and its 23-column ending need
// 90 columns, the wordings need 103, 107 and 120; with all three keys the ending has 44 columns and the same wordings need 111, 124,
// 128 and 141.

#[test]
fn the_typing_footer_keeps_the_withdraw_key_at_every_width_and_takes_the_longest_wording_that_fits()
{
    let mut clock = Clock::new();
    let mut app = working();
    app.update(messages_done(
        vec![view(1, "pending", "next", "still waiting", json!({}))],
        1,
        "s-1",
    ));
    let app = into_typing(app, &mut clock);
    for width in 90..=160u16 {
        let footer = typing_footer_at(&app, width);
        assert!(
            footer.ends_with("Esc, then w withdraw"),
            "{width} columns: {footer:?}"
        );
        assert!(
            !footer.contains('\u{2026}') && footer.chars().count() <= usize::from(width),
            "{width} columns: {footer:?}"
        );
        let wording = match width {
            120.. => "Esc browse, then a artifacts, c c cancel \u{b7} Ctrl-C",
            107..=119 => "Esc browse, then c c cancel \u{b7} Ctrl-C",
            103..=106 => "Esc browse (c c cancel) \u{b7} Ctrl-C",
            _ => "Esc browse \u{b7} Ctrl-C",
        };
        assert!(
            footer.contains(wording),
            "{width} columns: wanted {wording:?} in {footer:?}"
        );
    }
}

#[test]
fn the_typing_footer_keeps_all_three_message_keys_when_the_line_has_room_for_them() {
    let mut clock = Clock::new();
    let app = into_typing(two_held(), &mut clock);
    for width in 111..=160u16 {
        let footer = typing_footer_at(&app, width);
        assert!(
            footer.ends_with("Esc, then w withdraw, x confirm, h choose"),
            "{width} columns: {footer:?}"
        );
        assert!(
            !footer.contains('\u{2026}') && footer.chars().count() <= usize::from(width),
            "{width} columns: {footer:?}"
        );
        let wording = match width {
            141.. => "Esc browse, then a artifacts, c c cancel \u{b7} Ctrl-C",
            128..=140 => "Esc browse, then c c cancel \u{b7} Ctrl-C",
            124..=127 => "Esc browse (c c cancel) \u{b7} Ctrl-C",
            _ => "Esc browse \u{b7} Ctrl-C",
        };
        assert!(
            footer.contains(wording),
            "{width} columns: wanted {wording:?} in {footer:?}"
        );
    }
}

#[test]
fn without_a_message_to_act_on_the_typing_footer_keeps_its_old_widths() {
    // The room the message keys need is taken only while there are message keys.
    let mut clock = Clock::new();
    let app = into_typing(working(), &mut clock);
    for (width, wording) in [
        (
            97u16,
            "Esc browse, then a artifacts, c c cancel \u{b7} Ctrl-C",
        ),
        (96, "Esc browse, then c c cancel \u{b7} Ctrl-C"),
        (84, "Esc browse, then c c cancel \u{b7} Ctrl-C"),
        (83, "Esc browse (c c cancel) \u{b7} Ctrl-C"),
        (80, "Esc browse (c c cancel) \u{b7} Ctrl-C"),
        (79, "Esc browse \u{b7} Ctrl-C"),
    ] {
        let footer = typing_footer_at(&app, width);
        assert!(
            footer.contains(wording) && !footer.contains("Esc, then"),
            "{width} columns: wanted {wording:?} in {footer:?}"
        );
    }
}

// ---- /now ---------------------------------------------------------------------------------------------------------

#[test]
fn the_now_preview_tells_the_person_to_restate_what_the_message_refers_to() {
    let mut app = working();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/now stop the sleep");
    let text = flat(&screen(&app, 160, 40));
    assert!(
        text.contains(
            "A cancelled turn leaves no partial text in Cline's memory, so restate what your message refers to."
        ),
        "{text}"
    );
}
