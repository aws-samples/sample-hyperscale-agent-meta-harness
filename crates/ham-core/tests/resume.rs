//! The resume of paused and stalled runs as the client words it: which runs the key `R` and `ham resume --all` take, which they leave
//! and why, the one confirmation line, the idempotency key, the words for a refusal and the report of what each run did.
#![allow(clippy::unwrap_used)]

use ham_core::resume::{self, Outcome};
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};

const A: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1A";
const B: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1B";
const C: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1C";
const D: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1D";
const E: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1E";

fn run(id: &str, status: &str, mode: Option<&str>, continues: Option<&str>) -> RunSummary {
    let mut v = json!({
        "runId": id, "status": status, "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-11-03T08:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/Admin/me",
    });
    if let Some(m) = mode {
        v["resumeMode"] = json!(m);
        v["statusReason"] = json!("total_time: the run used the total time it was given");
    }
    if let Some(c) = continues {
        v["continueFrom"] = json!(c);
    }
    serde_json::from_value::<RunSummary>(v).unwrap()
}

#[test]
fn b689_the_plan_takes_the_paused_and_stalled_runs_that_delivered_code() {
    let runs = [
        run(A, "paused", Some("follow_up"), None),
        run(B, "stalled", Some("follow_up"), None),
        run(C, "active", None, None),
        run(D, "complete", None, None),
        run(E, "blocked", None, None),
    ];
    let plan = resume::plan(&runs);
    assert_eq!(plan.resume, [A, B]);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
}

#[test]
fn b689_a_run_with_nothing_to_continue_from_is_left_and_says_why() {
    let runs = [run(A, "stalled", Some("none"), None)];
    let plan = resume::plan(&runs);
    assert!(plan.resume.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].run_id, A);
    assert!(
        plan.skipped[0].why.contains("nothing"),
        "{}",
        plan.skipped[0].why
    );
}

#[test]
fn b689_a_run_that_already_has_a_follow_up_in_the_list_is_not_resumed_again() {
    // B continues A: pressing R twice (or resuming by hand first) never starts a second continuation of A. If B itself stalled, B is the leaf.
    let runs = [
        run(B, "stalled", Some("follow_up"), Some(A)),
        run(A, "paused", Some("follow_up"), None),
    ];
    let plan = resume::plan(&runs);
    assert_eq!(
        plan.resume,
        [B],
        "the newest run of the line is the one that continues"
    );
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].run_id, A);
    assert!(plan.skipped[0].why.contains(B), "{}", plan.skipped[0].why);
}

#[test]
fn b689_a_run_the_service_has_not_described_is_resumed_as_a_follow_up() {
    // An older backend sends the status and no resume mode; a delivered paused run can still go on.
    let runs = [run(A, "paused", None, None)];
    assert_eq!(resume::plan(&runs).resume, [A]);
}

#[test]
fn b689_the_confirmation_is_one_line_that_names_the_count_and_the_split() {
    let runs = [
        run(A, "paused", Some("follow_up"), None),
        run(B, "stalled", Some("follow_up"), None),
        run(C, "stalled", Some("none"), None),
    ];
    let line = resume::confirm_line(&resume::plan(&runs)).unwrap();
    assert!(!line.contains('\n'), "{line}");
    assert!(line.starts_with("Resume 2 runs"), "{line}");
    assert!(line.contains("1 skipped"), "{line}");
    assert!(line.contains("Enter"), "{line}");
    assert_eq!(
        resume::confirm_line(&resume::plan(&[run(D, "complete", None, None)])),
        None
    );
    let one =
        resume::confirm_line(&resume::plan(&[run(A, "paused", Some("follow_up"), None)])).unwrap();
    assert!(one.starts_with("Resume 1 run:"), "{one}");
}

#[test]
fn b689_the_key_is_the_same_for_every_press_and_belongs_to_the_run() {
    assert_eq!(resume::start_key(A), format!("resume-{A}"));
    assert_eq!(resume::start_key(A), resume::start_key(A));
    assert_ne!(resume::start_key(A), resume::start_key(B));
}

#[test]
fn b689_a_refusal_is_told_in_the_servers_words_with_what_to_do() {
    let busy = resume::explain(
        409,
        "run-active",
        "run X has not ended; there is nothing to resume yet",
    );
    assert!(busy.contains("has not ended"), "{busy}");
    let none = resume::explain(
        409,
        "no-delivered-head",
        "run X delivered no code to continue from",
    );
    assert!(none.contains("start a new run"), "{none}");
    let down = resume::explain(503, "unavailable", "the harness is unavailable");
    assert!(down.contains("press the key again"), "{down}");
    let old = resume::explain(404, "not-found", "no such route");
    assert!(old.contains("newer backend"), "{old}");
    let silent = resume::explain(0, "", "connection reset");
    assert!(silent.contains("may not have reached"), "{silent}");
    // control characters of the service's words never reach the screen
    assert!(!resume::explain(409, "x", "a\u{1b}[2Jb").contains('\u{1b}'));
}

#[test]
fn b689_the_report_names_every_run_what_it_did() {
    let plan = resume::plan(&[
        run(A, "paused", Some("follow_up"), None),
        run(B, "stalled", Some("follow_up"), None),
        run(C, "stalled", Some("none"), None),
    ]);
    let outcomes = [
        Outcome {
            run_id: A.to_string(),
            result: Ok(D.to_string()),
        },
        Outcome {
            run_id: B.to_string(),
            result: Err("the harness is unavailable".to_string()),
        },
    ];
    let lines = resume::report_lines(&plan, &outcomes);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[0].contains(A) && lines[0].contains(D) && lines[0].contains("follow-up"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains(B) && lines[1].contains("not started"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].contains(C) && lines[2].contains("skipped"),
        "{}",
        lines[2]
    );
}

#[test]
fn b689_the_report_as_json_has_one_object_per_run() {
    let plan = resume::plan(&[
        run(A, "paused", Some("follow_up"), None),
        run(C, "stalled", Some("none"), None),
    ]);
    let outcomes = [Outcome {
        run_id: A.to_string(),
        result: Ok(D.to_string()),
    }];
    let objects: Vec<Value> = resume::report_json(&plan, &outcomes);
    assert_eq!(
        objects[0],
        json!({"runId": A, "result": "resumed", "mode": "follow_up", "newRunId": D})
    );
    assert_eq!(objects[1]["runId"], json!(C));
    assert_eq!(objects[1]["result"], json!("skipped"));
    assert!(objects[1]["reason"].as_str().unwrap().contains("nothing"));
}

#[test]
fn b689_at_most_four_runs_start_at_once() {
    assert_eq!(resume::MAX_AT_ONCE, 4);
}

#[test]
fn b689_the_command_takes_one_run_id_or_all_and_nothing_else() {
    use ham_core::command::{Command, parse};
    let argv = |words: &[&str]| words.iter().map(ToString::to_string).collect::<Vec<_>>();
    let one = parse(&argv(&["resume", A])).unwrap();
    assert_eq!(
        one.command,
        Command::Resume {
            run_id: Some(A.to_string()),
            all: false,
            dry_run: false
        }
    );
    let all = parse(&argv(&["resume", "--all", "--dry-run"])).unwrap();
    assert_eq!(
        all.command,
        Command::Resume {
            run_id: None,
            all: true,
            dry_run: true
        }
    );
    assert!(
        parse(&argv(&["resume"])).is_err(),
        "neither a run id nor --all"
    );
    assert!(parse(&argv(&["resume", A, "--all"])).is_err(), "both");
    // the words round-trip, so the terminal app's hotkey and a typed line mean the same command
    assert_eq!(all.to_argv(), argv(&["resume", "--all", "--dry-run"]));
}

#[test]
fn b689_a_list_with_nothing_to_resume_and_runs_left_out_says_what_it_found() {
    let plan = resume::plan(&[
        run(A, "stalled", Some("none"), None),
        run(B, "paused", Some("none"), None),
    ]);
    let line = resume::confirm_line(&plan).unwrap();
    assert!(
        line.contains("Resume 0 runs, 2 skipped: nothing here can be resumed"),
        "{line}"
    );
    assert!(resume::confirm_line(&resume::plan(&[])).is_none());
}

#[test]
fn b689_the_service_refusals_of_a_model_and_of_a_reused_key_say_what_to_do_next() {
    let model = resume::explain(422, "unknown-model", "the model is not offered");
    assert!(
        model.contains("the model is not offered") && model.contains("pick another model"),
        "{model}"
    );
    let key = resume::explain(422, "idempotency-key-reused", "ignored");
    assert!(
        key.contains("already used for another request") && key.contains("start a new run"),
        "{key}"
    );
}

#[test]
fn b689_a_failed_resume_is_one_json_object_with_its_reason() {
    let plan = resume::plan(&[run(A, "paused", Some("follow_up"), None)]);
    let outcomes = [Outcome {
        run_id: A.to_string(),
        result: Err("the service did not answer".to_string()),
    }];
    let objects = resume::report_json(&plan, &outcomes);
    assert_eq!(
        objects,
        vec![json!({"runId": A, "result": "failed", "reason": "the service did not answer"})]
    );
}

/// The runs a `ham resume` report says started: what the terminal app reads from the child's answer to take the card of a resume that
/// did not start away from a run whose resume started later.
#[test]
fn b832_the_runs_a_report_says_started_are_the_ones_whose_resume_started() {
    let plan = resume::plan(&[
        run(A, "paused", Some("follow_up"), None),
        run(B, "stalled", Some("follow_up"), None),
        run(C, "paused", Some("none"), None),
    ]);
    let outcomes = [
        Outcome {
            run_id: A.to_string(),
            result: Ok(D.to_string()),
        },
        Outcome {
            run_id: B.to_string(),
            // A reason that holds the words of a start does not make the run one that started.
            result: Err(format!(
                "run {E}: follow-up run {D} started from the code it delivered"
            )),
        },
    ];
    let lines = resume::report_lines(&plan, &outcomes);
    assert_eq!(resume::started_run_ids(&lines), vec![A.to_string()]);
    // The same report as JSON lines.
    let json: Vec<String> = resume::report_json(&plan, &outcomes)
        .iter()
        .map(Value::to_string)
        .collect();
    assert_eq!(resume::started_run_ids(&json), vec![A.to_string()]);
    // Not a report: a dry run, a line of another command, a blank line, and the line of a line that only looks like one.
    let other = [
        format!("run {A}: would start a follow-up run"),
        String::new(),
        format!("run {A}: skipped: already continued by run {D}"),
        format!("not started: run {A}: follow-up run {D} started from the code it delivered"),
    ];
    assert!(resume::started_run_ids(&other).is_empty(), "{other:?}");
}
