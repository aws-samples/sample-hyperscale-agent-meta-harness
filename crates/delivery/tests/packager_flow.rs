//! `packager` and the flow a run declared: EVIDENCE.md carries one `Flow` row with the word `quick`, `full` or `none` when
//! `PackageRequest.direct.flow` is set, and renders exactly as before when it is not. The row sits beside the other facts of the run and
//! changes nothing about the outcome; no text says the harness checked, tested, gated or evaluated the work.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::packager::{PackageRequest, render_evidence};
use harness_protocol::direct::{
    DirectEvidence, DirectFlow, DirectOutcome, DirectRuleRef, HeadSource, banned_claim_word,
};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn md(flow: Option<DirectFlow>) -> String {
    render_evidence(&PackageRequest {
        run_id: RUN.into(),
        repo: "hamh-e2e-python-calc-20260925".into(),
        base_sha: None,
        head_sha: HEAD.into(),
        objective: "Add percent_change(a, b)".into(),
        pr_url: None,
        direct: DirectEvidence {
            outcome: DirectOutcome::ReportedDone,
            label: DirectOutcome::ReportedDone.label().into(),
            detail: None,
            stop_reason: Some("end_turn".into()),
            cline_message: Some("Added pct(a, b).".into()),
            model_id: "us.anthropic.claude-opus-5-5".into(),
            engine_version: None,
            rules: vec![DirectRuleRef {
                name: "00-my-rules.md".into(),
                sha256: "ab".repeat(32),
                bytes: Some(22),
            }],
            usage: None,
            wall_s: Some(3731),
            head_source: HeadSource::Final,
            final_model_id: None,
            model_switches: 0,
            test_report: None,
            review: None,
            flow,
        },
    })
}

#[test]
fn a_run_without_the_record_renders_exactly_as_before() {
    let text = md(None);
    assert!(!text.contains("| Flow |"), "{text}");
    assert!(!text.contains("Flow"), "{text}");
    assert!(
        text.contains("It ran no test and made no judgment on the work."),
        "{text}"
    );
}

#[test]
fn each_word_is_one_row_of_the_table() {
    for flow in [DirectFlow::Quick, DirectFlow::Full, DirectFlow::Undeclared] {
        let text = md(Some(flow));
        let row = format!("| Flow | {} |", flow.word());
        assert_eq!(text.matches(&row).count(), 1, "{text}");
        assert_eq!(text.matches("| Flow |").count(), 1, "{text}");
    }
}

#[test]
fn the_row_sits_in_the_table_and_the_outcome_row_is_untouched() {
    let with = md(Some(DirectFlow::Quick));
    let without = md(None);
    let (table_end, row) = (
        with.find("\n\n## Objective").unwrap(),
        with.find("| Flow | quick |").unwrap(),
    );
    assert!(row < table_end, "{with}");
    assert!(with.contains("| Outcome | Cline reported done |"), "{with}");
    // Only the one line differs from the same evidence without the record.
    let stripped: String = with
        .lines()
        .filter(|l| *l != "| Flow | quick |")
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(stripped, without);
}

#[test]
fn no_row_says_the_work_was_checked() {
    for flow in [DirectFlow::Quick, DirectFlow::Full, DirectFlow::Undeclared] {
        let text = md(Some(flow));
        assert_eq!(banned_claim_word(&text), None, "{text}");
    }
}
