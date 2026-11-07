//! `summary.json`: built from the classification and the facts of the session, always valid
//! against the frozen rules of `DirectSummary::validate`, and honest about what was not measured (`usage` is null when the
//! engine reported none, never zeros).
#![allow(clippy::unwrap_used)]

use agentd::acp::Watched;
use agentd::direct::classify::{PromptDone, RawEnd, classify, upload_conflict};
use agentd::direct::summary::{
    InputNote, SummaryExtras, SummaryInputs, UsageSum, build_summary, build_summary_with,
    input_sentence, nudge_note, reclassify,
};
use harness_protocol::common::{Protocol, Usage};
use harness_protocol::direct::{
    DirectAttempt, DirectAttemptKind, DirectAttemptResult, DirectFreshSession, DirectGit,
    DirectIntervention, DirectModelSpec, DirectNudge, DirectOutcome, DirectPackages,
    DirectRulesPreflight, DirectSnapshots, DirectSwitch, DirectSwitchKind, DirectTiming,
    InstructAction, InstructOrigin, MAX_CLINE_MESSAGE_CHARS, MAX_DIRECT_DETAIL_CHARS,
    MAX_DIRECT_SUMMARY_BYTES, banned_claim_word,
};
use harness_protocol::runtime::TurnOutcome;

fn model() -> DirectModelSpec {
    DirectModelSpec {
        id: "us.anthropic.claude-opus-5-5".into(),
        region: "us-east-1".into(),
        protocol: Protocol::Converse,
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
        default_reasoning_effort: None,
    }
}

fn inputs(raw: &RawEnd) -> SummaryInputs {
    SummaryInputs {
        run_id: "01M4XABCDEFGHJKMNPQRSTVWXY".into(),
        classified: classify(raw),
        stop_reason: agentd::direct::classify::stop_reason(raw),
        message: Some(raw.message.clone()),
        model: model(),
        pinned: Some("us.anthropic.claude-opus-5-5".into()),
        engine_name: "cline".into(),
        engine_version: Some("3.0.71".into()),
        engine_binary_sha256: Some("ab".repeat(32)),
        engine_identity: Some("nobody".to_string()),
        rules: vec![],
        rules_preflight: DirectRulesPreflight {
            ran: false,
            ms: None,
        },
        request_sha256: "cd".repeat(32),
        prompt_bytes: 47,
        nudge: DirectNudge {
            requested: false,
            honored: false,
        },
        usage: None,
        tool_calls: 41,
        timing: DirectTiming {
            started_at: "2026-10-13T18:00:00Z".into(),
            ended_at: "2026-10-13T19:02:11Z".into(),
            wall_s: 3731,
            prompt_s: Some(3690),
            max_silent_gap_s: Some(84),
        },
        git: DirectGit {
            base_sha: Some("1".repeat(40)),
            head_sha: Some("2".repeat(40)),
            committed_by_harness: true,
            files_changed: 12,
            bundle_uri: Some("s3://b/runs/r/agents/direct/turns/1/bundle".into()),
            bundle_bytes: Some(123_456),
        },
        snapshots: DirectSnapshots::default(),
        packages: DirectPackages::default(),
        error: None,
        attempts: vec![],
        switches: vec![],
        final_model: None,
        failover: None,
        outage_s: None,
        reasoning_effort: None,
        reasoning_source: None,
    }
}

fn done_raw(message: &str) -> RawEnd {
    RawEnd {
        watched: Watched::Done(Ok(PromptDone {
            stop_reason: "end_turn".into(),
            usage: None,
        })),
        cancel_sent: false,
        message: message.into(),
        setup_error: None,
    }
}

#[test]
fn a_reported_done_summary_has_the_frozen_label_and_validates() {
    let s = build_summary(inputs(&done_raw("Added percent_change.")));
    s.validate().unwrap();
    assert_eq!(s.schema_version, 1);
    assert_eq!((s.agent_id.as_str(), s.turn), ("direct", 1));
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(s.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(s.cline_message.as_deref(), Some("Added percent_change."));
    assert_eq!(s.versions.agentd, env!("CARGO_PKG_VERSION"));
    assert_eq!(s.versions.protocol, harness_protocol::PROTOCOL_VERSION);
}

#[test]
fn the_summary_says_who_the_engine_ran_as_and_leaves_it_out_when_the_run_never_got_that_far() {
    let s = build_summary(inputs(&done_raw("Added percent_change.")));
    s.validate().unwrap();
    assert_eq!(s.engine.identity.as_deref(), Some("nobody"));
    let mut i = inputs(&done_raw("x"));
    i.engine_identity = None;
    let s = build_summary(i);
    assert_eq!(s.engine.identity, None);
    assert!(!serde_json::to_string(&s).unwrap().contains("identity"));
}

#[test]
fn usage_is_null_when_the_engine_reported_none_not_zeros() {
    let s = build_summary(inputs(&done_raw("done")));
    assert_eq!(s.usage, None);
    let json = serde_json::to_value(&s).unwrap();
    assert!(json["usage"].is_null());
    // The keys the contract example shows as null are present as null.
    for key in ["detail", "error", "faultHint", "usage"] {
        assert!(
            json.get(key).is_some_and(serde_json::Value::is_null),
            "{key}"
        );
    }
}

#[test]
fn reported_usage_is_kept_as_reported() {
    let mut i = inputs(&done_raw("done"));
    i.usage = Some(Usage {
        input_tokens: 10,
        output_tokens: 20,
        cache_read_input_tokens: 30,
        cache_write_input_tokens: 40,
    });
    let s = build_summary(i);
    assert_eq!(s.usage.unwrap().cache_write_input_tokens, 40);
}

#[test]
fn the_message_keeps_its_last_8000_characters() {
    let long = format!(
        "{}{}",
        "a".repeat(5000),
        "b".repeat(MAX_CLINE_MESSAGE_CHARS)
    );
    let s = build_summary(inputs(&done_raw(&long)));
    let kept = s.cline_message.clone().unwrap();
    assert_eq!(kept.chars().count(), MAX_CLINE_MESSAGE_CHARS);
    assert!(kept.chars().all(|c| c == 'b'));
    s.validate().unwrap();
}

#[test]
fn a_blank_message_is_recorded_as_none() {
    let s = build_summary(inputs(&done_raw("  \n ")));
    assert_eq!(s.outcome, DirectOutcome::StoppedNoMessage);
    assert_eq!(s.cline_message, None);
}

#[test]
fn a_long_detail_and_error_are_cut_to_300_characters() {
    let mut i = inputs(&done_raw("x"));
    i.classified.detail = Some("d".repeat(1000));
    i.error = Some("e".repeat(1000));
    let s = build_summary(i);
    assert_eq!(
        s.detail.as_ref().unwrap().chars().count(),
        MAX_DIRECT_DETAIL_CHARS
    );
    assert_eq!(
        s.error.as_ref().unwrap().chars().count(),
        MAX_DIRECT_DETAIL_CHARS
    );
    s.validate().unwrap();
}

#[test]
fn a_summary_whose_message_is_huge_still_fits_the_size_limit() {
    let wide = "\u{1F600}".repeat(60_000);
    let s = build_summary(inputs(&done_raw(&wide)));
    let bytes = serde_json::to_vec(&s).unwrap();
    assert!(bytes.len() <= MAX_DIRECT_SUMMARY_BYTES, "{}", bytes.len());
    s.validate().unwrap();
}

#[test]
fn a_setup_failure_summary_leaves_the_unmeasured_parts_out() {
    let mut raw = done_raw("");
    raw.setup_error = Some("worktree: git failed".into());
    let mut i = inputs(&raw);
    i.pinned = None;
    i.git.base_sha = None;
    i.git.head_sha = None;
    i.git.bundle_uri = None;
    i.git.bundle_bytes = None;
    i.git.committed_by_harness = false;
    i.git.files_changed = 0;
    i.timing.prompt_s = None;
    i.timing.max_silent_gap_s = None;
    let s = build_summary(i);
    s.validate().unwrap();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed);
    assert_eq!(s.label, "Cline did not start");
    assert_eq!(s.detail.as_deref(), Some("worktree: git failed"));
    let json = serde_json::to_value(&s).unwrap();
    assert!(json["model"].get("pinned").is_none());
    assert!(json["git"].get("headSha").is_none());
    assert!(json["git"].get("bundleBytes").is_none());
    assert!(json["timing"].get("promptS").is_none());
}

#[test]
fn nothing_the_summary_says_claims_the_work_was_checked() {
    for message in ["done", "", "all tests pass"] {
        let s = build_summary(inputs(&done_raw(message)));
        assert_eq!(banned_claim_word(&s.label), None);
        assert_eq!(s.detail.as_deref().and_then(banned_claim_word), None);
    }
}

#[test]
fn the_nudge_note_says_honored_only_when_the_run_asked_for_a_nudge_that_ran() {
    assert_eq!(
        nudge_note(None),
        DirectNudge {
            requested: false,
            honored: false
        }
    );
    // max_nudges 0 turns the nudge off: requested in the file, not honored.
    assert_eq!(
        nudge_note(Some(0)),
        DirectNudge {
            requested: true,
            honored: false
        }
    );
    assert_eq!(
        nudge_note(Some(3)),
        DirectNudge {
            requested: true,
            honored: true
        }
    );
}

#[test]
fn the_summary_round_trips_through_json_unchanged() {
    let s = build_summary(inputs(&done_raw("done")));
    let back: harness_protocol::direct::DirectSummary =
        serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

// ---- a run that waited for the user -------------------------------------------------------

#[test]
fn the_sentences_of_a_run_that_followed_a_wait_are_plain() {
    assert_eq!(
        input_sentence(&InputNote::ChoseFinish),
        "You chose to finish."
    );
    assert_eq!(
        input_sentence(&InputNote::FreshSessionFailed),
        "A fresh session could not start."
    );
}

#[test]
fn nobody_answered_names_the_waited_time_in_whole_minutes_and_at_least_one() {
    let said = |waited_s| input_sentence(&InputNote::NobodyAnswered { waited_s });
    assert_eq!(
        said(1800),
        "Nobody answered in 30 min, so the run finished with what exists."
    );
    // Rounded to the nearest minute, halves up.
    assert_eq!(
        said(1790),
        "Nobody answered in 30 min, so the run finished with what exists."
    );
    assert_eq!(
        said(3600),
        "Nobody answered in 60 min, so the run finished with what exists."
    );
    assert_eq!(
        said(150),
        "Nobody answered in 3 min, so the run finished with what exists."
    );
    assert_eq!(
        said(89),
        "Nobody answered in 1 min, so the run finished with what exists."
    );
    assert_eq!(
        said(30),
        "Nobody answered in 1 min, so the run finished with what exists."
    );
    assert_eq!(
        said(1),
        "Nobody answered in 1 min, so the run finished with what exists."
    );
    assert_eq!(
        said(0),
        "Nobody answered in 1 min, so the run finished with what exists."
    );
}

fn usage(i: u64, o: u64, r: u64, w: u64) -> Usage {
    Usage {
        input_tokens: i,
        output_tokens: o,
        cache_read_input_tokens: r,
        cache_write_input_tokens: w,
    }
}

#[test]
fn usage_is_the_sum_of_every_prompt_that_answered_across_sessions() {
    let mut sum = UsageSum::new();
    assert_eq!(sum.total(), None, "nothing was measured");
    // Prompt 1 of the first session, then prompts 2 and 3 of a fresh session: the engine reports each prompt's own usage.
    sum.add_prompt(1, Some(usage(100, 20, 300, 40)));
    assert_eq!(sum.total(), Some(usage(100, 20, 300, 40)));
    sum.add_prompt(2, Some(usage(1, 2, 3, 4)));
    sum.add_prompt(3, Some(usage(5, 5, 5, 5)));
    assert_eq!(sum.total(), Some(usage(106, 27, 308, 49)));
}

#[test]
fn a_prompt_is_counted_once_even_when_its_late_response_arrives_again() {
    let mut sum = UsageSum::new();
    sum.add_prompt(7, Some(usage(10, 10, 10, 10)));
    sum.add_prompt(7, Some(usage(10, 10, 10, 10)));
    sum.add_prompt(7, Some(usage(99, 99, 99, 99)));
    assert_eq!(sum.total(), Some(usage(10, 10, 10, 10)));
    sum.add_prompt(8, Some(usage(1, 1, 1, 1)));
    assert_eq!(sum.total(), Some(usage(11, 11, 11, 11)));
}

#[test]
fn usage_is_not_measured_when_no_prompt_reported_any_and_never_wraps() {
    let mut sum = UsageSum::new();
    sum.add_prompt(1, None);
    sum.add_prompt(2, None);
    assert_eq!(
        sum.total(),
        None,
        "no zeros stand in for a missing measurement"
    );
    sum.add_prompt(3, Some(usage(u64::MAX, 0, 0, 0)));
    sum.add_prompt(4, Some(usage(1, 0, 0, 0)));
    assert_eq!(
        sum.total(),
        Some(usage(u64::MAX, 0, 0, 0)),
        "a sum that would overflow stops at the largest value"
    );
}

#[test]
fn a_prompt_without_a_report_does_not_hide_the_ones_that_had_one() {
    let mut sum = UsageSum::new();
    sum.add_prompt(1, Some(usage(7, 7, 7, 7)));
    sum.add_prompt(2, None);
    assert_eq!(sum.total(), Some(usage(7, 7, 7, 7)));
}

// ---- interventions, fresh sessions and the lead sentences in summary.json ---------------------------------------------------------

fn intervention(n: u32) -> DirectIntervention {
    DirectIntervention {
        n,
        request_id: format!("01M5A2R7K3C9XJ4D8E6FQ0WT{n:02}"),
        reason: "engine_stop".into(),
        opened_at: "2026-10-15T13:41:07Z".into(),
        closed_at: "2026-10-15T13:42:40Z".into(),
        waited_s: 93,
        outcome: "answered".into(),
        action: Some(InstructAction::NewSessionWithPrompt),
        origin: Some(InstructOrigin::User),
        model: None,
        text_bytes: Some(88),
        at_attempt: 3,
    }
}

fn fresh(n: u32) -> DirectFreshSession {
    DirectFreshSession {
        n,
        trigger: "user".into(),
        reason: "engine_stop".into(),
        request_id: Some("01M5A2R7K3C9XJ4D8E6FQ0WT1N".into()),
        started_at: "2026-10-15T13:42:41Z".into(),
        restarted_process: false,
        digest_bytes: 18_432,
        digest_sha256: "9f".repeat(32),
        at_attempt: 3,
    }
}

#[test]
fn a_run_that_never_waited_has_no_intervention_or_fresh_session_members() {
    let s = build_summary(inputs(&done_raw("done")));
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("interventions"), "{json}");
    assert!(!json.contains("freshSessions"), "{json}");
    assert_eq!(
        s,
        build_summary_with(inputs(&done_raw("done")), SummaryExtras::default())
    );
}

#[test]
fn interventions_and_fresh_sessions_are_listed_under_their_wire_names() {
    let extras = SummaryExtras {
        interventions: vec![intervention(1)],
        fresh_sessions: vec![fresh(2)],
        observations: Vec::new(),
        input_notes: vec![],
    };
    let s = build_summary_with(inputs(&done_raw("done")), extras);
    s.validate().unwrap();
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(
        json["interventions"][0]["requestId"],
        "01M5A2R7K3C9XJ4D8E6FQ0WT01"
    );
    assert_eq!(
        json["interventions"][0]["action"],
        "new_session_with_prompt"
    );
    assert_eq!(json["interventions"][0]["textBytes"], 88);
    assert_eq!(json["freshSessions"][0]["n"], 2);
    assert_eq!(json["freshSessions"][0]["restartedProcess"], false);
    assert_eq!(json["freshSessions"][0]["digestBytes"], 18_432);
    let back: harness_protocol::direct::DirectSummary = serde_json::from_value(json).unwrap();
    assert_eq!(back, s);
}

#[test]
fn only_the_last_32_interventions_and_fresh_sessions_are_listed() {
    let extras = SummaryExtras {
        interventions: (1..=40).map(intervention).collect(),
        fresh_sessions: (2..=40).map(fresh).collect(),
        observations: Vec::new(),
        input_notes: vec![],
    };
    let s = build_summary_with(inputs(&done_raw("done")), extras);
    s.validate().unwrap();
    let ns: Vec<u32> = s.interventions.iter().map(|i| i.n).collect();
    assert_eq!(ns, (9..=40).collect::<Vec<_>>());
    let ns: Vec<u32> = s.fresh_sessions.iter().map(|f| f.n).collect();
    assert_eq!(ns, (9..=40).collect::<Vec<_>>());
}

#[test]
fn the_wait_sentences_follow_the_failover_sentences_and_lead_the_cause() {
    let mut i = inputs(&done_raw(""));
    i.classified = classify(&RawEnd {
        watched: Watched::Done(Ok(PromptDone {
            stop_reason: "end_turn".into(),
            usage: None,
        })),
        cancel_sent: false,
        message: "I cannot do that.".into(),
        setup_error: None,
    });
    i.final_model = Some("global.anthropic.claude-opus-5-5".into());
    i.switches = vec![DirectSwitch {
        n: 1,
        at: "2026-10-15T13:00:00Z".into(),
        from: "us.anthropic.claude-opus-5-5".into(),
        to: "global.anthropic.claude-opus-5-5".into(),
        kind: DirectSwitchKind::Route,
        class: "transient".into(),
        detail: None,
    }];
    i.outage_s = Some(120);
    let cause = i.classified.detail.clone();
    let s = build_summary_with(
        i,
        SummaryExtras {
            input_notes: vec![InputNote::NobodyAnswered { waited_s: 1800 }],
            ..SummaryExtras::default()
        },
    );
    let detail = s.detail.unwrap();
    let lead = "Ran on global.anthropic.claude-opus-5-5 after 1 switch. Every route was down for the last 120 s. Nobody answered in 30 min, so the run finished with what exists.";
    assert!(detail.starts_with(lead), "{detail}");
    if let Some(cause) = cause {
        assert!(detail.ends_with(&cause), "{detail}");
    }
}

#[test]
fn several_wait_sentences_keep_their_order_and_the_detail_stays_within_its_limit() {
    let mut i = inputs(&done_raw("done"));
    i.classified.detail = Some("c".repeat(MAX_DIRECT_DETAIL_CHARS));
    let s = build_summary_with(
        i,
        SummaryExtras {
            input_notes: vec![InputNote::ChoseFinish, InputNote::FreshSessionFailed],
            ..SummaryExtras::default()
        },
    );
    s.validate().unwrap();
    let detail = s.detail.unwrap();
    assert!(
        detail.starts_with("You chose to finish. A fresh session could not start. "),
        "{detail}"
    );
    assert!(detail.chars().count() <= MAX_DIRECT_DETAIL_CHARS);
}

#[test]
fn the_new_attempt_kinds_are_listed_under_their_wire_words() {
    let attempt = |n: u32, kind: DirectAttemptKind| DirectAttempt {
        n,
        kind,
        started_at: "2026-10-15T13:00:00Z".into(),
        ended_at: "2026-10-15T13:01:00Z".into(),
        result: DirectAttemptResult::Ok,
        retry_class: None,
        detail: None,
        backoff_s: None,
        model_id: None,
    };
    let mut i = inputs(&done_raw("done"));
    i.attempts = vec![
        attempt(1, DirectAttemptKind::Prompt),
        attempt(2, DirectAttemptKind::Instruction),
        attempt(3, DirectAttemptKind::FreshSession),
    ];
    let s = build_summary(i);
    s.validate().unwrap();
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(json["attempts"][1]["kind"], "instruction");
    assert_eq!(json["attempts"][2]["kind"], "fresh_session");
}

/// agentd writes its own `summary.json` over a foreign one after the end sequence found the conflict. That summary must say so: the
/// outcome, the telemetry outcome, the detail, the label (which `validate` ties to the detail), the error; every other member stays.
#[test]
fn a_summary_reclassified_as_an_upload_conflict_names_the_object_and_still_validates() {
    let mut s = build_summary(inputs(&done_raw("Added percent_change.")));
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    let c = upload_conflict("turns/1/summary.json");
    let error = "upload: the object turns/1/summary.json already existed before agentd wrote it";
    reclassify(&mut s, &c, Some(error.to_string()));
    s.validate().unwrap();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther);
    assert_eq!(s.turn_outcome, TurnOutcome::Failed);
    assert_eq!(
        s.label,
        "Cline stopped: the harness ended the run (the object turns/1/summary.json already existed before agentd wrote it)"
    );
    assert_eq!(s.detail, c.detail);
    assert_eq!(s.error.as_deref(), Some(error));
    assert_eq!(s.fault_hint, None);
    assert_eq!(banned_claim_word(&s.label), None);
    assert_eq!(
        s.git.head_sha,
        Some("2".repeat(40)),
        "the delivered head stays"
    );
    assert_eq!(s.cline_message.as_deref(), Some("Added percent_change."));
    assert_eq!(s.tool_calls, 41);
}

#[test]
fn reclassify_cuts_a_long_error_to_the_limit_so_the_summary_stays_valid() {
    let mut s = build_summary(inputs(&done_raw("done")));
    let c = upload_conflict("turns/1/bundle");
    reclassify(&mut s, &c, Some("e".repeat(MAX_DIRECT_DETAIL_CHARS * 2)));
    s.validate().unwrap();
    assert_eq!(s.error.unwrap().chars().count(), MAX_DIRECT_DETAIL_CHARS);
}

#[test]
fn b776_summary_json_keeps_a_message_of_150_thousand_characters_whole() {
    let long: String = (0..5_000)
        .map(|i| format!("line {i:05} of the final report\n"))
        .collect();
    assert!(long.chars().count() > 150_000, "{}", long.chars().count());
    let s = build_summary(inputs(&done_raw(&long)));
    assert_eq!(
        s.cline_message.as_deref(),
        Some(long.as_str()),
        "the message is whole"
    );
    s.validate().unwrap();
    let bytes = serde_json::to_vec(&s).unwrap();
    assert!(bytes.len() <= MAX_DIRECT_SUMMARY_BYTES, "{}", bytes.len());
}

#[test]
fn b776_a_message_over_the_guard_keeps_its_end() {
    let long = format!(
        "{}{}",
        "a".repeat(10_000),
        "b".repeat(MAX_CLINE_MESSAGE_CHARS)
    );
    let s = build_summary(inputs(&done_raw(&long)));
    let kept = s.cline_message.clone().unwrap();
    assert_eq!(kept.chars().count(), MAX_CLINE_MESSAGE_CHARS);
    assert!(kept.chars().all(|c| c == 'b'));
    s.validate().unwrap();
}

#[test]
fn b776_a_detail_and_an_error_keep_a_thousand_characters() {
    let mut i = inputs(&done_raw("x"));
    i.classified.detail = Some("d".repeat(1500));
    i.error = Some("e".repeat(1500));
    let s = build_summary(i);
    assert_eq!(s.detail.as_ref().unwrap().chars().count(), 1_000);
    assert_eq!(s.error.as_ref().unwrap().chars().count(), 1_000);
    s.validate().unwrap();
}
