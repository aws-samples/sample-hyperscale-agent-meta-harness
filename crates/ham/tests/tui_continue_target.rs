//! The typed `/continue` follows the target of the run it continues. The preview names the target when the screen knows it
//! (the run on screen, or a row of the run list) and says it is read when the run starts when not. The argv that was previewed is the argv
//! that runs: the screen adds no `--repo` or `--s3-only` of its own, so the child `ham continue` resolves the target exactly as the shell
//! does and prints the same lines. The second Enter hands the terminal to that child, with the text, because the in-process runner does
//! not follow a run.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use ham::tui::{App, Effect, Msg};
use harness_protocol::api::v1::{RunDetail, RunSummary};
use serde_json::{Value, json};

const OTHER: &str = "01K6A0000000000000000000R2";

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

fn summary(run: &str, target: Value) -> Value {
    json!({"runId": run, "status": "complete", "phase": "green", "target": target,
           "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"})
}

/// A finished run on screen whose summary names `target`.
fn finished_on(target: Value) -> App {
    let mut app = ready(finished_events());
    let detail: RunDetail = serde_json::from_value(json!({
        "summary": summary(RUN, target), "objective": "add percent_change", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(detail)));
    app
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

/// Types `line`, presses the first Enter and returns the text of the screen.
fn previewed(app: &mut App, clock: &mut Clock, line: &str) -> String {
    let effects = send(app, clock, line);
    assert!(
        effects.is_empty(),
        "ran without a second Enter: {effects:?}"
    );
    flat(&screen(app, 160, 50))
}

#[test]
fn the_preview_names_the_repository_of_a_repository_run_and_the_argv_is_untouched() {
    let mut app = finished_on(json!({"mode": "repo", "repo": "cc:calc"}));
    let mut clock = Clock::new();
    let text = previewed(&mut app, &mut clock, "/continue add the docs");
    for want in [
        "ham continue",
        RUN,
        "billable",
        "Target: cc:calc,",
        "pull request",
        &format!("the target of run {RUN}"),
        "until the new run ends",
    ] {
        assert!(text.contains(want), "{want}\n{text}");
    }
    assert!(!text.contains("read when it starts"), "{text}");
    // The second Enter hands the terminal to the child with the line as typed: no target flag was added.
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["continue", RUN, "add the docs"])
        }]
    );
}

#[test]
fn a_run_that_made_its_repository_is_followed_on_it_and_an_s3_only_run_by_a_zip() {
    let mut app = finished_on(json!({"mode": "new-repo", "repo": "cc:calc"}));
    let mut clock = Clock::new();
    let text = previewed(&mut app, &mut clock, "/continue add the docs");
    assert!(text.contains("Target: cc:calc,"), "{text}");

    let mut app = finished_on(json!({"mode": "s3-only"}));
    let mut clock = Clock::new();
    let text = previewed(&mut app, &mut clock, "/continue add the docs");
    assert!(
        text.contains("Target: s3-only,") && text.contains("no pull request"),
        "{text}"
    );
    assert!(!text.contains("cc:calc"), "{text}");
}

#[test]
fn a_target_typed_on_the_line_is_the_one_the_preview_names() {
    // The shell form typed in the box: the person's flag wins over the run's target, and the preview says it was chosen.
    let mut app = finished_on(json!({"mode": "repo", "repo": "cc:calc"}));
    let mut clock = Clock::new();
    let text = previewed(
        &mut app,
        &mut clock,
        &format!("ham continue {RUN} 'add the docs' --s3-only"),
    );
    assert!(
        text.contains("Target: s3-only,") && text.contains("chosen on the line"),
        "{text}"
    );
    assert!(!text.contains("cc:calc"), "{text}");
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["continue", RUN, "add the docs", "--s3-only"])
        }]
    );
}

#[test]
fn when_the_screen_does_not_know_the_target_the_preview_says_it_is_read_when_the_run_starts() {
    // A finished run whose summary has not arrived.
    let mut app = ready(finished_events());
    let mut clock = Clock::new();
    let text = previewed(&mut app, &mut clock, "/continue add the docs");
    assert!(
        text.contains(&format!(
            "Target: the target of run {RUN}, read when it starts"
        )),
        "{text}"
    );
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["continue", RUN, "add the docs"])
        }]
    );
}

#[test]
fn another_run_named_with_run_is_known_from_the_run_list() {
    let mut app = finished_on(json!({"mode": "s3-only"}));
    let rows: Vec<RunSummary> = vec![
        serde_json::from_value(summary(OTHER, json!({"mode": "repo", "repo": "cc:calc"}))).unwrap(),
    ];
    app.update(Msg::Runs(Ok(rows)));
    let mut clock = Clock::new();
    let text = previewed(
        &mut app,
        &mut clock,
        &format!("/continue --run {OTHER} add the docs"),
    );
    assert!(
        text.contains(OTHER) && text.contains("Target: cc:calc,"),
        "the target of the named run, not of the run on screen:\n{text}"
    );
    assert!(
        text.contains(&format!("the target of run {OTHER}")),
        "{text}"
    );
}

#[test]
fn a_run_id_after_the_slash_word_is_refused_and_the_text_stays_in_the_box() {
    let mut app = finished_on(json!({"mode": "s3-only"}));
    let mut clock = Clock::new();
    let line = format!("/continue {OTHER} add the docs");
    let effects = send(&mut app, &mut clock, &line);
    assert!(effects.is_empty(), "{effects:?}");
    let text = flat(&screen(&app, 160, 50));
    assert!(text.contains("Nothing was sent"), "{text}");
    assert!(
        text.contains(&format!("/continue --run {OTHER} add the docs")),
        "the panel shows the line to type:\n{text}"
    );
    assert_eq!(app.chat_text().as_deref(), Some(line.as_str()));
    // Nothing waits for a second Enter.
    assert!(enter(&mut app, &mut clock, 400).is_empty());
}
