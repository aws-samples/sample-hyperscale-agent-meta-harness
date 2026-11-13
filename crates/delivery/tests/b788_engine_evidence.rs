//! EVIDENCE.md names the engine that ran. A Cline run writes the file it always wrote; an OpenCode run has the same file with `OpenCode` where it says who
//! reported, left, stopped and worked, and the heading of the last message reads `## OpenCode's last message`. The client reads either heading.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::packager::{PackageRequest, render_evidence};
use harness_protocol::common::Engine;
use harness_protocol::direct::{
    DirectEvidence, DirectOutcome, DirectReview, DirectRuleRef, HeadSource, REVIEW_SKIP_BLOCKED,
    banned_claim_word,
};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn evidence(engine: Engine, message: Option<&str>) -> DirectEvidence {
    DirectEvidence {
        outcome: DirectOutcome::ReportedDone,
        label: DirectOutcome::ReportedDone.label_for_engine(engine, None),
        detail: None,
        stop_reason: Some("end_turn".into()),
        cline_message: message.map(str::to_string),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        engine_version: Some("9.9.9".into()),
        rules: vec![DirectRuleRef {
            name: "00-my-rules.md".into(),
            sha256: "ab".repeat(32),
            bytes: Some(22),
        }],
        usage: None,
        wall_s: Some(60),
        head_source: HeadSource::Final,
        final_model_id: None,
        model_switches: 0,
        test_report: None,
        review: None,
        flow: None,
        engine: (engine != Engine::CLINE).then_some(engine),
    }
}

fn md(d: DirectEvidence) -> String {
    render_evidence(&PackageRequest {
        run_id: RUN.into(),
        repo: "hamh-e2e-python-calc".into(),
        base_sha: None,
        head_sha: HEAD.into(),
        objective: "Add pct(a, b).".into(),
        pr_url: None,
        direct: d,
    })
}

#[test]
fn b788_the_evidence_of_an_opencode_run_differs_from_a_cline_run_only_in_the_name() {
    let cline = md(evidence(Engine::CLINE, Some("Added pct.")));
    let open = md(evidence(Engine::OPENCODE, Some("Added pct.")));
    assert_ne!(cline, open);
    assert_eq!(open.replace("OpenCode", "Cline"), cline);
    assert!(!open.contains("Cline"), "{open}");
}

#[test]
fn b788_the_rows_and_the_heading_of_an_opencode_run_name_opencode() {
    let open = md(evidence(Engine::OPENCODE, Some("Added pct.")));
    assert!(
        open.contains("| Outcome | OpenCode reported done |"),
        "{open}"
    );
    assert!(
        open.contains("| OpenCode stop reason | `end_turn` |"),
        "{open}"
    );
    assert!(open.contains("| OpenCode version | 9.9.9 |"), "{open}");
    assert!(open.contains("\n## OpenCode's last message\n"), "{open}");
    assert!(
        open.contains("committed what OpenCode left in the tree"),
        "{open}"
    );
}

#[test]
fn b788_a_cline_run_keeps_the_cline_heading_and_the_cline_rows() {
    let cline = md(evidence(Engine::CLINE, Some("Added pct.")));
    assert!(cline.contains("\n## Cline's last message\n"), "{cline}");
    assert!(
        cline.contains("| Cline stop reason | `end_turn` |"),
        "{cline}"
    );
    assert!(cline.contains("| Cline version | 9.9.9 |"), "{cline}");
    // An old evidence file has no engine member and renders as Cline.
    let mut d = evidence(Engine::CLINE, Some("Added pct."));
    d.engine = None;
    assert_eq!(md(d), cline);
}

#[test]
fn b788_a_run_with_no_message_says_which_engine_left_none() {
    let open = md(evidence(Engine::OPENCODE, None));
    assert!(open.contains("OpenCode left no final message."), "{open}");
    let cline = md(evidence(Engine::CLINE, None));
    assert!(cline.contains("Cline left no final message."), "{cline}");
}

#[test]
fn b788_the_review_sentence_names_the_engine_and_claims_nothing() {
    let mut d = evidence(Engine::OPENCODE, Some("m"));
    d.review = Some(DirectReview::skipped(REVIEW_SKIP_BLOCKED));
    let open = md(d);
    assert!(
        open.contains("The harness did not ask OpenCode to review its delivery: OpenCode ended its message with a BLOCKED line."),
        "{open}"
    );
    assert!(!open.contains("Cline"), "{open}");
    assert_eq!(banned_claim_word(&open), None, "{open}");
    // The protocol's one sentence for Cline is what it was.
    assert_eq!(
        DirectReview::skipped(REVIEW_SKIP_BLOCKED).sentence(),
        DirectReview::skipped(REVIEW_SKIP_BLOCKED).sentence_for("Cline")
    );
}

#[test]
fn b788_the_evidence_of_a_cline_run_has_no_engine_member() {
    let v = serde_json::to_value(evidence(Engine::CLINE, Some("m"))).unwrap();
    assert!(v.get("engine").is_none());
    let v = serde_json::to_value(evidence(Engine::OPENCODE, Some("m"))).unwrap();
    assert_eq!(v["engine"], "opencode");
}
