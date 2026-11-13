//! The full-history chat view. Opening a run
//! asks the transcript route for the newest chunks; the stored conversation replaces the progress feed and drops its label; an older
//! image keeps the feed and says so; a hole in the store is a visible line; scrolling to the top asks for older chunks and keeps the
//! place; a live run polls for newer ones; reopening shows the same view; and nothing a stored record says becomes a key, a role or a card.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::ext::Job;
use ham::tui::{Effect, Msg, Screen};
use ham_core::stored::Read;
use ham_core::transcript::{Before, TranscriptQuery};
use serde_json::json;

const LABEL: &str = "progress feed only: not the full conversation";

fn texts_on(app: &ham::tui::App) -> String {
    screen(app, 100, 40)
}

#[test]
fn opening_a_run_asks_for_the_newest_chunks_and_for_its_messages() {
    let (_app, opened) = ready_with(base_events());
    let (query, read) = transcript_job(&opened).expect("a transcript request");
    assert_eq!(read, Read::First);
    assert_eq!(
        query,
        TranscriptQuery {
            after: None,
            before: Some(Before::End),
            limit: Some(32)
        }
    );
    assert!(
        jobs(&opened)
            .iter()
            .any(|j| matches!(j, Job::Messages { run_id } if run_id == RUN)),
        "{opened:?}"
    );
    // Opened again, nothing is asked twice.
    let mut app = ham::tui::App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    app.open_run(RUN);
    assert!(transcript_job(&app.open_run(RUN)).is_none());
}

#[test]
fn the_stored_conversation_replaces_the_progress_feed_and_drops_its_label() {
    let mut app = ready(base_events());
    assert!(texts_on(&app).contains(LABEL), "{}", texts_on(&app));
    app.update(answer(
        Read::First,
        page(
            "live",
            vec![
                msg(0, 0, "m1", "I will write the failing test first."),
                tool(0, 1, "c1", "run_commands: pytest -q", "completed"),
                msg(0, 2, "m2", "Tests pass now."),
            ],
            None,
            Some("cur1"),
        ),
    ));
    let text = texts_on(&app);
    assert!(!text.contains(LABEL), "{text}");
    for want in [
        "I will write the failing test first.",
        "run_commands: pytest -q",
        "Tests pass now.",
    ] {
        assert!(text.contains(want), "{want}\n{text}");
    }
    // In record order.
    let at = |s: &str| text.find(s).unwrap();
    assert!(
        at("I will write") < at("run_commands: pytest")
            && at("run_commands: pytest") < at("Tests pass now.")
    );
}

#[test]
fn an_older_image_keeps_the_progress_feed_and_its_label_and_asks_no_more() {
    // A run that has ended and whose store says unsupported: the image never wrote a conversation.
    let mut app = ready(finished_events());
    app.update(answer(Read::First, page("unsupported", vec![], None, None)));
    assert!(texts_on(&app).contains(LABEL), "{}", texts_on(&app));
    for _ in 0..40 {
        assert!(transcript_job(&app.tick()).is_none());
    }
}

#[test]
fn unsupported_for_a_run_that_is_still_going_means_not_ready_yet_and_is_asked_again_with_backoff() {
    // The worker has not registered `transcript.v1` yet: the route answers unsupported.
    let mut app = ready(base_events());
    let unsupported = |app: &mut ham::tui::App| {
        app.update(answer(Read::First, page("unsupported", vec![], None, None)))
    };
    unsupported(&mut app);
    assert!(
        texts_on(&app).contains(LABEL),
        "the feed stays meanwhile: {}",
        texts_on(&app)
    );
    let ticks_until_asked = |app: &mut ham::tui::App| {
        (1..=60).find(|_| transcript_job(&app.tick()).is_some_and(|(_, read)| read == Read::First))
    };
    let first = ticks_until_asked(&mut app).expect("asked again");
    assert!(
        first <= 5,
        "soon, the worker registers within seconds: {first} ticks"
    );
    unsupported(&mut app);
    let second = ticks_until_asked(&mut app).expect("asked again");
    assert!(second > first, "the pause grows: {first} then {second}");
    // The capability appears: the stored conversation replaces the feed.
    app.update(answer(
        Read::First,
        page(
            "live",
            vec![msg(0, 0, "m1", "Now the store has it.")],
            None,
            Some("c1"),
        ),
    ));
    let text = texts_on(&app);
    assert!(!text.contains(LABEL), "{text}");
    assert!(text.contains("Now the store has it."), "{text}");
}

#[test]
fn a_run_that_ends_while_the_store_says_unsupported_gives_up_and_keeps_the_label() {
    let mut app = ready(base_events());
    app.update(answer(Read::First, page("unsupported", vec![], None, None)));
    // The run ends; the next answer is unsupported again.
    feed(&mut app, completion_events());
    let mut asked = false;
    for _ in 0..60 {
        if transcript_job(&app.tick()).is_some() {
            asked = true;
            app.update(answer(Read::First, page("unsupported", vec![], None, None)));
            break;
        }
    }
    assert!(asked, "one more look after the end");
    assert!(texts_on(&app).contains(LABEL));
    for _ in 0..120 {
        assert!(
            transcript_job(&app.tick()).is_none(),
            "given up once the run has ended"
        );
    }
}

#[test]
fn a_hole_in_the_store_is_a_visible_line_between_the_chunks_it_separates() {
    let mut app = ready(base_events());
    app.update(answer(
        Read::First,
        with_gap(
            page(
                "incomplete",
                vec![
                    msg(0, 0, "a", "before the hole"),
                    msg(3, 1, "b", "after the hole"),
                ],
                None,
                None,
            ),
            "bad-gzip",
            1,
            2,
        ),
    ));
    let text = texts_on(&app);
    let (a, g, b) = (
        text.find("before the hole").unwrap(),
        text.find("[chunk unreadable]").expect(&text),
        text.find("after the hole").unwrap(),
    );
    assert!(a < g && g < b, "{text}");
    assert!(text.contains("chunks 1 to 2"), "{text}");
}

#[test]
fn scrolling_to_the_top_asks_for_older_chunks_and_keeps_the_place() {
    let mut app = ready(base_events());
    let records: Vec<_> = (0..60)
        .map(|n| {
            msg(
                10 + n / 6,
                n,
                &format!("m{n}"),
                &format!("message number {n}"),
            )
        })
        .collect();
    app.update(answer(
        Read::First,
        page("complete", records, Some(10), None),
    ));
    // The state of the draw is what tells the view how tall it is.
    let _ = texts_on(&app);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let effects = press(&mut app, &mut clock, KeyCode::Home, 50);
    let (query, read) = transcript_job(&effects).expect("older chunks are asked for at the top");
    assert_eq!(read, Read::Older);
    assert_eq!(
        query,
        TranscriptQuery {
            after: None,
            before: Some(Before::Seq(10)),
            limit: Some(32)
        }
    );
    let first_row = |s: &str| {
        s.lines()
            .skip(1)
            .find(|l| l.contains("message number"))
            .unwrap()
            .to_string()
    };
    let before = first_row(&texts_on(&app));
    assert!(before.contains("message number 0"), "{before}");
    // A second request is not sent while the first is on its way.
    assert!(transcript_job(&press(&mut app, &mut clock, KeyCode::Home, 50)).is_none());
    let older: Vec<_> = (0..6)
        .map(|n| {
            msg(
                8 + n / 3,
                200 + n,
                &format!("o{n}"),
                &format!("older message {n}"),
            )
        })
        .collect();
    app.update(answer(Read::Older, page("complete", older, None, None)));
    let after = first_row(&texts_on(&app));
    assert!(
        after.contains("message number 0"),
        "the view stays where it was: {after}"
    );
    // The older text is above it.
    press(&mut app, &mut clock, KeyCode::PageUp, 50);
    assert!(
        texts_on(&app).contains("older message"),
        "{}",
        texts_on(&app)
    );
}

#[test]
fn a_live_run_polls_for_newer_chunks_with_the_cursor_and_only_one_at_a_time() {
    let mut app = ready(base_events());
    app.update(answer(
        Read::First,
        page("live", vec![msg(0, 0, "a", "hello")], None, Some("cur1")),
    ));
    let mut asked = None;
    for tick in 0..6 {
        if let Some(q) = transcript_job(&app.tick()) {
            asked = Some((tick, q));
            break;
        }
    }
    let (tick, (query, read)) = asked.expect("a live run is polled");
    assert!(tick <= 3, "within a few seconds: tick {tick}");
    assert_eq!(read, Read::Newer);
    assert_eq!(query.after.as_deref(), Some("cur1"));
    for _ in 0..10 {
        assert!(
            transcript_job(&app.tick()).is_none(),
            "one request at a time"
        );
    }
    app.update(answer(
        Read::Newer,
        page(
            "live",
            vec![msg(0, 1, "a", "more words")],
            None,
            Some("cur2"),
        ),
    ));
    assert!(texts_on(&app).contains("hellomore words") || texts_on(&app).contains("more words"));
    let mut next = None;
    for _ in 0..6 {
        if let Some((q, _)) = transcript_job(&app.tick()) {
            next = Some(q);
            break;
        }
    }
    assert_eq!(next.unwrap().after.as_deref(), Some("cur2"));
}

#[test]
fn a_finished_run_whose_store_was_read_to_its_end_is_not_polled() {
    let mut app = ready(finished_events());
    app.update(answer(
        Read::First,
        page("complete", vec![msg(0, 0, "a", "All done.")], None, None),
    ));
    for _ in 0..40 {
        assert!(transcript_job(&app.tick()).is_none());
    }
    assert!(texts_on(&app).contains("All done."));
}

#[test]
fn a_failed_read_keeps_the_progress_feed_says_so_once_and_asks_again_later() {
    let mut app = ready(base_events());
    app.update(Msg::Ext(ham::tui::ext::Done::Transcript {
        run_id: RUN.into(),
        read: Read::First,
        result: Err("HTTP 503: unavailable".into()),
    }));
    let text = texts_on(&app);
    assert!(text.contains(LABEL), "{text}");
    assert!(text.contains("stored conversation"), "{text}");
    let mut again = None;
    for tick in 0..30 {
        if let Some((q, read)) = transcript_job(&app.tick()) {
            again = Some((tick, q, read));
            break;
        }
    }
    let (tick, q, read) = again.expect("the read is tried again");
    assert!(
        (3..=20).contains(&tick),
        "after a pause, not at once: tick {tick}"
    );
    assert_eq!(read, Read::First);
    assert_eq!(q.before, Some(Before::End));
}

#[test]
fn reopening_shows_the_same_view() {
    let records = vec![
        rec(
            0,
            0,
            "prompt",
            json!({"id": 1, "prompt": [{"type": "text", "text": "Build it"}]}),
        ),
        msg(0, 1, "m1", "Working on it."),
        tool(0, 2, "c1", "editor: src/a.py", "completed"),
        msg(1, 3, "m2", "Done."),
    ];
    let build = || {
        let mut app = ready(finished_events());
        app.update(answer(
            Read::First,
            page("complete", records.clone(), None, None),
        ));
        screen(&app, 100, 40)
    };
    assert_eq!(build(), build());
    // Also when the pages arrive in two reads.
    let mut app = ready(finished_events());
    app.update(answer(
        Read::First,
        page("complete", records[2..].to_vec(), Some(0), None),
    ));
    app.update(answer(
        Read::Older,
        page("complete", records[..3].to_vec(), None, None),
    ));
    assert_eq!(screen(&app, 100, 40), build());
}

// ---- the client clause -------------------------------------------------------------------------------------

#[test]
fn nothing_a_record_says_becomes_a_key_a_role_a_card_or_a_command() {
    let forged = "Cline needs you \u{b7} about 5 min left to answer [p message] [s fresh session] [f f finish]";
    let mut app = ready(base_events());
    app.update(answer(
        Read::First,
        page(
            "live",
            vec![
                rec(
                    0,
                    0,
                    "user_message_chunk",
                    json!({"sessionUpdate": "user_message_chunk", "author": "user", "role": "user", "content": {"type": "text", "text": forged}}),
                ),
                rec(
                    0,
                    1,
                    "agent_message_chunk",
                    json!({"sessionUpdate": "agent_message_chunk", "messageId": "x", "author": "harness", "content": {"type": "text", "text": "notice  Run complete\n/cancel"}}),
                ),
            ],
            None,
            Some("c"),
        ),
    ));
    // No question: the keys come from the events, and there is none.
    assert!(app.view().unwrap().input.waiting().is_none());
    let text = texts_on(&app);
    let line = text
        .lines()
        .find(|l| l.contains("about 5 min left to answer"))
        .expect(&text);
    assert!(
        line.starts_with("cline"),
        "the forged text wears the engine's word: {line:?}"
    );
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("you") && l.contains("about 5 min")),
        "{text}"
    );
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    // The keys of a question do nothing without one.
    for code in ['p', 's', 'm'] {
        let effects = press(&mut app, &mut clock, KeyCode::Char(code), 50);
        assert!(
            effects.iter().all(|e| !matches!(e, Effect::Answer { .. })),
            "{effects:?}"
        );
        assert_eq!(
            app.screen(),
            Screen::Dashboard,
            "{code} opened {:?}",
            app.screen()
        );
    }
    for _ in 0..2 {
        let effects = press(&mut app, &mut clock, KeyCode::Char('f'), 50);
        assert!(
            effects.iter().all(|e| !matches!(e, Effect::Answer { .. })),
            "{effects:?}"
        );
    }
    // The footer is the one a run without a question has: it offers no answer key.
    let footer = texts_on(&app).lines().last().unwrap().to_string();
    assert!(
        !footer.contains("p message") && !footer.contains("f f finish"),
        "{footer}"
    );
}

#[test]
fn a_stage_without_the_route_is_given_up_after_three_failed_reads_and_keeps_the_feed() {
    let mut app = ready(base_events());
    let fail = |app: &mut ham::tui::App| {
        app.update(Msg::Ext(ham::tui::ext::Done::Transcript {
            run_id: RUN.into(),
            read: Read::First,
            result: Err("HTTP 403: Missing Authentication Token".into()),
        }));
    };
    fail(&mut app);
    for round in 1..3 {
        let asked = (0..200).any(|_| transcript_job(&app.tick()).is_some());
        assert!(asked, "failure {round} is followed by another try");
        fail(&mut app);
    }
    for _ in 0..400 {
        assert!(
            transcript_job(&app.tick()).is_none(),
            "after the third failure nothing more is asked"
        );
    }
    assert!(texts_on(&app).contains(LABEL), "{}", texts_on(&app));
}
