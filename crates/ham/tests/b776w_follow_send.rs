//! follow-up (limits audit row D09): the send box follows the protocol constant, so twenty messages may wait for Cline and the
//! twenty-first stays in the box with the number in the words of the refusal.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use ham::tui::ext::{Done, Job};
use ham::tui::{App, Effect, Msg};
use ham_core::messages::MessagesPage;
use harness_protocol::messages::{MAX_OUTSTANDING_MESSAGES, SendMessageRequest};
use serde_json::{Value, json};

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

fn waiting(n: u32) -> Value {
    json!({"messageId": mid(n), "seq": n, "mode": "next", "state": "pending", "text": format!("queued {n}"), "epoch": "s-1", "epochNo": 1,
           "acceptedAt": "2026-10-16T08:15:10Z", "updatedAt": "2026-10-16T08:15:10Z"})
}

fn listed(count: u32) -> Msg {
    let page: MessagesPage = serde_json::from_value(
        json!({"runId": RUN, "messages": (1..=count).map(waiting).collect::<Vec<_>>(),
        "admission": "open", "epoch": "s-1", "epochNo": 1, "watermark": count}),
    )
    .unwrap();
    Msg::Ext(Done::Messages {
        run_id: RUN.into(),
        result: Ok(page),
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

#[test]
fn b776w_the_send_box_sends_the_twentieth_message_and_keeps_the_twenty_first() {
    assert_eq!(MAX_OUTSTANDING_MESSAGES, 20);
    let mut app = working();
    app.update(listed(MAX_OUTSTANDING_MESSAGES - 1));
    let mut clock = Clock::new();
    // nineteen wait: the twentieth goes out
    let effects = send(&mut app, &mut clock, "number twenty");
    assert_eq!(sends(&effects).len(), 1, "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some(""), "the box is emptied");
    // that send is on its way and counts: the twenty-first is not sent and its text stays in the box
    let effects = send(&mut app, &mut clock, "number twenty-one");
    assert!(sends(&effects).is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("number twenty-one"));
    let text = screen(&app, 160, 40);
    assert!(
        text.contains(
            "20 messages are waiting for Cline already; wait for one to be delivered or withdraw one (w)"
        ),
        "{text}"
    );
}

#[test]
fn b776w_twenty_listed_messages_stop_the_next_send() {
    let mut app = working();
    app.update(listed(MAX_OUTSTANDING_MESSAGES));
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "one too many");
    assert!(
        sends(&effects).is_empty() && effects.is_empty(),
        "{effects:?}"
    );
    assert_eq!(app.chat_text().as_deref(), Some("one too many"));
    assert!(screen(&app, 160, 40).contains("20 messages are waiting for Cline already"));
}

#[test]
fn b776w_the_box_title_counts_the_waiting_messages_once_the_rows_hide_some() {
    // three rows show the newest three: up to three waiting messages need no count
    let mut app = working();
    app.update(listed(3));
    let text = screen(&app, 160, 40);
    assert!(text.contains("Enter: Send next to run"), "{text}");
    assert!(
        !text.contains("waiting)") && !text.contains(" waiting"),
        "{text}"
    );
    // twenty waiting: the title says so
    let mut app = working();
    app.update(listed(MAX_OUTSTANDING_MESSAGES));
    let text = screen(&app, 160, 40);
    let title = text
        .lines()
        .find(|l| l.contains("Message to Cline"))
        .unwrap_or_else(|| panic!("no box title:\n{text}"));
    assert!(
        title.contains("Enter: Send next to run") && title.contains("20 waiting"),
        "{title}"
    );
}
