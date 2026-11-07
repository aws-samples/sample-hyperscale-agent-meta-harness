//! `packager` and the review round: EVIDENCE.md carries one section and one sentence about the round when
//! `PackageRequest.direct.review` is set, and renders exactly as before when it is not. The sentence is the protocol's own
//! (`DirectReview::sentence`), so the zip, the card and the summary say the same words, and no text says the harness checked, tested, gated
//! or evaluated the work.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::packager::{PackageRequest, render_evidence};
use harness_protocol::direct::{
    DirectEvidence, DirectOutcome, DirectReview, DirectRuleRef, HeadSource, REVIEW_COMPLETED,
    REVIEW_FAILED, REVIEW_RAN, REVIEW_SKIP_BLOCKED, banned_claim_word,
};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn evidence(review: Option<DirectReview>) -> DirectEvidence {
    DirectEvidence {
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
        review,
        flow: None,
    }
}

fn md(review: Option<DirectReview>) -> String {
    render_evidence(&PackageRequest {
        run_id: RUN.into(),
        repo: "hamh-e2e-python-calc-20260925".into(),
        base_sha: None,
        head_sha: HEAD.into(),
        objective: "Add percent_change(a, b)".into(),
        pr_url: None,
        direct: evidence(review),
    })
}

fn ran() -> DirectReview {
    DirectReview {
        status: REVIEW_RAN.into(),
        reason: None,
        commit_before: Some("a".repeat(40)),
        commit_after: Some("b".repeat(40)),
        changed_files: Some(true),
        seconds: Some(312),
        input_tokens: Some(4321),
        output_tokens: Some(876),
        result: Some(REVIEW_COMPLETED.into()),
    }
}

#[test]
fn a_run_without_the_record_renders_exactly_as_before() {
    let text = md(None);
    assert!(!text.contains("Review round"), "{text}");
    assert!(!text.contains("review"), "{text}");
    assert!(
        text.contains("It ran no test and made no judgment on the work."),
        "{text}"
    );
}

#[test]
fn a_round_that_ran_adds_a_section_with_the_one_sentence() {
    let text = md(Some(ran()));
    assert!(text.contains("## Review round"), "{text}");
    assert!(text.contains(&ran().sentence()), "{text}");
    assert!(text.contains("312 s"), "{text}");
    assert!(text.contains("4321 input and 876 output tokens"), "{text}");
    // The section sits between Cline's message and what the harness did; the outcome row is untouched.
    let (a, b, c) = (
        text.find("## Cline's last message").unwrap(),
        text.find("## Review round").unwrap(),
        text.find("## What the harness did").unwrap(),
    );
    assert!(a < b && b < c, "{text}");
    assert!(text.contains("| Outcome | Cline reported done |"), "{text}");
}

#[test]
fn a_round_that_was_skipped_or_failed_says_so_in_one_sentence() {
    let skipped = md(Some(DirectReview::skipped(REVIEW_SKIP_BLOCKED)));
    assert!(
        skipped.contains(
            "The harness did not ask Cline to review its delivery: Cline ended its message with a BLOCKED line."
        ),
        "{skipped}"
    );
    let mut failed = ran();
    failed.result = Some(REVIEW_FAILED.into());
    let text = md(Some(failed));
    assert!(
        text.contains("the outcome of the run is the one the turn before it had"),
        "{text}"
    );
    // The outcome row still names the turn before the round.
    assert!(text.contains("| Outcome | Cline reported done |"), "{text}");
}

#[test]
fn no_text_of_the_section_claims_a_check_of_the_work() {
    for r in [
        ran(),
        DirectReview::skipped(REVIEW_SKIP_BLOCKED),
        DirectReview::skipped("a_reason_of_a_later_build"),
    ] {
        let text = md(Some(r));
        let section = text
            .split("## Review round")
            .nth(1)
            .unwrap()
            .split("## What the harness did")
            .next()
            .unwrap();
        assert_eq!(banned_claim_word(section), None, "{section}");
        assert!(!section.contains('\r'), "{section}");
    }
}
