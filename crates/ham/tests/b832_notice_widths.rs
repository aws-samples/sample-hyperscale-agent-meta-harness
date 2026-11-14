//! A notice of a resume, a retry or a follow-up never cuts a sentence the user needs at 80 or 120 columns. The Runs screen keeps a
//! short notice on its note row and shows the whole text in a wrapped panel above it; a notice with no such text that does not fit one row
//! wraps onto a few rows. The run screen keeps a short notice on its one notice row and the whole reason of a start that did not start as a
//! card of the run it was started from, wrapped to the width, the way it keeps the line of provider data sharing. The result panel of
//! `/resume --all` wraps its lines. Every test draws the screen at 80 and at 120 columns.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::output::{Chunk, CmdOutput, Stream};
use ham::tui::ext::Done;
use ham::tui::{App, DirectSettings, Msg, Screen};
use ham_core::resume::Outcome;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];
/// The run a retry, a resume or a follow-up starts from (`chatfx::RUN`, the run on screen) and the run it starts.
const NEW: &str = "01M4ZKQ2AB3CD4EF5GH6JK7MNP";
const A: &str = "01K6A0000000000000000000A1";
const B: &str = "01K6B0000000000000000000B2";
const C: &str = "01K6C0000000000000000000C3";
const D: &str = "01K6D0000000000000000000D4";

/// The screen as one line of words: the rows joined, the side border of a box taken out.
fn flat(text: &str) -> String {
    text.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one message that stops a resume or a retry while `run.provider_sharing` is off: the part that says how to turn it on comes last.
fn stop() -> String {
    ham_core::sharing::stored_stop("Claude Fable 5.2", "111122223333", "us-east-1", "inherit")
}

/// The line after ham switched the account's mode.
fn switched() -> String {
    ham_core::sharing::switched_line(
        "Claude Fable 5.2",
        "111122223333",
        "us-east-1",
        "inherit",
        "provider_data_share",
        ham_core::sharing::Agreement::Setting,
    )
}

fn runs_screen() -> App {
    let app = App::new_direct("n".into(), None, DirectSettings::default());
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn harness_event(seq: u64, event_type: EventType, value: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq,
        ts: T0.into(),
        event_type,
        authority: Authority::Harness,
        payload: Payload::Inline { value },
    }
}

/// The events of `RUN` blocked: what `mark-blocked` writes after the usual events.
fn blocked_events() -> Vec<Envelope> {
    let mut events = base_events();
    events.extend([
        harness_event(
            20,
            EventType::GoalBlocked,
            json!({"reason": "the run machine failed"}),
        ),
        harness_event(
            21,
            EventType::GoalStatus,
            json!({"status": "blocked", "phase": "delivering"}),
        ),
        harness_event(
            22,
            EventType::RunCompleted,
            json!({"status": "blocked", "outcome": "delivery_failed", "label": "Not delivered"}),
        ),
    ]);
    events
}

/// The run `RUN` on screen, blocked.
fn blocked() -> App {
    ready(blocked_events())
}

fn sized(app: &App, w: u16, h: u16) -> String {
    flat(&screen(app, w, h))
}

// ---- the Runs screen --------------------------------------------------------------------------------------------------------------

#[test]
fn b832_a_resume_the_setting_stopped_shows_how_to_turn_it_on() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::Resumed {
            outcomes: vec![Outcome {
                run_id: RUN.into(),
                result: Err(stop()),
            }],
        }));
        let text = sized(&app, w, h);
        assert!(text.contains("resumed 0 of 1"), "{w}x{h}: {text}");
        assert!(text.contains(&flat(&stop())), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_a_resume_that_switched_shows_what_each_run_did_and_the_whole_line() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::ResumedNoted {
            outcomes: vec![Outcome {
                run_id: RUN.into(),
                result: Ok(NEW.into()),
            }],
            notes: vec![(NEW.into(), switched())],
        }));
        let text = sized(&app, w, h);
        let line = text
            .find(&flat(&switched()))
            .unwrap_or_else(|| panic!("{w}x{h}: {text}"));
        let summary = text
            .find("resumed 1 of 1: 01K6A000 -> 01M4ZKQ2")
            .unwrap_or_else(|| panic!("{w}x{h}: {text}"));
        // The account may have changed, so its line comes first.
        assert!(line < summary, "{w}x{h}: {text}");
    }
}

#[test]
fn b832_runs_one_reason_stopped_show_the_reason_once() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::Resumed {
            outcomes: [A, B, C]
                .iter()
                .map(|id| Outcome {
                    run_id: (*id).into(),
                    result: Err(stop()),
                })
                .collect(),
        }));
        let text = sized(&app, w, h);
        assert!(text.contains("resumed 0 of 3"), "{w}x{h}: {text}");
        assert_eq!(text.matches(&flat(&stop())).count(), 1, "{w}x{h}: {text}");
        assert!(
            text.contains("01K6A000, 01K6B000, 01K6C000 not started:"),
            "{w}x{h}: {text}"
        );
    }
}

#[test]
fn b832_a_resume_of_runs_that_started_and_one_that_did_not_names_each() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::ResumedNoted {
            outcomes: vec![
                Outcome {
                    run_id: A.into(),
                    result: Ok(NEW.into()),
                },
                Outcome {
                    run_id: B.into(),
                    result: Err("the service did not answer".into()),
                },
            ],
            notes: vec![(NEW.into(), switched())],
        }));
        let text = sized(&app, w, h);
        assert!(text.contains(&flat(&switched())), "{w}x{h}: {text}");
        assert!(
            text.contains("01K6B000 not started: the service did not answer"),
            "{w}x{h}: {text}"
        );
        assert!(
            text.contains("resumed 1 of 2: 01K6A000 -> 01M4ZKQ2"),
            "{w}x{h}: {text}"
        );
    }
}

fn run_row(id: &str, status: &str, extra: Value) -> Value {
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
    run_row(
        id,
        "paused",
        json!({"statusReason": "total_time: the run used the total time it was given", "resumeMode": "follow_up", "session": 3}),
    )
}

#[test]
fn b832_the_resume_question_keeps_its_keys() {
    for (w, h) in SIZES {
        let mut app = App::new_direct("n".into(), None, DirectSettings::default());
        let runs: Vec<harness_protocol::api::v1::RunSummary> =
            serde_json::from_value(Value::Array(vec![
                paused(A),
                paused(B),
                paused(D),
                run_row(C, "active", json!({"continueFrom": A})),
            ]))
            .unwrap();
        app.update(Msg::Runs(Ok(runs)));
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Char('R'), 50);
        let text = sized(&app, w, h);
        assert!(
            text.contains("Resume 2 runs: each starts a new billable run from the code it delivered, 1 skipped. Enter confirms · Esc cancels"),
            "{w}x{h}: {text}"
        );
    }
}

#[test]
fn b832_a_retry_answer_on_the_runs_screen_is_whole() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::RetryNoted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
            note: switched(),
        }));
        let text = sized(&app, w, h);
        assert!(
            text.contains(&format!("started run {NEW}, a retry of run {RUN}")),
            "{w}x{h}: {text}"
        );
        assert!(text.contains(&flat(&switched())), "{w}x{h}: {text}");

        // A retry that did not start says so on the Runs screen too, with the whole reason.
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::RetryStarted {
            run_id: RUN.into(),
            result: Err(stop()),
        }));
        let text = sized(&app, w, h);
        assert!(
            text.contains(&format!("the retry of run {RUN} did not start")),
            "{w}x{h}: {text}"
        );
        assert!(text.contains(&flat(&stop())), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_the_next_key_takes_the_notice_and_its_panel_away() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::Resumed {
            outcomes: vec![Outcome {
                run_id: RUN.into(),
                result: Err(stop()),
            }],
        }));
        assert!(sized(&app, w, h).contains(&flat(&stop())));
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Down, 50);
        let text = sized(&app, w, h);
        assert!(!text.contains("resumed 0 of 1"), "{w}x{h}: {text}");
        assert!(
            !text.contains("ham switches the mode only when you agree"),
            "{w}x{h}: {text}"
        );
        assert!(text.contains("no runs yet"), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_a_run_a_resume_did_not_start_keeps_the_reason_as_a_card_until_one_starts() {
    for (w, h) in SIZES {
        let mut app = runs_screen();
        app.update(Msg::Ext(Done::Resumed {
            outcomes: vec![Outcome {
                run_id: RUN.into(),
                result: Err(stop()),
            }],
        }));
        app.open_run(RUN);
        feed(&mut app, base_events());
        let text = sized(&app, w, h);
        assert!(text.contains("resume did not start"), "{w}x{h}: {text}");
        assert!(text.contains(&flat(&stop())), "{w}x{h}: {text}");

        // A later resume of the same run that starts takes the card away.
        app.update(Msg::Ext(Done::Resumed {
            outcomes: vec![Outcome {
                run_id: RUN.into(),
                result: Ok(NEW.into()),
            }],
        }));
        app.open_run(RUN);
        feed(&mut app, base_events());
        let text = sized(&app, w, h);
        assert!(!text.contains("resume did not start"), "{w}x{h}: {text}");
    }
}

// ---- the run screen ---------------------------------------------------------------------------------------------------------------

#[test]
fn b832_a_retry_that_did_not_start_says_why_and_keeps_the_text() {
    let after_switch = format!("{} the service did not answer", switched());
    for why in [stop(), after_switch] {
        for (w, h) in SIZES {
            let mut app = blocked();
            app.update(Msg::Ext(Done::RetryStarted {
                run_id: RUN.into(),
                result: Err(why.clone()),
            }));
            assert_eq!(app.focused_run(), Some(RUN));
            let text = sized(&app, w, h);
            assert!(text.contains("retry did not start"), "{w}x{h}: {text}");
            assert!(text.contains(&flat(&why)), "{w}x{h}: {text}");
            assert!(text.contains("your text is kept"), "{w}x{h}: {text}");
        }
    }
}

#[test]
fn b832_a_retry_that_switched_keeps_what_started_on_the_row_and_the_whole_line() {
    for (w, h) in SIZES {
        let mut app = blocked();
        app.update(Msg::Ext(Done::RetryNoted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
            note: switched(),
        }));
        assert_eq!(app.focused_run(), Some(NEW));
        let text = sized(&app, w, h);
        assert!(
            text.contains(&format!("the retry of run {RUN} started")),
            "{w}x{h}: {text}"
        );
        assert!(text.contains(&flat(&switched())), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_a_follow_up_that_did_not_start_shows_the_switch_and_why() {
    for (w, h) in SIZES {
        let mut app = ready(finished_events());
        app.update(Msg::Ext(Done::FollowUpNoted {
            run_id: RUN.into(),
            result: Err("the stage takes no run now".into()),
            note: switched(),
        }));
        assert_eq!(app.focused_run(), Some(RUN));
        let text = sized(&app, w, h);
        let line = text
            .find(&flat(&switched()))
            .unwrap_or_else(|| panic!("{w}x{h}: {text}"));
        let why = text
            .find("the follow-up run did not start: the stage takes no run now")
            .unwrap_or_else(|| panic!("{w}x{h}: {text}"));
        assert!(line < why, "{w}x{h}: {text}");
        assert!(text.contains("your text is kept"), "{w}x{h}: {text}");

        // A start that the check of provider data sharing stopped says the whole of it too.
        let mut app = ready(finished_events());
        app.update(Msg::Ext(Done::FollowUpStarted {
            run_id: RUN.into(),
            result: Err(stop()),
        }));
        let text = sized(&app, w, h);
        assert!(text.contains(&flat(&stop())), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_a_run_that_stopped_being_blocked_says_so_in_one_row() {
    for (w, h) in SIZES {
        let mut app = blocked();
        let mut clock = Clock::new();
        send(&mut app, &mut clock, "go");
        feed(
            &mut app,
            vec![harness_event(
                30,
                EventType::GoalStatus,
                json!({"status": "active", "phase": "green"}),
            )],
        );
        enter(&mut app, &mut clock, 400);
        let text = sized(&app, w, h);
        assert!(
            text.contains(
                "the run changed while you looked: it is not blocked now; nothing started"
            ),
            "{w}x{h}: {text}"
        );
    }
}

#[test]
fn b832_the_result_panel_of_resume_all_wraps_its_lines() {
    let report = format!("run {RUN}: not started: {}\n", stop());
    let out = CmdOutput {
        chunks: vec![
            Chunk {
                stream: Stream::Err,
                text: format!("{}\n", switched()),
            },
            Chunk {
                stream: Stream::Out,
                text: report.clone(),
            },
        ],
        code: 1,
        bearer: None,
    };
    // The panel takes at most two fifths of the screen; 40 rows leave it room for the whole answer.
    for (w, h) in [(80, 40), (120, 40)] {
        let mut app = ready(finished_events());
        app.update(Msg::CommandDone {
            argv: vec!["resume".into(), "--all".into()],
            out: out.clone(),
        });
        let text = sized(&app, w, h);
        assert!(text.contains(&flat(&switched())), "{w}x{h}: {text}");
        assert!(text.contains(&flat(&report)), "{w}x{h}: {text}");
    }
}

/// What `ham resume` writes for a resume of `started` (each ran to a new run) and `failed` (each stopped with `why`): its own report lines.
fn resume_report(started: &[&str], failed: &[&str], why: &str) -> CmdOutput {
    let outcomes: Vec<Outcome> = started
        .iter()
        .map(|id| Outcome {
            run_id: (*id).into(),
            result: Ok(NEW.into()),
        })
        .chain(failed.iter().map(|id| Outcome {
            run_id: (*id).into(),
            result: Err(why.into()),
        }))
        .collect();
    let plan = ham_core::resume::ResumePlan {
        resume: outcomes.iter().map(|o| o.run_id.clone()).collect(),
        skipped: vec![],
    };
    CmdOutput {
        chunks: vec![Chunk {
            stream: Stream::Out,
            text: format!(
                "{}\n",
                ham_core::resume::report_lines(&plan, &outcomes).join("\n")
            ),
        }],
        code: i32::from(!failed.is_empty()),
        bearer: None,
    }
}

/// `RUN` on screen with the card that says its resume did not start (`R` on the Runs screen, `run.provider_sharing` off).
fn run_with_resume_card() -> App {
    let mut app = runs_screen();
    app.update(Msg::Ext(Done::Resumed {
        outcomes: vec![Outcome {
            run_id: RUN.into(),
            result: Err(stop()),
        }],
    }));
    app.open_run(RUN);
    feed(&mut app, base_events());
    assert!(sized(&app, 120, 40).contains("resume did not start"));
    app
}

#[test]
fn b832_a_resume_by_command_takes_the_did_not_start_card_away() {
    // The two ways a resume of one run starts besides the Runs screen's `R`: `/resume <id>` and `/resume --all` (also `R` on a run's screen),
    // both of which run `ham resume` as a child command and answer with its report.
    let by_id = vec!["resume".to_string(), RUN.to_string()];
    let all = vec!["resume".to_string(), "--all".to_string()];
    for (w, h) in SIZES {
        for argv in [by_id.clone(), all.clone()] {
            let mut app = run_with_resume_card();
            app.update(Msg::CommandDone {
                argv: argv.clone(),
                out: resume_report(&[RUN], &[], ""),
            });
            app.open_run(RUN);
            feed(&mut app, base_events());
            let text = sized(&app, w, h);
            assert!(
                !text.contains("resume did not start"),
                "{argv:?} {w}x{h}: {text}"
            );
        }
    }
}

#[test]
fn b832_a_resume_by_command_that_did_not_start_this_run_keeps_its_card() {
    for (w, h) in SIZES {
        // The report says this run did not start, and another run did.
        let mut app = run_with_resume_card();
        app.update(Msg::CommandDone {
            argv: vec!["resume".into(), "--all".into()],
            out: resume_report(&[A], &[RUN], "the service did not answer"),
        });
        app.open_run(RUN);
        feed(&mut app, base_events());
        let text = sized(&app, w, h);
        assert!(text.contains("resume did not start"), "{w}x{h}: {text}");
        assert!(text.contains(&flat(&stop())), "{w}x{h}: {text}");

        // A command that is not a resume starts nothing, whatever its words are.
        let mut app = run_with_resume_card();
        app.update(Msg::CommandDone {
            argv: vec!["status".into()],
            out: resume_report(&[RUN], &[], ""),
        });
        app.open_run(RUN);
        feed(&mut app, base_events());
        let text = sized(&app, w, h);
        assert!(text.contains("resume did not start"), "{w}x{h}: {text}");
    }
}

// ---- a start that is answered while the person looks at another screen ------------------------------------------------

#[test]
fn b832_a_follow_up_refusal_on_its_own_run_points_to_the_card_above() {
    for (w, h) in SIZES {
        let mut app = ready(finished_events());
        app.update(Msg::Ext(Done::FollowUpStarted {
            run_id: RUN.into(),
            result: Err(stop()),
        }));
        let text = sized(&app, w, h);
        assert!(text.contains("the card above says why"), "{w}x{h}: {text}");
        assert!(text.contains(&flat(&stop())), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_a_follow_up_refusal_seen_from_another_run_names_the_run_and_not_a_card_above() {
    for (w, h) in SIZES {
        let mut app = ready(finished_events());
        // The start is out; the person opens another run before the answer comes.
        app.open_run(C);
        app.update(Msg::Ext(Done::FollowUpStarted {
            run_id: RUN.into(),
            result: Err(stop()),
        }));
        assert_eq!(app.focused_run(), Some(C));
        let text = sized(&app, w, h);
        assert!(!text.contains("the card above says why"), "{w}x{h}: {text}");
        assert!(
            text.contains("the follow-up of run 01K6A000 did not start"),
            "{w}x{h}: {text}"
        );
    }
}

#[test]
fn b832_a_retry_that_switched_is_said_where_the_person_looks_when_the_new_run_is_not_on_screen() {
    for (w, h) in SIZES {
        let mut app = blocked();
        // The retry is out; the person opens another run before the answer comes.
        app.open_run(C);
        app.update(Msg::Ext(Done::RetryNoted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
            note: switched(),
        }));
        assert_eq!(app.focused_run(), Some(C));
        // The notice row is one row: what switched is in its first words.
        let text = sized(&app, w, h);
        assert!(
            text.contains("ham switched the Bedrock data retention mode of AWS account"),
            "{w}x{h}: {text}"
        );
    }
}

#[test]
fn b832_a_retry_that_did_not_start_keeps_its_card_until_one_starts() {
    for (w, h) in SIZES {
        let mut app = ready(blocked_events());
        app.update(Msg::Ext(Done::RetryStarted {
            run_id: RUN.into(),
            result: Err(stop()),
        }));
        let text = sized(&app, w, h);
        assert!(text.contains("retry did not start"), "{w}x{h}: {text}");

        // The same run, retried again, starts: its card is gone when the person comes back to it.
        app.update(Msg::Ext(Done::RetryStarted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
        }));
        assert_eq!(app.focused_run(), Some(NEW));
        app.open_run(RUN);
        feed(&mut app, blocked_events());
        let text = sized(&app, w, h);
        assert!(!text.contains("retry did not start"), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_a_follow_up_that_did_not_start_keeps_its_card_until_one_starts() {
    for (w, h) in SIZES {
        let mut app = ready(finished_events());
        app.update(Msg::Ext(Done::FollowUpStarted {
            run_id: RUN.into(),
            result: Err(stop()),
        }));
        let text = sized(&app, w, h);
        assert!(text.contains("follow-up did not start"), "{w}x{h}: {text}");

        app.update(Msg::Ext(Done::FollowUpStarted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
        }));
        assert_eq!(app.focused_run(), Some(NEW));
        app.open_run(RUN);
        feed(&mut app, finished_events());
        let text = sized(&app, w, h);
        assert!(!text.contains("follow-up did not start"), "{w}x{h}: {text}");
    }
}

#[test]
fn b832_the_result_panel_of_resume_all_keeps_the_whole_answer_within_reach_at_80_by_24() {
    let report = format!("run {RUN}: not started: {}\n", stop());
    let out = CmdOutput {
        chunks: vec![
            Chunk {
                stream: Stream::Err,
                text: format!("{}\n", switched()),
            },
            Chunk {
                stream: Stream::Out,
                text: report.clone(),
            },
        ],
        code: 1,
        bearer: None,
    };
    let mut app = ready(finished_events());
    app.update(Msg::CommandDone {
        argv: vec!["resume".into(), "--all".into()],
        out,
    });
    let first = sized(&app, 80, 24);
    // The panel takes two fifths of 24 rows, so it scrolls: it says so, and the end of the answer comes into view by paging.
    assert!(first.contains(" of "), "{first}");
    assert!(first.contains("PgUp PgDn scroll"), "{first}");
    assert!(first.contains(&flat(&switched())[..60]), "{first}");
    let last_words = flat(&stop());
    let last_words = &last_words[last_words.len() - 30..];
    let mut clock = Clock::new();
    let mut seen = first.contains(last_words);
    for _ in 0..6 {
        press(&mut app, &mut clock, KeyCode::PageDown, 50);
        seen |= sized(&app, 80, 24).contains(last_words);
    }
    assert!(seen, "the end of the answer never came into view");
}
