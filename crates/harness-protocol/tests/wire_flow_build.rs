//! The flow a run declared and the session's build settings on the wire.
//!
//! - `DirectFlow`: `quick`, `full` or `none`, read from the `Flow:` trailer of the run's final commit message, else from the first line of the
//!   engine's last message. It is a fact recorded beside the label (`DirectSummary.flow`, `DirectEvidence.flow`); no label, check or outcome
//!   reads it.
//! - `StartDirectRunRequest.build` (`BuildOptions`) and `GoalOptions.outsideCheckoutNote`: switches that let a user turn off four defaults of the
//!   session. Every member is additive: a request that leaves them out is byte for byte the request of protocol 1.45.0.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    BuildOptions, DirectFlow, DirectSummary, GoalOptions, HeadSource, check_goal,
};
use serde_json::{Value, json};

/// A request with no optional member: the serialization of protocol 1.45.0.
const MINIMAL: &str = r#"{"prompt":"p","target":{"mode":"s3-only"},"model":"m","rules":[]}"#;

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
        "timing": {"startedAt": "2026-10-27T18:00:00Z", "endedAt": "2026-10-27T18:00:01Z", "wallS": 1},
        "git": {"committedByHarness": false, "filesChanged": 0},
        "snapshots": {"count": 0},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.48.0"}
    })
}

#[test]
fn the_protocol_version_is_1_48_0_or_later() {
    let mut parts = PROTOCOL_VERSION
        .split('.')
        .map(|p| p.parse::<u32>().unwrap());
    let (major, minor) = (parts.next().unwrap(), parts.next().unwrap());
    assert!(major == 1 && minor >= 48, "{PROTOCOL_VERSION}");
}

// ---- the flow words --------------------------------------------------------------------------------------------------

#[test]
fn the_three_words_round_trip_and_nothing_else_is_a_flow() {
    for (flow, word) in [
        (DirectFlow::Quick, "quick"),
        (DirectFlow::Full, "full"),
        (DirectFlow::Undeclared, "none"),
    ] {
        assert_eq!(flow.word(), word);
        assert_eq!(DirectFlow::parse(word), Some(flow));
        assert_eq!(serde_json::to_value(flow).unwrap(), json!(word));
        assert_eq!(
            serde_json::from_value::<DirectFlow>(json!(word)).unwrap(),
            flow
        );
    }
    for bad in ["", "Quick", "FULL", "light", "auto", "quick ", " none"] {
        assert_eq!(DirectFlow::parse(bad), None, "{bad:?}");
        // A reader is lenient (a later writer may add a word): a word this build does not know reads as `none`, and the summary around it
        // still parses.
        assert_eq!(
            serde_json::from_value::<DirectFlow>(json!(bad)).unwrap(),
            DirectFlow::Undeclared,
            "{bad:?}"
        );
    }
}

// ---- the trailer of the final commit message --------------------------------------------------------------------------

#[test]
fn a_flow_trailer_after_a_blank_line_is_read() {
    let m = "Fix the parser\n\nThe parser dropped the last token.\n\nFlow: quick";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Quick));
    let m = "Add the exporter\n\nBody.\n\nFlow: full\n";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Full));
    // The key and the word are read without regard to case, as Git reads a trailer key.
    let m = "Fix\n\nBody.\n\nflow: Quick";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Quick));
    // Windows line ends.
    let m = "Fix\r\n\r\nBody.\r\n\r\nFlow: full\r\n";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Full));
}

#[test]
fn a_flow_trailer_under_another_trailer_is_read_and_the_last_flow_line_wins() {
    let m = "Fix\n\nBody.\n\nTest: python3 -m unittest pkg.test_mod (3 tests)\nFlow: quick";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Quick));
    let m = "Fix\n\nBody.\n\nFlow: quick\nFlow: full";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Full));
    // The harness's own trailers sit in the same paragraph.
    let m = "Fix\n\nBody.\n\nFlow: full\nHam-Turn: 1";
    assert_eq!(DirectFlow::from_commit_message(m), Some(DirectFlow::Full));
}

#[test]
fn a_line_that_git_would_not_read_as_a_trailer_is_not_a_flow() {
    for m in [
        // No message, a subject alone, and the line as the subject.
        "",
        "Fix the parser",
        "Flow: quick",
        // Under a sentence in the same paragraph: not a trailer block.
        "Fix\n\nThe fix is small.\nFlow: quick",
        // Followed by prose.
        "Fix\n\nFlow: quick\n\nThat was the path I took.",
        // Not one of the two words.
        "Fix\n\nBody.\n\nFlow: light",
        "Fix\n\nBody.\n\nFlow: quick path",
        "Fix\n\nBody.\n\nFlow:",
        // Quoted, indented or prefixed.
        "Fix\n\nBody.\n\n\"Flow: quick\"",
        "Fix\n\nBody.\n\n- Flow: quick",
        // Another key that merely holds the word.
        "Fix\n\nBody.\n\nWorkflow: quick",
    ] {
        assert_eq!(DirectFlow::from_commit_message(m), None, "{m:?}");
    }
}

// ---- the first line of the last message -------------------------------------------------------------------------------

#[test]
fn the_first_line_of_the_last_message_can_declare_the_flow() {
    for (m, want) in [
        ("Flow: quick\n\nDone.", Some(DirectFlow::Quick)),
        ("  Flow: full\nrest", Some(DirectFlow::Full)),
        ("\n\nFlow: quick", Some(DirectFlow::Quick)),
        ("`Flow: quick`\nrest", Some(DirectFlow::Quick)),
        ("**Flow: full**\nrest", Some(DirectFlow::Full)),
        ("flow: Full", Some(DirectFlow::Full)),
        ("", None),
        ("Done. Flow: quick", None),
        ("I fixed it.\nFlow: quick", None),
        ("Flow: quicker", None),
        ("Flow:", None),
    ] {
        assert_eq!(DirectFlow::from_last_message(m), want, "{m:?}");
    }
}

#[test]
fn the_commit_trailer_wins_and_a_run_that_declared_nothing_is_none() {
    let trailer = "Fix\n\nBody.\n\nFlow: full";
    assert_eq!(
        DirectFlow::read(Some(trailer), Some("Flow: quick\nDone")),
        DirectFlow::Full
    );
    assert_eq!(
        DirectFlow::read(Some("Fix the parser"), Some("Flow: quick\nDone")),
        DirectFlow::Quick
    );
    assert_eq!(DirectFlow::read(None, Some("Flow: full")), DirectFlow::Full);
    assert_eq!(DirectFlow::read(Some(trailer), None), DirectFlow::Full);
    assert_eq!(
        DirectFlow::read(Some("Fix the parser"), Some("All done.")),
        DirectFlow::Undeclared
    );
    assert_eq!(DirectFlow::read(None, None), DirectFlow::Undeclared);
}

// ---- the summary and the evidence ---------------------------------------------------------------------------------------

#[test]
fn a_summary_without_the_flow_has_the_bytes_it_had_before() {
    let s: DirectSummary = serde_json::from_value(bare_summary()).unwrap();
    assert_eq!(s.flow, None);
    assert!(serde_json::to_value(&s).unwrap().get("flow").is_none());
    assert_eq!(s.validate(), Ok(()));
    let e = s.to_evidence(HeadSource::Final);
    assert_eq!(e.flow, None);
    assert!(serde_json::to_value(&e).unwrap().get("flow").is_none());
}

#[test]
fn the_flow_rides_in_the_summary_and_in_the_evidence_and_changes_nothing_else() {
    for (word, flow) in [
        ("quick", DirectFlow::Quick),
        ("full", DirectFlow::Full),
        ("none", DirectFlow::Undeclared),
    ] {
        let mut v = bare_summary();
        v["flow"] = json!(word);
        let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(s.flow, Some(flow));
        assert_eq!(s.validate(), Ok(()));
        assert_eq!(serde_json::to_value(&s).unwrap(), v);
        let e = s.to_evidence(HeadSource::Final);
        assert_eq!(e.flow, Some(flow));
        assert_eq!(serde_json::to_value(&e).unwrap()["flow"], json!(word));
        // The outcome and the label are the summary's own, whatever the flow says.
        let plain: DirectSummary = serde_json::from_value(bare_summary()).unwrap();
        assert_eq!(e.outcome, plain.to_evidence(HeadSource::Final).outcome);
        assert_eq!(e.label, plain.to_evidence(HeadSource::Final).label);
    }
}

#[test]
fn a_summary_with_a_word_this_build_does_not_know_still_parses_and_reads_it_as_none() {
    let mut v = bare_summary();
    v["flow"] = json!("light");
    let s: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(s.flow, Some(DirectFlow::Undeclared));
    assert_eq!(s.validate(), Ok(()));
}

// ---- the build member -----------------------------------------------------------------------------------------------------

#[test]
fn a_request_without_the_build_member_serializes_as_1_45_0() {
    let r: StartDirectRunRequest = serde_json::from_str(MINIMAL).unwrap();
    assert!(r.build.is_none());
    assert_eq!(serde_json::to_string(&r).unwrap(), MINIMAL);
    assert!(!MINIMAL.contains("build"));
}

#[test]
fn the_build_member_round_trips_in_camel_case_and_leaves_out_what_is_unset() {
    let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
    v["build"] = json!({"cargoNoDebugInfo": false, "excludeTargetDir": false});
    let r: StartDirectRunRequest = serde_json::from_value(v.clone()).unwrap();
    let b = r.build.unwrap();
    assert_eq!(b.cargo_no_debug_info, Some(false));
    assert_eq!(b.cargo_no_incremental, None);
    assert_eq!(b.exclude_target_dir, Some(false));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
}

#[test]
fn the_build_defaults_are_on_for_an_absent_member_and_an_unset_member_and_true() {
    let none = BuildOptions::default();
    assert!(none.is_empty());
    assert!(none.cargo_no_debug_info() && none.cargo_no_incremental() && none.exclude_target_dir());
    let off = BuildOptions {
        cargo_no_debug_info: Some(false),
        cargo_no_incremental: Some(false),
        exclude_target_dir: Some(false),
    };
    assert!(!off.is_empty());
    assert!(!off.cargo_no_debug_info() && !off.cargo_no_incremental() && !off.exclude_target_dir());
    let on = BuildOptions {
        cargo_no_debug_info: Some(true),
        ..BuildOptions::default()
    };
    assert!(on.cargo_no_debug_info() && !on.is_empty());
}

#[test]
fn an_unknown_member_inside_or_beside_the_build_member_is_refused_and_named() {
    let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
    v["builds"] = json!({});
    let e = serde_json::from_value::<StartDirectRunRequest>(v).unwrap_err();
    assert!(e.to_string().contains("builds"), "{e}");
    let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
    v["build"] = json!({"cargoJobs": 4});
    let e = serde_json::from_value::<StartDirectRunRequest>(v).unwrap_err();
    assert!(e.to_string().contains("cargoJobs"), "{e}");
    let mut v: Value = serde_json::from_str(MINIMAL).unwrap();
    v["build"] = json!({"cargoNoIncremental": "off"});
    assert!(serde_json::from_value::<StartDirectRunRequest>(v).is_err());
}

// ---- the goal member's sentence switch ------------------------------------------------------------------------------------

#[test]
fn the_goal_member_carries_the_sentence_switch_and_a_plain_switch_does_not() {
    let plain = serde_json::to_value(GoalOptions::switch(true)).unwrap();
    assert_eq!(plain, json!({"enabled": true}));
    assert!(!GoalOptions::switch(true).has_limits());
    let g = GoalOptions {
        outside_checkout_note: Some(false),
        ..GoalOptions::switch(true)
    };
    assert!(g.has_limits());
    assert_eq!(
        serde_json::to_value(g).unwrap(),
        json!({"enabled": true, "outsideCheckoutNote": false})
    );
    assert_eq!(check_goal(&g), Ok(()));
    let back: GoalOptions =
        serde_json::from_value(json!({"enabled": true, "outsideCheckoutNote": false})).unwrap();
    assert_eq!(back.outside_checkout_note, Some(false));
}
