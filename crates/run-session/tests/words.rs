//! The words and the limits of the pull request and the package request (review of the run machine, S5: the
//! mutants `description_sentence`, `desc_cut`, `objective_uncut` and `wall_time_units` survived).
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::common::Engine;
use harness_protocol::direct::DirectOutcome;
use run_session::ops::{Op, handle};
use run_session::text::{self, PrFacts};
use serde_json::{Value, json};
use support::fakes::*;

fn facts<'a>(message: Option<&'a str>, wall_s: Option<u64>) -> PrFacts<'a> {
    PrFacts {
        run_id: RUN,
        engine: Engine::CLINE,
        outcome: DirectOutcome::ReportedDone,
        model_id: MODEL,
        wall_s,
        message,
        detail: None,
        from_snapshot: false,
        snapshot_at: None,
        continues: None,
    }
}

#[test]
fn the_wall_time_names_every_unit_it_shows() {
    assert_eq!(text::wall_time(5), "5s");
    assert_eq!(text::wall_time(59), "59s");
    assert_eq!(text::wall_time(60), "1m 0s");
    assert_eq!(text::wall_time(131), "2m 11s");
    assert_eq!(text::wall_time(3_599), "59m 59s");
    assert_eq!(text::wall_time(3_600), "1h 0m 0s");
    assert_eq!(text::wall_time(3_731), "1h 2m 11s");
    assert_eq!(text::wall_time(90_061), "25h 1m 1s");
}

#[test]
fn the_description_is_the_label_the_facts_the_message_and_the_one_sentence_about_checking() {
    let d = text::description(&facts(Some("Added pct.\nTwo tests."), Some(3_731)));
    assert_eq!(
        d,
        "Cline reported done\nRun 01M4X3Q7R9S0V1W2X3Y4Z5A6B7. Model us.anthropic.claude-opus-5-5. Wall time 1h 2m 11s.\n\n\
         Cline's last message:\n> Added pct.\n> Two tests.\n\n\
         Nothing in this pull request was checked by the harness. The zip carries what Cline reported (EVIDENCE.md)."
    );
    // no wall time, no message
    let d = text::description(&facts(None, None));
    assert_eq!(
        d,
        "Cline reported done\nRun 01M4X3Q7R9S0V1W2X3Y4Z5A6B7. Model us.anthropic.claude-opus-5-5.\n\n\
         Cline left no final message.\n\n\
         Nothing in this pull request was checked by the harness. The zip carries what Cline reported (EVIDENCE.md)."
    );
    let d = text::description(&facts(Some("   \n"), None));
    assert!(d.contains("Cline left no final message."), "{d}");
}

#[test]
fn a_message_of_exactly_six_thousand_characters_goes_whole_into_the_description() {
    let m = "a".repeat(6_000);
    let d = text::description(&facts(Some(&m), None));
    assert!(d.contains(&format!("> {m}")), "the whole message is quoted");
    assert!(!d.contains("cut here"), "{}", &d[d.len() - 300..]);
    assert!(text::comments(Some(&m)).is_empty());
    // one more character cuts it at 6,000 and moves the whole message to a comment
    let m = "a".repeat(6_001);
    let d = text::description(&facts(Some(&m), None));
    assert!(d.contains(&format!("> {}\n> … (cut here", "a".repeat(6_000))));
    assert!(!d.contains(&"a".repeat(6_001)));
    let c = text::comments(Some(&m));
    assert_eq!(c.len(), 1);
    assert!(c[0].contains(&"a".repeat(6_001)));
}

#[test]
fn the_title_and_the_comment_keep_their_limits_to_the_character() {
    assert_eq!(
        text::title(&"x".repeat(120)),
        format!("ham: {}", "x".repeat(120))
    );
    assert_eq!(
        text::title(&"x".repeat(121)),
        format!("ham: {}…", "x".repeat(120))
    );
    assert_eq!(text::title("  \n\t "), "ham: direct run");
    let long = format!("{}\n", "word ".repeat(3_000));
    let c = text::comments(Some(&long));
    assert_eq!(c[0].chars().count(), 10_240);
}

#[tokio::test]
async fn the_objective_of_the_package_is_the_prompt_cut_at_eight_thousand_characters() {
    for (len, want) in [
        (7_999usize, 7_999usize),
        (8_000, 8_000),
        (8_001, 8_000),
        (20_000, 8_000),
    ] {
        let mem = Mem::default();
        mem.seed(repo_target(), &"p".repeat(len), MODEL);
        mem.put_bundle();
        mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
        let op: Op = serde_json::from_value(
            json!({"op": "plan-delivery", "runId": RUN, "staged": staged(),
            "session": {"turn": turn_output(true), "failure": {}}}),
        )
        .unwrap();
        let p: Value = handle(&mem.deps(), &op).await.unwrap();
        assert_eq!(
            p["package"]["objective"].as_str().unwrap().chars().count(),
            want,
            "prompt of {len} characters"
        );
    }
}

#[tokio::test]
async fn the_pull_request_description_carries_the_wall_time_of_the_run() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(DirectOutcome::ReportedDone, Some("m"), true));
    let op: Op = serde_json::from_value(
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(),
        "session": {"turn": turn_output(true), "failure": {}}}),
    )
    .unwrap();
    let p: Value = handle(&mem.deps(), &op).await.unwrap();
    let d = p["pushPr"]["description"].as_str().unwrap();
    assert!(d.contains("Wall time 1h 2m 11s."), "{d}");
}

#[test]
fn the_description_of_a_stop_by_the_harness_says_so_in_its_label_line() {
    let mut f = facts(Some("Still working."), Some(60));
    f.outcome = DirectOutcome::StoppedOther;
    f.detail = Some(
        "The harness ended the run: 3 rounds in a row changed no file. Its words: \"BLOCKED: x\"",
    );
    let d = text::description(&f);
    assert_eq!(
        d.lines().next(),
        Some("Cline stopped: the harness ended the run (3 rounds in a row changed no file)")
    );
    assert!(!d.contains("unknown stop reason"), "{d}");
    // another detail keeps the frozen label
    f.detail = Some("silence 900 s");
    assert_eq!(
        text::description(&f).lines().next(),
        Some(DirectOutcome::StoppedOther.label())
    );
}

// ---- a follow-up run ------------------------------------------------------------------------------------------

const OLD_RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B8";
const OLD_PR: &str = "https://us-east-1.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc/pull-requests/7/details";

#[test]
fn a_follow_up_description_names_the_run_it_continues_and_its_pull_request() {
    let mut f = facts(Some("Added pct."), Some(5));
    f.continues = Some(text::Continues {
        run_id: OLD_RUN,
        pull_request_url: Some(OLD_PR),
    });
    let d = text::description(&f);
    assert!(
        d.contains(&format!(
            "Follow-up of run {OLD_RUN}; it continues the pull request {OLD_PR}."
        )),
        "{d}"
    );
    // The sentence sits between the run line and Cline's message; the rest of the description is what it was.
    let plain = text::description(&facts(Some("Added pct."), Some(5)));
    assert_eq!(
        d.replace(
            &format!("Follow-up of run {OLD_RUN}; it continues the pull request {OLD_PR}.\n\n"),
            ""
        ),
        plain
    );
    assert!(!d.to_lowercase().contains("verified") && !d.to_lowercase().contains("tested"));
    assert!(d.chars().count() <= text::PR_DESCRIPTION_MAX_CHARS);
}

#[test]
fn a_follow_up_of_a_run_that_opened_no_pull_request_says_so() {
    let mut f = facts(None, None);
    f.continues = Some(text::Continues {
        run_id: OLD_RUN,
        pull_request_url: None,
    });
    let d = text::description(&f);
    assert!(
        d.contains(&format!(
            "Follow-up of run {OLD_RUN}; that run opened no pull request."
        )),
        "{d}"
    );
}

#[test]
fn a_description_at_its_longest_still_fits_with_the_follow_up_sentence() {
    let long = "x".repeat(20_000);
    let mut f = facts(Some(&long), Some(5));
    f.continues = Some(text::Continues {
        run_id: OLD_RUN,
        pull_request_url: Some(OLD_PR),
    });
    let d = text::description(&f);
    assert!(
        d.chars().count() <= text::PR_DESCRIPTION_MAX_CHARS,
        "{}",
        d.chars().count()
    );
    assert!(
        d.contains("Follow-up of run"),
        "the sentence is not the part that is cut"
    );
}
