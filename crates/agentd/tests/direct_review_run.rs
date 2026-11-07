//! The review round of the direct driver end to end, in process: `POST /invocations` with `kind: "direct"`
//! against a fake AWS (S3 and the internal API) and the fake ACP engine, with a real git repository (the run's bundle). The shared test world
//! switches the stage's round off so that every other test sends the one prompt it always sent; these tests switch it on through the stage's
//! setting, or leave it to the request's `review` member. The pure rules and the stage loop are pinned in `direct_review.rs`; this file pins the
//! wiring: the request member, the stage setting, the real git reads, the summary and the events.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use direct_support::{CfgEdit, Direct, Opts, PROMPT};
use harness_protocol::direct::{
    DirectOutcome, REVIEW_COMPLETED, REVIEW_RAN, REVIEW_SKIP_OFF, REVIEW_SKIPPED, ReviewOptions,
};
use harness_protocol::envelope::EventType;
use serde_json::json;
use std::time::Duration;

/// The stage's switch on, and its guard at the least the setting allows: the test world's run has two minutes, which is less than the stage's
/// default guard of 20 minutes (the round would be skipped as "no_time").
fn stage_on() -> Option<CfgEdit> {
    Some(Box::new(|cfg: &mut Config| {
        cfg.direct.review_round = true;
        cfg.direct.review_guard = Duration::from_secs(60);
    }))
}

/// `nudge_done`: the engine edits two files and ends its turn with "I made a start."; its second answer is "DONE: everything is finished.".
fn opts() -> Opts {
    Opts {
        scenario: "nudge_done",
        edit_cfg: stage_on(),
        // Minutes, not seconds, so that a loaded host does not use up the guard before the first turn ends.
        timeout_s: 900,
        ..Opts::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_done_run_with_changed_files_is_followed_by_one_review_prompt_and_the_summary_records_it()
{
    let d = Direct::start(opts()).await;
    d.invoke().await;
    let sent = d.prompts_received();
    assert_eq!(sent.len(), 2, "{sent:?} {:?}", d.summary().review);
    assert_eq!(sent[0], PROMPT);
    let s = d.summary();
    let base = s
        .git
        .base_sha
        .clone()
        .expect("the run started from a commit");
    let review = &sent[1];
    assert!(
        review.starts_with("Review this delivery as the maintainer who must merge it"),
        "{review}"
    );
    assert!(
        review.contains(&format!("the commit you started from: {base}.")),
        "{review}"
    );
    assert!(!review.contains("{{"), "every token is replaced: {review}");
    assert!(
        review.contains("- Changed files: 2 (src/calc.py, tests/test_calc.py)."),
        "{review}"
    );
    assert!(
        review.contains("- New top-level folders: none."),
        "{review}"
    );
    assert!(
        review.contains("- Files under .ham/ (records the harness reads; keep them): none."),
        "{review}"
    );
    assert!(review.contains("- Untracked files: 0."), "{review}");

    // The label is the one it always was, and the message is the round's.
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(
        s.cline_message.as_deref(),
        Some("DONE: everything is finished.")
    );
    assert_eq!(s.git.files_changed, 2);
    s.validate().unwrap();
    let r = s.review.expect("the summary records the round");
    assert_eq!(r.status, REVIEW_RAN);
    assert_eq!(r.result.as_deref(), Some(REVIEW_COMPLETED));
    assert_eq!(r.reason, None);
    // The fake engine commits nothing, so HEAD is the base before and after, and the round changed no file.
    assert_eq!(r.commit_before.as_deref(), Some(base.as_str()));
    assert_eq!(r.commit_after.as_deref(), Some(base.as_str()));
    assert_eq!(r.changed_files, Some(false));
    assert!(r.seconds.is_some());
    // One turn.started and one turn.ended however many prompts the turn held; the card sees the round's two events.
    assert_eq!(d.events_of(EventType::TurnStarted).len(), 1);
    assert_eq!(d.events_of(EventType::TurnEnded).len(), 1);
    let kinds: Vec<(String, String)> = d
        .events_of(EventType::TurnProgress)
        .iter()
        .filter(|v| v["kind"] == "review-round")
        .map(|v| {
            (
                v["phase"].as_str().unwrap().to_string(),
                v["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(kinds.len(), 2, "{kinds:?}");
    assert_eq!(kinds[0].0, "start");
    assert_eq!(kinds[1].0, "end");
    assert!(kinds[1].1.contains("asked Cline once"), "{kinds:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_can_switch_the_round_off_for_one_run() {
    let d = Direct::start(Opts {
        review: Some(json!({"mode": "off"})),
        ..opts()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received(), vec![PROMPT.to_string()]);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    let r = s.review.expect("the skip is recorded");
    assert_eq!(r.status, REVIEW_SKIPPED);
    assert_eq!(r.reason.as_deref(), Some(REVIEW_SKIP_OFF));
    assert!(
        d.events_of(EventType::TurnProgress)
            .iter()
            .all(|v| v["kind"] != "review-round")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stage_switch_beats_a_request_that_asks_for_the_round() {
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        review: Some(
            serde_json::to_value(ReviewOptions::with_text("Look again {{facts}}", None)).unwrap(),
        ),
        // The shared world has the round off: no `edit_cfg`.
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received().len(), 1);
    let r = d.summary().review.expect("the skip is recorded");
    assert_eq!(r.reason.as_deref(), Some(REVIEW_SKIP_OFF));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_text_of_the_request_is_the_prompt_with_the_tokens_filled_in() {
    let d = Direct::start(Opts {
        review: Some(
            serde_json::to_value(ReviewOptions::with_text(
                "Check {{base}} again.\n{{facts}}\nThat is all.\n",
                Some(300),
            ))
            .unwrap(),
        ),
        ..opts()
    })
    .await;
    d.invoke().await;
    let sent = d.prompts_received();
    assert_eq!(sent.len(), 2, "{sent:?} {:?}", d.summary().review);
    let base = d.summary().git.base_sha.unwrap();
    assert!(
        sent[1].starts_with(&format!("Check {base} again.\n- Changed files: 2")),
        "{}",
        sent[1]
    );
    assert!(sent[1].ends_with("\nThat is all.\n"), "{}", sent[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_the_check_refuses_ends_the_run_before_any_model_call() {
    let mut bad = serde_json::to_value(ReviewOptions::with_text("Look again", None)).unwrap();
    bad["guardS"] = json!(5);
    let d = Direct::start(Opts {
        review: Some(bad),
        ..opts()
    })
    .await;
    d.invoke().await;
    assert!(d.prompts_received().is_empty(), "no model call");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed);
    let detail = s.detail.unwrap_or_default();
    assert!(
        detail.contains("review: guardS") && !detail.contains("Look again"),
        "{detail}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_changed_no_file_records_why_the_round_did_not_run() {
    let d = Direct::start(Opts {
        scenario: "nudge_idle",
        ..opts()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received().len(), 1, "{:?}", d.prompts_received());
    let s = d.summary();
    let r = s.review.expect("the skip is recorded");
    assert_eq!(r.status, REVIEW_SKIPPED);
    assert!(
        matches!(
            r.reason.as_deref(),
            Some("no_changed_file") | Some("not_done")
        ),
        "{r:?}"
    );
}
