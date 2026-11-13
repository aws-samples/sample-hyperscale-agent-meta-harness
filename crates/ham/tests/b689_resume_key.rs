//! The key R of the terminal app. On the Runs screen it asks one question that names how many paused and stalled runs it will resume;
//! Enter or R again, 300 ms or more later, starts one follow-up run for each, and Esc withdraws the question. On the screen of a run it shows
//! the preview of `/resume --all` and the same key again runs it. The Runs list marks the two statuses and its note line says why the selected run
//! stopped and which session it ended in.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::ext::Job;
use ham::tui::{App, DirectSettings, Effect, Msg};
use serde_json::{Value, json};

const A: &str = "01K6A0000000000000000000A1";
const B: &str = "01K6A0000000000000000000B2";
const C: &str = "01K6A0000000000000000000C3";
const D: &str = "01K6A0000000000000000000D4";

fn run(id: &str, status: &str, extra: Value) -> Value {
    let mut v = json!({
        "runId": id, "status": status, "phase": "delivering", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-15T13:00:00Z", "ownerPrincipalArn": "arn:aws:iam::111122223333:role/test"
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn paused(id: &str) -> Value {
    run(
        id,
        "paused",
        json!({"statusReason": "total_time: the run used the total time it was given", "resumeMode": "follow_up", "session": 3}),
    )
}

fn stalled(id: &str) -> Value {
    run(
        id,
        "stalled",
        json!({"statusReason": "session_lost: the session stopped answering before it finished", "resumeMode": "follow_up", "session": 2}),
    )
}

fn runs_app(runs: Vec<Value>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let runs: Vec<harness_protocol::api::v1::RunSummary> =
        serde_json::from_value(Value::Array(runs)).unwrap();
    app.update(Msg::Runs(Ok(runs)));
    app
}

fn resumed(effects: &[Effect]) -> Vec<Vec<String>> {
    jobs(effects)
        .into_iter()
        .filter_map(|j| match j {
            Job::ResumeRuns { run_ids } => Some(run_ids.clone()),
            _ => None,
        })
        .collect()
}

fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn b689_r_on_the_runs_screen_asks_one_question_that_names_the_count() {
    let mut app = runs_app(vec![
        paused(A),
        stalled(B),
        run(C, "active", json!({})),
        run(D, "complete", json!({})),
    ]);
    let mut clock = Clock::new();
    let effects = press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    assert!(effects.is_empty(), "{effects:?}");
    let text = flat(&screen(&app, 140, 30));
    assert!(
        text.contains(
            "Resume 2 runs: each starts a new billable run from the code it delivered. Enter confirms · Esc cancels"
        ),
        "{text}"
    );
}

#[test]
fn b689_enter_300_ms_later_starts_one_follow_up_for_each_and_an_earlier_enter_does_not() {
    let mut app = runs_app(vec![paused(A), stalled(B), run(C, "active", json!({}))]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    // Too soon: nothing starts and the question stays.
    let early = enter(&mut app, &mut clock, 100);
    assert!(resumed(&early).is_empty(), "{early:?}");
    assert!(flat(&screen(&app, 140, 30)).contains("Resume 2 runs"));
    // After the delay: one job names both runs, in the order of the list.
    let late = enter(&mut app, &mut clock, 400);
    assert_eq!(resumed(&late), vec![vec![A.to_string(), B.to_string()]]);
    assert!(
        flat(&screen(&app, 140, 30)).contains("starting 2 follow-up runs"),
        "{}",
        screen(&app, 140, 30)
    );
}

#[test]
fn b689_r_again_confirms_like_enter() {
    let mut app = runs_app(vec![paused(A)]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    let late = press(&mut app, &mut clock, KeyCode::Char('R'), 400);
    assert_eq!(resumed(&late), vec![vec![A.to_string()]]);
}

#[test]
fn b689_esc_withdraws_the_question_and_starts_nothing() {
    let mut app = runs_app(vec![paused(A), stalled(B)]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(resumed(&effects).is_empty(), "{effects:?}");
    // The question is gone: Enter now opens the selected run instead of confirming anything.
    let after = enter(&mut app, &mut clock, 400);
    assert!(resumed(&after).is_empty(), "{after:?}");
}

#[test]
fn b689_with_nothing_to_resume_the_line_says_so_and_asks_nothing() {
    let mut app = runs_app(vec![
        run(C, "active", json!({})),
        run(D, "complete", json!({})),
    ]);
    let mut clock = Clock::new();
    let effects = press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    assert!(effects.is_empty());
    let text = flat(&screen(&app, 140, 30));
    assert!(
        text.contains("no paused or stalled run to resume"),
        "{text}"
    );
    let after = enter(&mut app, &mut clock, 400);
    assert!(resumed(&after).is_empty(), "{after:?}");
}

#[test]
fn b689_a_run_that_a_later_run_continues_is_left_out_and_counted() {
    let mut app = runs_app(vec![
        run(C, "active", json!({"continueFrom": A})),
        paused(A),
        stalled(B),
    ]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    let text = flat(&screen(&app, 140, 30));
    assert!(text.contains("Resume 1 run: each starts"), "{text}");
    assert!(text.contains(", 1 skipped"), "{text}");
    let late = enter(&mut app, &mut clock, 400);
    assert_eq!(resumed(&late), vec![vec![B.to_string()]]);
}

#[test]
fn b689_the_list_marks_paused_and_stalled_runs_and_the_note_says_why_and_in_which_session() {
    let app = runs_app(vec![paused(A), stalled(B), run(C, "active", json!({}))]);
    let text = screen(&app, 140, 30);
    let row = |id: &str| {
        text.lines()
            .find(|l| l.contains(id))
            .unwrap_or_else(|| panic!("no row for {id}:\n{text}"))
            .to_string()
    };
    // The gutter is the first cell inside the table's border.
    assert!(row(A).contains("│= "), "{}", row(A));
    assert!(row(B).contains("│~ "), "{}", row(B));
    assert!(
        !row(C).contains("│= ") && !row(C).contains("│~ "),
        "{}",
        row(C)
    );
    let flat_text = flat(&text);
    assert!(flat_text.contains("2 to resume (R)"), "{flat_text}");
    // The first run is the selected one.
    assert!(
        flat_text.contains("paused: the run used the total time it was given · session 3"),
        "{flat_text}"
    );
}

#[test]
fn b689_r_on_the_screen_of_a_run_previews_resume_all_and_the_same_key_again_runs_it() {
    let mut app = ready(base_events());
    let mut clock = Clock::new();
    // Browse mode, then R: the key line says what the next press does and nothing starts.
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let first = press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    assert!(first.is_empty(), "{first:?}");
    let text = flat(&screen(&app, 140, 40));
    assert!(text.contains("press R again to resume"), "{text}");
    // The same key, 300 ms or more later, runs the command `ham resume --all`.
    let second = press(&mut app, &mut clock, KeyCode::Char('R'), 400);
    let ran: Vec<Vec<String>> = second
        .iter()
        .filter_map(|e| match e {
            Effect::RunCommand { argv, .. } => Some(argv.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ran, vec![vec!["resume".to_string(), "--all".to_string()]]);
}

fn summaries(runs: Vec<Value>) -> Vec<harness_protocol::api::v1::RunSummary> {
    serde_json::from_value(Value::Array(runs)).unwrap()
}

#[test]
fn b689_any_other_key_withdraws_the_question_and_is_the_screens_own_key() {
    let mut app = runs_app(vec![paused(A), stalled(B)]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    assert!(flat(&screen(&app, 140, 30)).contains("Resume 2 runs"));
    let effects = press(&mut app, &mut clock, KeyCode::Down, 400);
    assert!(resumed(&effects).is_empty(), "{effects:?}");
    assert!(
        !flat(&screen(&app, 140, 30)).contains("Resume 2 runs"),
        "the question is withdrawn by the next key"
    );
    // Enter no longer confirms anything
    let after = enter(&mut app, &mut clock, 400);
    assert!(resumed(&after).is_empty(), "{after:?}");
}

#[test]
fn b689_the_answer_of_the_resume_job_is_one_line_for_every_run_and_the_list_is_read_again() {
    use ham::tui::ext::Done;
    use ham_core::resume::Outcome;
    // A is continued by C, so only B starts; A is counted as skipped.
    let mut app = runs_app(vec![
        paused(A),
        stalled(B),
        run(C, "active", json!({"continueFrom": A})),
    ]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    let late = enter(&mut app, &mut clock, 400);
    assert_eq!(resumed(&late), vec![vec![B.to_string()]]);
    let effects = app.update(Msg::Ext(Done::Resumed {
        outcomes: vec![
            Outcome {
                run_id: B.to_string(),
                result: Ok("01K6E0000000000000000000E5".to_string()),
            },
            Outcome {
                run_id: D.to_string(),
                result: Err("the service did not answer".to_string()),
            },
        ],
    }));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::LoadRuns)),
        "the list is read again: {effects:?}"
    );
    // the note shows the first eight characters of each id
    let text = flat(&screen(&app, 200, 30));
    assert!(text.contains("resumed 1 of 2:"), "{text}");
    assert!(text.contains("01K6A000 -> 01K6E000"), "{text}");
    assert!(
        text.contains("01K6A000 not started: the service did not answer"),
        "{text}"
    );
    assert!(text.contains("1 skipped"), "{text}");
}

#[test]
fn b689_r_on_the_screen_of_a_run_counts_the_runs_of_the_list_it_has_read() {
    let mut app = ready(base_events());
    app.update(Msg::Runs(Ok(summaries(vec![paused(A), stalled(B)]))));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let first = press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    assert!(first.is_empty(), "{first:?}");
    let text = flat(&screen(&app, 140, 40));
    assert!(
        text.contains("press R again to resume 2 paused or stalled runs"),
        "{text}"
    );
}

#[test]
fn b689_r_on_the_screen_of_a_run_says_so_when_the_list_holds_nothing_to_resume() {
    let mut app = ready(base_events());
    app.update(Msg::Runs(Ok(summaries(vec![
        run(C, "active", json!({})),
        run(D, "complete", json!({})),
    ]))));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let first = press(&mut app, &mut clock, KeyCode::Char('R'), 50);
    assert!(first.is_empty(), "{first:?}");
    assert!(
        flat(&screen(&app, 140, 40)).contains("no paused or stalled run to resume"),
        "{}",
        screen(&app, 140, 40)
    );
    // and no second press runs anything
    let second = press(&mut app, &mut clock, KeyCode::Char('R'), 400);
    assert!(
        second
            .iter()
            .all(|e| !matches!(e, Effect::RunCommand { .. })),
        "{second:?}"
    );
}
