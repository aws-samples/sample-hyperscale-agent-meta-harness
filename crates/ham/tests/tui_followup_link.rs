//! A follow-up run and the run it continues from.
//! Item 6: before the second Enter starts a follow-up, the client looks for one from the same source that is still going and offers "a
//! follow-up from this run is already running: open it"; a lookup that fails asks again before it starts blind. Item 8: the chat of a
//! follow-up run shows the conversation of the run it continues from above a divider that says it is read-only and was not given to the new
//! session, and the view is the same after reopening.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::ext::{Done, Job};
use ham::tui::{App, Effect, Msg};
use ham_core::stored::Read;
use harness_protocol::goal::GoalStatus;
use serde_json::json;

const CHILD: &str = "01M5A2R7K3C9XJ4D8E6FCHILD1";
const PARENT: &str = "01M5A2R7K3C9XJ4D8E6FPARENT";
const DIVIDER: &str = "previous run, read-only. The new session received only the prompt you submitted, not this history.";
const OFFER: &str = "A follow-up from this run is already running: open it";

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

fn finished() -> App {
    ready(finished_events())
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

fn lookups(effects: &[Effect]) -> Vec<String> {
    jobs(effects)
        .into_iter()
        .filter_map(|j| match j {
            Job::FindFollowUp { run_id } => Some(run_id.clone()),
            _ => None,
        })
        .collect()
}

fn found(result: Result<Option<(String, GoalStatus)>, String>) -> Msg {
    Msg::Ext(Done::FollowUpFound {
        run_id: RUN.into(),
        result,
    })
}

/// A finished run with its follow-up prepared and the second Enter pressed: the lookup is out.
fn looking() -> (App, Clock, String) {
    let mut app = finished();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    let draft = app.chat_text().unwrap();
    let second = enter(&mut app, &mut clock, 400);
    assert_eq!(lookups(&second), vec![RUN.to_string()], "{second:?}");
    (app, clock, draft)
}

// ---- item 6: a follow-up that is already running -----------------------------------------------------------------------------

#[test]
fn the_second_enter_looks_for_a_running_follow_up_and_starts_nothing_yet() {
    let (app, _clock, _draft) = looking();
    let _ = app;
}

#[test]
fn nothing_found_starts_the_run_with_exactly_the_text_in_the_box() {
    let (mut app, _clock, draft) = looking();
    let effects = app.update(found(Ok(None)));
    assert_eq!(
        starts(&effects),
        vec![(RUN.to_string(), draft)],
        "{effects:?}"
    );
}

#[test]
fn a_running_follow_up_is_offered_and_enter_opens_it() {
    let (mut app, mut clock, draft) = looking();
    let effects = app.update(found(Ok(Some((CHILD.to_string(), GoalStatus::Active)))));
    assert!(starts(&effects).is_empty(), "{effects:?}");
    let text = flat(&screen(&app, 140, 40));
    assert!(text.contains(OFFER), "{text}");
    assert!(text.contains(CHILD), "the offer names the run: {text}");
    // Too soon, nothing happens; after the pause Enter opens the run that is going.
    assert!(enter(&mut app, &mut clock, 100).is_empty());
    let open = enter(&mut app, &mut clock, 400);
    assert!(starts(&open).is_empty(), "{open:?}");
    assert!(
        jobs(&open)
            .iter()
            .any(|j| matches!(j, Job::Transcript { run_id, .. } if run_id == CHILD)),
        "the running follow-up is opened: {open:?}"
    );
    let _ = draft;
}

#[test]
fn declining_the_offer_keeps_the_text_and_starts_after_the_confirmation_is_shown_again() {
    let (mut app, mut clock, draft) = looking();
    app.update(found(Ok(Some((CHILD.to_string(), GoalStatus::Active)))));
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert_eq!(app.chat_text().as_deref(), Some(draft.as_str()));
    // The next Enter shows the start confirmation again and starts nothing.
    let again = enter(&mut app, &mut clock, 400);
    assert!(
        starts(&again).is_empty() && lookups(&again).is_empty(),
        "{again:?}"
    );
    assert!(
        flat(&screen(&app, 140, 40)).contains("new billable run"),
        "{}",
        screen(&app, 140, 40)
    );
    // The one after starts it: the person has seen the offer, so there is no second lookup.
    let start = enter(&mut app, &mut clock, 400);
    assert!(lookups(&start).is_empty(), "{start:?}");
    assert_eq!(starts(&start), vec![(RUN.to_string(), draft)]);
}

#[test]
fn a_lookup_that_failed_says_so_and_the_next_enter_starts() {
    let (mut app, mut clock, draft) = looking();
    let effects = app.update(found(Err("HTTP 503: unavailable".into())));
    assert!(starts(&effects).is_empty(), "{effects:?}");
    let text = flat(&screen(&app, 140, 40));
    assert!(
        text.contains("could not look for a follow-up that is already running"),
        "{text}"
    );
    assert!(text.contains("Enter starts it anyway"), "{text}");
    let start = enter(&mut app, &mut clock, 400);
    assert_eq!(starts(&start), vec![(RUN.to_string(), draft)], "{start:?}");
}

#[test]
fn a_second_press_while_the_lookup_is_out_asks_for_nothing_more() {
    let (mut app, mut clock, _draft) = looking();
    let more = enter(&mut app, &mut clock, 400);
    assert!(
        lookups(&more).is_empty() && starts(&more).is_empty(),
        "{more:?}"
    );
}

// ---- item 8: the conversation of the run this one continues from -------------------------------------------------------------

fn follow_up_run() -> (App, Vec<Effect>) {
    // A finished run: its own conversation is read to its end and not polled, so the ticks of a test ask only for what is under test.
    let (mut app, opened) = ready_with(finished_events());
    let detail: harness_protocol::api::v1::RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b",
                    "continueFrom": PARENT},
        "objective": "now add the docs", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(detail)));
    (app, opened)
}

fn parent_page(read: Read, state: &str, text: &str) -> Msg {
    Msg::Ext(Done::Transcript {
        run_id: PARENT.into(),
        read,
        result: Ok(page(state, vec![msg(0, 0, "p1", text)], None, None)),
    })
}

fn transcript_for(effects: &[Effect], run: &str) -> Option<Read> {
    jobs(effects).into_iter().find_map(|j| match j {
        Job::Transcript { run_id, read, .. } if run_id == run => Some(*read),
        _ => None,
    })
}

fn tick_until_parent_asked(app: &mut App) -> Option<Read> {
    for _ in 0..10 {
        if let Some(r) = transcript_for(&app.tick(), PARENT) {
            return Some(r);
        }
    }
    None
}

#[test]
fn a_follow_up_run_asks_for_the_conversation_of_the_run_it_continues_from() {
    let (mut app, _opened) = follow_up_run();
    app.update(answer(
        Read::First,
        page(
            "complete",
            vec![msg(0, 0, "o1", "Own first message.")],
            None,
            None,
        ),
    ));
    assert_eq!(tick_until_parent_asked(&mut app), Some(Read::First));
}

#[test]
fn an_ordinary_run_never_asks_for_another_run_and_shows_no_divider() {
    let mut app = ready(base_events());
    app.update(answer(
        Read::First,
        page(
            "complete",
            vec![msg(0, 0, "o1", "Own first message.")],
            None,
            None,
        ),
    ));
    for _ in 0..10 {
        let effects = app.tick();
        assert!(
            jobs(&effects)
                .iter()
                .all(|j| !matches!(j, Job::Transcript { run_id, .. } if run_id != RUN)),
            "{effects:?}"
        );
    }
    assert!(!flat(&screen(&app, 140, 40)).contains("previous run"));
}

#[test]
fn the_previous_conversation_stands_above_the_divider_above_the_runs_own() {
    let (mut app, _o) = follow_up_run();
    app.update(answer(
        Read::First,
        page(
            "complete",
            vec![msg(0, 0, "o1", "Own first message.")],
            None,
            None,
        ),
    ));
    let _ = tick_until_parent_asked(&mut app);
    app.update(parent_page(Read::First, "complete", "Parent says hello."));
    let text = screen(&app, 140, 40);
    let flat_text = flat(&text);
    let (p, d, o) = (
        flat_text.find("Parent says hello.").expect(&text),
        flat_text.find(DIVIDER).expect(&text),
        flat_text.find("Own first message.").expect(&text),
    );
    assert!(p < d && d < o, "{text}");
}

#[test]
fn a_previous_run_without_a_stored_conversation_still_gets_the_divider() {
    let (mut app, _o) = follow_up_run();
    app.update(answer(
        Read::First,
        page(
            "complete",
            vec![msg(0, 0, "o1", "Own first message.")],
            None,
            None,
        ),
    ));
    let _ = tick_until_parent_asked(&mut app);
    app.update(Msg::Ext(Done::Transcript {
        run_id: PARENT.into(),
        read: Read::First,
        result: Ok(page("unsupported", vec![], None, None)),
    }));
    let text = flat(&screen(&app, 140, 40));
    assert!(text.contains(DIVIDER), "{text}");
    assert!(text.contains("Own first message."), "{text}");
}

#[test]
fn a_finished_previous_run_is_not_polled_and_the_view_is_the_same_after_reopening() {
    let build = || {
        let (mut app, _o) = follow_up_run();
        app.update(answer(
            Read::First,
            page(
                "complete",
                vec![msg(0, 0, "o1", "Own first message.")],
                None,
                None,
            ),
        ));
        let _ = tick_until_parent_asked(&mut app);
        app.update(parent_page(Read::First, "complete", "Parent says hello."));
        for _ in 0..40 {
            let effects = app.tick();
            assert!(
                transcript_for(&effects, PARENT).is_none(),
                "the previous run is read once: {effects:?}"
            );
        }
        screen(&app, 140, 40)
    };
    assert_eq!(build(), build());
}

#[test]
fn a_failed_read_of_the_previous_run_is_tried_again_and_given_up_after_three() {
    let (mut app, _o) = follow_up_run();
    app.update(answer(
        Read::First,
        page(
            "complete",
            vec![msg(0, 0, "o1", "Own first message.")],
            None,
            None,
        ),
    ));
    let fail = |app: &mut App| {
        app.update(Msg::Ext(Done::Transcript {
            run_id: PARENT.into(),
            read: Read::First,
            result: Err("HTTP 503: unavailable".into()),
        }));
    };
    assert!(tick_until_parent_asked(&mut app).is_some());
    fail(&mut app);
    for round in 1..3 {
        let asked = (0..200).any(|_| transcript_for(&app.tick(), PARENT).is_some());
        assert!(asked, "tried again after failure {round}");
        fail(&mut app);
    }
    for _ in 0..400 {
        assert!(transcript_for(&app.tick(), PARENT).is_none(), "given up");
    }
    // The divider still says what the new session was given.
    assert!(flat(&screen(&app, 140, 40)).contains(DIVIDER));
}

#[test]
fn scrolling_to_the_top_asks_for_older_chunks_of_the_previous_run() {
    let (mut app, _o) = follow_up_run();
    app.update(answer(
        Read::First,
        page(
            "complete",
            vec![msg(0, 0, "o1", "Own first message.")],
            None,
            None,
        ),
    ));
    let _ = tick_until_parent_asked(&mut app);
    let records: Vec<_> = (0..60)
        .map(|n| {
            msg(
                10 + n / 6,
                n,
                &format!("m{n}"),
                &format!("parent message number {n}"),
            )
        })
        .collect();
    app.update(Msg::Ext(Done::Transcript {
        run_id: PARENT.into(),
        read: Read::First,
        result: Ok(page("complete", records, Some(10), None)),
    }));
    let _ = screen(&app, 100, 40);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let effects = press(&mut app, &mut clock, KeyCode::Home, 50);
    assert_eq!(
        transcript_for(&effects, PARENT),
        Some(Read::Older),
        "{effects:?}"
    );
}
