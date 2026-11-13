//! The review round on the wire: the `review` member of the start request, its one check, the `review` record of the
//! summary and of the evidence, and the sentence EVIDENCE.md carries. The member is additive: a request that leaves it out is byte for byte
//! the request of protocol 1.43.0, and the request type still refuses every member it does not know.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DEFAULT_REVIEW_PROMPT, DirectReview, DirectSummary, DirectSummaryError, HeadSource,
    REVIEW_CANCELLED, REVIEW_COMPLETED, REVIEW_FAILED, REVIEW_GUARD_S, REVIEW_RAN,
    REVIEW_SKIP_BLOCKED, REVIEW_SKIP_CANCELLED, REVIEW_SKIP_FILES_UNKNOWN, REVIEW_SKIP_FORCED_STOP,
    REVIEW_SKIP_NO_CHANGED_FILE, REVIEW_SKIP_NO_TIME, REVIEW_SKIP_NOT_DONE, REVIEW_SKIP_OFF,
    REVIEW_SKIP_QUESTION, REVIEW_SKIPPED, REVIEW_TOKEN_BASE, REVIEW_TOKEN_FACTS, ReviewMode,
    ReviewOptions, ReviewProblem, banned_claim_word, check_review,
};
use serde_json::{Value, json};

/// A request with no optional member: the serialization of protocol 1.43.0.
const MINIMAL: &str = r#"{"prompt":"p","target":{"mode":"s3-only"},"model":"m","rules":[]}"#;

fn with_member(member: Value) -> Value {
    let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
    v["review"] = member;
    v
}

#[test]
fn the_protocol_version_is_1_44_0_or_later() {
    let mut parts = PROTOCOL_VERSION
        .split('.')
        .map(|p| p.parse::<u32>().unwrap());
    let (major, minor) = (parts.next().unwrap(), parts.next().unwrap());
    assert!(major == 1 && minor >= 44, "{PROTOCOL_VERSION}");
}

#[test]
fn a_request_without_the_member_serializes_as_1_43_0() {
    let r: StartDirectRunRequest = serde_json::from_str(MINIMAL).unwrap();
    assert!(r.review.is_none());
    assert_eq!(serde_json::to_string(&r).unwrap(), MINIMAL);
    assert!(!MINIMAL.contains("review"));
}

#[test]
fn the_member_round_trips_in_camel_case_and_leaves_out_what_is_unset() {
    let off = with_member(json!({"mode": "off"}));
    let r: StartDirectRunRequest = serde_json::from_value(off.clone()).unwrap();
    assert_eq!(r.review, Some(ReviewOptions::off()));
    assert_eq!(serde_json::to_value(&r).unwrap(), off);

    let sent = ReviewOptions::with_text("Look again at {{base}}.\n", Some(900));
    let v = with_member(serde_json::to_value(&sent).unwrap());
    let r: StartDirectRunRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.review.as_ref(), Some(&sent));
    assert_eq!(v["review"]["mode"], "auto");
    assert_eq!(v["review"]["guardS"], 900);
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
}

#[test]
fn an_unknown_member_inside_or_beside_is_refused_and_named() {
    for name in ["reviews", "review_round", "reviewRound"] {
        let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
        v[name] = json!({"mode": "auto"});
        let e = serde_json::from_value::<StartDirectRunRequest>(v).unwrap_err();
        assert!(e.to_string().contains(name), "{name}: {e}");
    }
    let e = serde_json::from_value::<StartDirectRunRequest>(with_member(
        json!({"mode": "auto", "later": 1}),
    ))
    .unwrap_err();
    assert!(e.to_string().contains("later"), "{e}");
    // A mode this build does not know is refused too.
    let e = serde_json::from_value::<StartDirectRunRequest>(with_member(json!({"mode": "always"})))
        .unwrap_err();
    assert!(e.to_string().contains("always"), "{e}");
}

#[test]
fn the_mode_words_are_auto_and_off() {
    assert_eq!(ReviewMode::parse("auto"), Some(ReviewMode::Auto));
    assert_eq!(ReviewMode::parse("off"), Some(ReviewMode::Off));
    for bad in ["", "on", "Auto", "OFF", "always", "auto "] {
        assert_eq!(ReviewMode::parse(bad), None, "{bad:?}");
    }
    assert_eq!(ReviewMode::Auto.word(), "auto");
    assert_eq!(ReviewMode::Off.word(), "off");
}

#[test]
fn check_review_takes_a_good_member_and_names_the_first_problem_without_the_value() {
    assert_eq!(check_review(&ReviewOptions::off()), Ok(()));
    assert_eq!(check_review(&ReviewOptions::with_text("go", None)), Ok(()));
    // The old limit was 8 KiB; the review text has no size of its own.
    let long = "x".repeat(100 * 1024);
    assert_eq!(check_review(&ReviewOptions::with_text(&long, None)), Ok(()));
    for g in [
        REVIEW_GUARD_S.min,
        REVIEW_GUARD_S.default,
        REVIEW_GUARD_S.max,
    ] {
        assert_eq!(
            check_review(&ReviewOptions::with_text("go", Some(g))),
            Ok(())
        );
    }

    assert_eq!(
        check_review(&ReviewOptions::with_text("  \n\t ", None)),
        Err(ReviewProblem::TextEmpty)
    );
    assert_eq!(
        check_review(&ReviewOptions::with_text("a\0b", None)),
        Err(ReviewProblem::TextNul)
    );
    let mut wrong_hash = ReviewOptions::with_text("go", None);
    wrong_hash.sha256 = Some("0".repeat(64));
    assert_eq!(check_review(&wrong_hash), Err(ReviewProblem::Sha));
    let mut hash_alone = ReviewOptions::off();
    hash_alone.sha256 = Some("0".repeat(64));
    assert_eq!(check_review(&hash_alone), Err(ReviewProblem::Sha));
    let mut no_hash = ReviewOptions::with_text("go", None);
    no_hash.sha256 = None;
    assert_eq!(check_review(&no_hash), Err(ReviewProblem::Sha));
    for g in [
        0,
        REVIEW_GUARD_S.min - 1,
        REVIEW_GUARD_S.max + 1,
        123_456,
        u32::MAX,
    ] {
        let mut o = ReviewOptions::off();
        o.guard_s = Some(g);
        let p = check_review(&o).unwrap_err();
        assert_eq!(p, ReviewProblem::Guard);
        // The message gives the range and never the value that broke it (a short value such as 0 can sit inside the range's own digits).
        if g > 99_999 {
            assert!(!p.to_string().contains(&g.to_string()), "{p}");
        }
        assert_eq!(p.key(), "guardS");
        assert_eq!(p.code(), "out_of_range");
    }
    // The first problem in the order text, sha256, guardS; the text is never repeated in a message.
    let mut both = ReviewOptions::with_text("a\0b", Some(1));
    both.sha256 = None;
    assert_eq!(check_review(&both), Err(ReviewProblem::TextNul));
    for p in [
        ReviewProblem::TextEmpty,
        ReviewProblem::TextNul,
        ReviewProblem::Sha,
        ReviewProblem::Guard,
    ] {
        assert!(p.to_string().starts_with("review: "), "{p}");
    }
}

#[test]
fn the_default_prompt_has_both_tokens_the_six_points_and_the_scratch_worktree_rule() {
    assert_eq!(DEFAULT_REVIEW_PROMPT.matches(REVIEW_TOKEN_BASE).count(), 1);
    assert_eq!(DEFAULT_REVIEW_PROMPT.matches(REVIEW_TOKEN_FACTS).count(), 1);
    for n in 1..=6 {
        assert!(
            DEFAULT_REVIEW_PROMPT
                .lines()
                .any(|l| l.starts_with(&format!("{n} "))),
            "point {n}"
        );
    }
    for point in [
        "Scope:",
        "Other users:",
        "Clean base:",
        "Red first:",
        "Claims:",
    ] {
        assert!(DEFAULT_REVIEW_PROMPT.contains(point), "{point}");
    }
    assert!(DEFAULT_REVIEW_PROMPT.contains("never git stash"));
    assert!(DEFAULT_REVIEW_PROMPT.contains("outside the repository"));
    assert!(DEFAULT_REVIEW_PROMPT.contains("git worktree remove"));
    // The harness reads the lifecycle records, and a review must not rewrite history.
    assert!(DEFAULT_REVIEW_PROMPT.contains("Leave the lifecycle records under .ham/ as they are"));
    assert!(DEFAULT_REVIEW_PROMPT.contains("do not rewrite commits that exist"));
    assert!(DEFAULT_REVIEW_PROMPT.contains("any build or setup error"));
    // The default passes the check it is sent through.
    assert_eq!(
        check_review(&ReviewOptions::with_text(DEFAULT_REVIEW_PROMPT, None)),
        Ok(())
    );
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
fn the_record_leaves_out_what_is_unknown_and_round_trips() {
    let skipped = DirectReview::skipped(REVIEW_SKIP_BLOCKED);
    assert_eq!(
        serde_json::to_value(&skipped).unwrap(),
        json!({"status": "skipped", "reason": "blocked"})
    );
    let back: DirectReview = serde_json::from_value(serde_json::to_value(ran()).unwrap()).unwrap();
    assert_eq!(back, ran());
    let v = serde_json::to_value(ran()).unwrap();
    for key in [
        "status",
        "commitBefore",
        "commitAfter",
        "changedFiles",
        "seconds",
        "inputTokens",
        "outputTokens",
        "result",
    ] {
        assert!(v.get(key).is_some(), "{key}: {v}");
    }
    assert!(v.get("reason").is_none());
}

#[test]
fn is_valid_takes_plain_words_and_full_commit_ids_only() {
    assert!(ran().is_valid());
    assert!(DirectReview::skipped(REVIEW_SKIP_NO_TIME).is_valid());
    let mut sha256_ids = ran();
    sha256_ids.commit_before = Some("c".repeat(64));
    assert!(sha256_ids.is_valid());
    for bad in ["", "Ran", "ran now", "ran;rm", &"x".repeat(33)] {
        let mut r = ran();
        r.status = bad.to_string();
        assert!(!r.is_valid(), "status {bad:?}");
        let mut s = DirectReview::skipped("off");
        s.reason = Some(bad.to_string());
        assert!(!s.is_valid(), "reason {bad:?}");
        let mut t = ran();
        t.result = Some(bad.to_string());
        assert!(!t.is_valid(), "result {bad:?}");
    }
    for bad in ["abc", &"A".repeat(40), &"g".repeat(40), &"a".repeat(41)] {
        let mut r = ran();
        r.commit_after = Some(bad.to_string());
        assert!(!r.is_valid(), "commit {bad:?}");
    }
}

#[test]
fn the_sentence_says_what_happened_and_claims_nothing_about_the_work() {
    for reason in [
        REVIEW_SKIP_OFF,
        REVIEW_SKIP_FORCED_STOP,
        REVIEW_SKIP_CANCELLED,
        REVIEW_SKIP_NOT_DONE,
        REVIEW_SKIP_BLOCKED,
        REVIEW_SKIP_QUESTION,
        REVIEW_SKIP_NO_CHANGED_FILE,
        REVIEW_SKIP_FILES_UNKNOWN,
        REVIEW_SKIP_NO_TIME,
        "a_reason_of_a_later_build",
    ] {
        let s = DirectReview::skipped(reason).sentence();
        assert!(
            s.starts_with("The harness did not ask Cline to review"),
            "{s}"
        );
        assert!(banned_claim_word(&s).is_none(), "{s}");
        assert!(!s.contains('\n'), "{s}");
    }
    let s = ran().sentence();
    assert!(s.contains("asked Cline once"), "{s}");
    assert!(s.contains("312 s"), "{s}");
    assert!(s.contains("4321 input and 876 output tokens"), "{s}");
    assert!(s.contains("changed files"), "{s}");
    assert!(s.contains("aaaaaaaaaaaa to bbbbbbbbbbbb"), "{s}");
    assert!(
        s.ends_with("The harness made no judgment of the result."),
        "{s}"
    );

    let mut same = ran();
    same.commit_after = same.commit_before.clone();
    same.changed_files = Some(false);
    let s = same.sentence();
    assert!(s.contains("changed no file"), "{s}");
    assert!(s.contains("stayed aaaaaaaaaaaa"), "{s}");

    let mut failed = ran();
    failed.result = Some(REVIEW_FAILED.into());
    assert!(
        failed
            .sentence()
            .contains("the outcome of the run is the one the turn before it had")
    );
    // A failed round that had changed files says that the last message is older than the tree.
    assert!(failed.sentence().contains(
        "The last message of the run is from before the round, and the round had changed files."
    ));
    let mut quiet = failed.clone();
    quiet.changed_files = Some(false);
    assert!(!quiet.sentence().contains("from before the round"));
    let mut cancelled = ran();
    cancelled.result = Some(REVIEW_CANCELLED.into());
    assert!(cancelled.sentence().contains("You cancelled the run"));

    // Every sentence is free of the words no text about the work may use.
    for r in [ran(), same, failed, cancelled] {
        assert_eq!(banned_claim_word(&r.sentence()), None);
    }
    // A record with nothing but a status still reads.
    let bare = DirectReview {
        status: REVIEW_RAN.into(),
        reason: None,
        commit_before: None,
        commit_after: None,
        changed_files: None,
        seconds: None,
        input_tokens: None,
        output_tokens: None,
        result: None,
    };
    assert!(bare.sentence().starts_with("The harness asked Cline once"));
    assert_eq!(REVIEW_SKIPPED, "skipped");
}

#[test]
fn the_error_word_of_a_bad_summary_record_is_in_the_summary_error() {
    // The summary check names the member; the message holds plain words only.
    let text = DirectSummaryError::ReviewInvalid.to_string();
    assert!(text.contains("review"), "{text}");
}

/// The summary of a run that failed in its setup: the smallest valid one.
fn bare_summary() -> Value {
    json!({
        "schemaVersion": 1, "runId": "01M4SXQGW2N5315QY99Y4XWXX0", "agentId": "direct", "turn": 1,
        "outcome": "stopped_setup_failed", "label": "Cline did not start",
        "detail": "request.json sha256 differs", "stopReason": null, "turnOutcome": "failed",
        "clineMessage": null, "error": "request.json sha256 differs",
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1"},
        "engine": {"name": "cline"},
        "rules": [], "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 0},
        "nudge": {"requested": false, "honored": false},
        "usage": null, "toolCalls": 0,
        "timing": {"startedAt": "2026-10-25T18:00:00Z", "endedAt": "2026-10-25T18:00:01Z", "wallS": 1},
        "git": {"committedByHarness": false, "filesChanged": 0},
        "snapshots": {"count": 0},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.44.0"}
    })
}

#[test]
fn a_summary_leaves_the_record_out_when_the_run_had_none_and_reads_it_back_when_it_had() {
    let none: DirectSummary = serde_json::from_value(bare_summary()).unwrap();
    assert!(none.review.is_none());
    assert!(serde_json::to_value(&none).unwrap().get("review").is_none());
    assert_eq!(none.validate(), Ok(()));
    assert!(none.to_evidence(HeadSource::Final).review.is_none());

    let mut v = bare_summary();
    v["review"] = serde_json::to_value(ran()).unwrap();
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(s.review, Some(ran()));
    assert_eq!(s.validate(), Ok(()));
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    // The evidence carries the same record, and the outcome and label are the summary's own.
    let e = s.to_evidence(HeadSource::Final);
    assert_eq!(e.review, Some(ran()));
    assert_eq!(e.label, "Cline did not start");
    let ev = serde_json::to_value(&e).unwrap();
    assert_eq!(ev["review"]["status"], "ran");
}

#[test]
fn a_summary_with_a_malformed_record_is_refused_by_validate() {
    for (key, bad) in [
        ("status", json!("Ran now")),
        ("reason", json!("x".repeat(40))),
        ("commitBefore", json!("not-a-commit")),
        ("result", json!("")),
    ] {
        let mut v = bare_summary();
        let mut rec = serde_json::to_value(ran()).unwrap();
        rec[key] = bad.clone();
        v["review"] = rec;
        let s: DirectSummary = serde_json::from_value(v).unwrap();
        assert_eq!(
            s.validate(),
            Err(DirectSummaryError::ReviewInvalid),
            "{key}"
        );
    }
}

#[test]
fn a_reader_ignores_a_member_a_later_record_adds() {
    let mut v = bare_summary();
    let mut rec = serde_json::to_value(ran()).unwrap();
    rec["somethingNew"] = json!(1);
    v["review"] = rec;
    let s: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(s.review, Some(ran()));
    assert_eq!(s.validate(), Ok(()));
}
