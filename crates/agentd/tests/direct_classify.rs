//! The stop classifier of the direct driver: one test per row of the table. The classifier is
//! pure: ACP's raw end of the one prompt goes in, the outcome, its frozen label and the telemetry outcome come out. Only
//! row 2 says "Cline reported done"; no row claims the harness checked anything.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpError, Watched};
use agentd::direct::classify::{
    Classified, PromptDone, RawEnd, TreeFacts, classify, clean_message, not_replaced_text,
    redact_known, refine, refine_engine_stop, repository_not_trusted, stop_reason,
    summary_not_written, upload_conflict, upload_conflict_payload,
};
use agentd::direct::engine_stop::{AttemptFacts, StopReport};
use harness_protocol::api::internal::FaultTransport;
use harness_protocol::direct::{
    BANNED_CLAIM_WORDS, DirectOutcome, HARNESS_STOP_PREFIX, MAX_DIRECT_DETAIL_CHARS,
    banned_claim_word,
};
use harness_protocol::runtime::TurnOutcome;
use std::time::Duration;

fn done(reason: &str) -> Watched<Result<PromptDone, AcpError>> {
    Watched::Done(Ok(PromptDone {
        stop_reason: reason.into(),
        usage: None,
    }))
}

fn raw(watched: Watched<Result<PromptDone, AcpError>>, cancel_sent: bool, message: &str) -> RawEnd {
    RawEnd {
        watched,
        cancel_sent,
        message: message.into(),
        setup_error: None,
    }
}

// ---- row 1: the engine never got to the prompt --------------------------------------------------------------------------

#[test]
fn row_1_a_setup_error_is_cline_did_not_start() {
    let mut r = raw(done("end_turn"), false, "text");
    r.setup_error = Some("rules preflight: 00-my-rules.md is missing".into());
    let c = classify(&r);
    assert_eq!(c.outcome, DirectOutcome::StoppedSetupFailed);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert_eq!(
        c.detail.as_deref(),
        Some("rules preflight: 00-my-rules.md is missing")
    );
    assert_eq!(c.outcome.label(), "Cline did not start");
}

#[test]
fn row_1_a_json_rpc_error_on_session_new_is_a_setup_failure_not_a_provider_failure() {
    let mut r = raw(
        Watched::Done(Err(AcpError::Rpc {
            code: -32602,
            message: "bad cwd".into(),
            data: None,
        })),
        false,
        "",
    );
    r.setup_error = Some("session/new: bad cwd".into());
    let c = classify(&r);
    assert_eq!(c.outcome, DirectOutcome::StoppedSetupFailed);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
}

#[test]
fn row_1_a_model_mismatch_is_a_setup_failure() {
    let r = raw(
        Watched::Done(Err(AcpError::ModelMismatch {
            wanted: "us.anthropic.claude-opus-5-5".into(),
            got: Some("other".into()),
        })),
        false,
        "",
    );
    let c = classify(&r);
    assert_eq!(c.outcome, DirectOutcome::StoppedSetupFailed);
    assert!(c.detail.unwrap().contains("us.anthropic.claude-opus-5-5"));
}

// ---- rows 2 and 3: end_turn ----------------------------------------------------------------------------------------------

#[test]
fn row_2_end_turn_with_a_message_is_the_one_reported_done() {
    let c = classify(&raw(done("end_turn"), false, "Added percent_change."));
    assert_eq!(c.outcome, DirectOutcome::ReportedDone);
    assert_eq!(c.turn_outcome, TurnOutcome::Completed);
    assert_eq!(c.detail, None);
    assert_eq!(c.fault_hint, None);
    assert_eq!(c.outcome.label(), "Cline reported done");
}

#[test]
fn row_2_only_end_turn_with_a_message_says_reported_done() {
    for other in [
        done("max_tokens"),
        done("max_turn_requests"),
        done("refusal"),
        done("cancelled"),
        done("something_new"),
        Watched::TimedOut,
        Watched::Stalled {
            silent: Duration::from_secs(900),
        },
        Watched::Done(Err(AcpError::Closed)),
    ] {
        assert_ne!(
            classify(&raw(other, false, "a message")).outcome,
            DirectOutcome::ReportedDone
        );
    }
}

#[test]
fn row_3_end_turn_with_a_blank_message_is_stopped_no_message() {
    for message in ["", "   ", "\n\t \n"] {
        let c = classify(&raw(done("end_turn"), false, message));
        assert_eq!(c.outcome, DirectOutcome::StoppedNoMessage, "{message:?}");
        assert_eq!(c.turn_outcome, TurnOutcome::Completed);
        assert_eq!(
            c.outcome.label(),
            "Cline stopped: it ended its turn without a message"
        );
    }
}

// ---- row 16: an end_turn with a message, and a tree that equals the base --------------------------------------------------------

fn gave_up_message() -> RawEnd {
    raw(
        done("end_turn"),
        false,
        "I cannot do this: the task contradicts the module's tests. I changed nothing.",
    )
}

fn tree(files_changed: Option<u32>) -> TreeFacts {
    TreeFacts { files_changed }
}

/// The harness cannot tell a finished task from a give-up by reading the message, and it never reads it. It can count: when the
/// delivered tree is the base tree, "Cline reported done" would claim a completion that left no trace, so the stop is named by that fact.
#[test]
fn row_16_an_end_turn_with_a_message_and_zero_changed_files_is_stopped_no_changes() {
    let c = refine(classify(&gave_up_message()), tree(Some(0)));
    assert_eq!(c.outcome, DirectOutcome::StoppedNoChanges);
    assert_eq!(c.outcome.as_str(), "stopped_no_changes");
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: it ended its turn without changing a file"
    );
    assert!(!c.outcome.is_reported_done());
    // The engine finished its prompt normally: telemetry and the fault channel do not change.
    assert_eq!(c.turn_outcome, TurnOutcome::Completed);
    assert_eq!(c.detail, None);
    assert_eq!(c.fault_hint, None);
}

#[test]
fn row_2_stays_cline_reported_done_when_at_least_one_file_changed() {
    for n in [1, 2, 300, u32::MAX] {
        let c = refine(classify(&gave_up_message()), tree(Some(n)));
        assert_eq!(c.outcome, DirectOutcome::ReportedDone, "{n} files");
        assert_eq!(c.outcome.label(), "Cline reported done");
    }
}

/// Unknown is not zero: with no base commit, no worktree, a failed commit or a failed diff the harness measured nothing, and it does not
/// downgrade a stop on a guess.
#[test]
fn an_unmeasured_tree_never_turns_reported_done_into_no_changes() {
    let c = refine(classify(&gave_up_message()), tree(None));
    assert_eq!(c.outcome, DirectOutcome::ReportedDone);
}

#[test]
fn only_reported_done_is_refined_every_other_outcome_keeps_its_words_and_specifics() {
    for outcome in DirectOutcome::ALL {
        if outcome == DirectOutcome::ReportedDone {
            continue;
        }
        let before = Classified {
            outcome,
            turn_outcome: TurnOutcome::Failed,
            detail: Some("specifics".into()),
            fault_hint: None,
        };
        assert_eq!(refine(before.clone(), tree(Some(0))), before, "{outcome:?}");
        assert_eq!(refine(before.clone(), tree(Some(4))), before, "{outcome:?}");
        assert_eq!(refine(before.clone(), tree(None)), before, "{outcome:?}");
    }
}

#[test]
fn the_refinement_is_stable_when_applied_twice() {
    let once = refine(classify(&gave_up_message()), tree(Some(0)));
    assert_eq!(refine(once.clone(), tree(Some(0))), once);
}

/// A blank final message is row 3 whatever the tree holds; a stop reason other than end_turn keeps its own row.
#[test]
fn the_other_end_turn_and_stop_rows_win_over_the_tree() {
    let blank = refine(classify(&raw(done("end_turn"), false, "  ")), tree(Some(0)));
    assert_eq!(blank.outcome, DirectOutcome::StoppedNoMessage);
    for reason in ["max_tokens", "max_turn_requests", "refusal", "weird"] {
        let c = classify(&raw(done(reason), false, "text"));
        let before = c.outcome;
        assert_eq!(refine(c, tree(Some(0))).outcome, before, "{reason}");
    }
}

// ---- rows 4 to 6: the other stop reasons -------------------------------------------------------------------------------------

#[test]
fn row_4_max_tokens_is_the_output_limit() {
    let c = classify(&raw(done("max_tokens"), false, "partial"));
    assert_eq!(c.outcome, DirectOutcome::StoppedMaxTokens);
    assert_eq!(c.turn_outcome, TurnOutcome::Completed);
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: it reached the output limit"
    );
}

#[test]
fn row_5_max_turn_requests_is_the_request_limit() {
    let c = classify(&raw(done("max_turn_requests"), false, "partial"));
    assert_eq!(c.outcome, DirectOutcome::StoppedMaxRequests);
    assert_eq!(c.turn_outcome, TurnOutcome::Completed);
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: it reached its request limit"
    );
}

#[test]
fn row_6_refusal_is_the_model_refusing() {
    let c = classify(&raw(done("refusal"), false, "I cannot"));
    assert_eq!(c.outcome, DirectOutcome::StoppedRefusal);
    assert_eq!(c.turn_outcome, TurnOutcome::Completed);
    assert_eq!(c.outcome.label(), "Cline stopped: the model refused");
}

// ---- rows 7 to 9: cancelled ----------------------------------------------------------------------------------------------------

#[test]
fn row_7_cancelled_after_the_client_cancel_is_the_user_stopping_the_run() {
    let c = classify(&raw(done("cancelled"), true, "so far"));
    assert_eq!(c.outcome, DirectOutcome::StoppedByUser);
    assert_eq!(c.turn_outcome, TurnOutcome::Cancelled);
    assert_eq!(c.outcome.label(), "Cline stopped: you stopped the run");
}

#[test]
fn row_8_cancelled_with_no_client_cancel_is_cline_ending_the_run_itself() {
    let c = classify(&raw(done("cancelled"), false, "so far"));
    assert_eq!(c.outcome, DirectOutcome::StoppedEngineCancelled);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert!(
        c.outcome
            .label()
            .starts_with("Cline stopped: Cline ended the run itself"),
        "{}",
        c.outcome.label()
    );
}

/// On the pinned engine a really cut model stream never ends `cancelled` (it is a -32603), so the hint
/// `stream-cut` and the transport `closed` named a cause the evidence does not show and misled the reader (both analyzer models read
/// the loop-guard run as a transient fault). Without the facts of the attempt the classifier claims only what it knows.
#[test]
fn row_8_without_the_facts_of_the_attempt_names_no_cut_stream_and_no_transport() {
    let c = classify(&raw(done("cancelled"), false, "so far"));
    let hint = c.fault_hint.unwrap();
    assert_eq!(hint.code.as_deref(), Some("unexplained"));
    assert_eq!(hint.transport, None, "no transport fault is shown");
    let detail = c.detail.unwrap();
    assert!(!detail.contains("cut model stream"), "{detail}");
    assert!(!detail.contains("cut provider stream"), "{detail}");
    assert!(detail.contains("no cancel request"), "{detail}");
    assert!(
        detail.contains("did not change the model"),
        "the detail keeps saying that agentd moved nothing: {detail}"
    );
}

/// The label is per outcome and cannot depend on the shape, so it names no cause that only some stops have. Turns green when package P
/// changes `DirectOutcome::label` (harness-protocol `direct.rs`, pinned there in `wire_direct_runtime.rs`).
#[test]
fn row_8_the_frozen_label_does_not_blame_a_cut_model_stream() {
    let label = DirectOutcome::StoppedEngineCancelled.label();
    assert!(!label.contains("cut model stream"), "{label}");
    assert!(label.starts_with("Cline stopped: "), "{label}");
}

fn engine_stop_with(facts: AttemptFacts, ran_s: u64) -> Classified {
    let c = classify(&raw(done("cancelled"), false, "so far"));
    refine_engine_stop(c, &StopReport::new(facts, Duration::from_secs(ran_s)))
}

fn facts(completed: u64, failed_tail: u32, identical_tail: u32) -> AttemptFacts {
    AttemptFacts {
        tool_calls: completed + u64::from(failed_tail),
        completed,
        failed_tail,
        identical_tail,
    }
}

#[test]
fn row_8_five_identical_calls_at_the_end_are_the_loop_guard() {
    let c = engine_stop_with(facts(40, 0, 5), 600);
    assert_eq!(c.outcome, DirectOutcome::StoppedEngineCancelled);
    let hint = c.fault_hint.unwrap();
    assert_eq!(hint.code.as_deref(), Some("loop-guard"));
    assert_eq!(hint.transport, None);
    let detail = c.detail.unwrap();
    assert!(
        detail.contains("5 identical tool calls in a row"),
        "{detail}"
    );
    assert!(!detail.contains("cut model stream"), "{detail}");
    assert!(detail.contains("did not change the model"), "{detail}");
}

#[test]
fn row_8_six_failed_calls_at_the_end_are_the_mistake_limit() {
    let c = engine_stop_with(facts(0, 6, 1), 90);
    let hint = c.fault_hint.unwrap();
    assert_eq!(hint.code.as_deref(), Some("mistake-limit"));
    assert_eq!(hint.transport, None);
    assert!(c.detail.unwrap().contains("6 failed tool calls in a row"));
}

#[test]
fn row_8_a_stop_the_facts_do_not_explain_stays_unexplained() {
    let c = engine_stop_with(facts(2, 0, 1), 20);
    let hint = c.fault_hint.unwrap();
    assert_eq!(hint.code.as_deref(), Some("unexplained"));
    assert_eq!(hint.transport, None);
    assert!(c.detail.unwrap().contains("no repeated or failed calls"));
}

#[test]
fn the_engine_stop_refinement_touches_no_other_outcome() {
    let before = classify(&raw(done("end_turn"), false, "done: edited src/calc.py"));
    let after = refine_engine_stop(
        before.clone(),
        &StopReport::new(facts(40, 0, 5), Duration::from_secs(600)),
    );
    assert_eq!(after, before);
    let max = classify(&raw(done("max_tokens"), false, "cut"));
    assert_eq!(
        refine_engine_stop(
            max.clone(),
            &StopReport::new(facts(0, 6, 1), Duration::from_secs(60))
        ),
        max
    );
}

#[test]
fn the_refined_detail_stays_one_scrubbed_line_within_the_limit() {
    let c = engine_stop_with(facts(40, 0, 5), 600);
    let detail = c.detail.unwrap();
    assert!(detail.chars().count() <= MAX_DIRECT_DETAIL_CHARS);
    assert!(!detail.contains('\n'));
}

#[test]
fn row_9_a_cancel_the_engine_ignored_is_the_user_stopping_the_run() {
    let c = classify(&raw(
        Watched::Done(Err(AcpError::CancelIgnored)),
        true,
        "so far",
    ));
    assert_eq!(c.outcome, DirectOutcome::StoppedByUser);
    assert_eq!(c.turn_outcome, TurnOutcome::Cancelled);
    assert!(c.detail.unwrap().contains("session/cancel"));
}

// ---- rows 10 and 11: errors during the prompt --------------------------------------------------------------------------------

#[test]
fn row_10_a_json_rpc_error_answering_the_prompt_is_the_provider_failing() {
    let c = classify(&raw(
        Watched::Done(Err(AcpError::Rpc {
            code: -32603,
            message: "ThrottlingException: Too many requests".into(),
            data: Some(serde_json::json!({"statusCode": 429, "retryAfter": 7})),
        })),
        false,
        "so far",
    ));
    assert_eq!(c.outcome, DirectOutcome::StoppedProviderError);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    let detail = c.detail.unwrap();
    assert!(
        detail.contains("-32603") && detail.contains("ThrottlingException"),
        "{detail}"
    );
    let hint = c.fault_hint.unwrap();
    assert_eq!(hint.http_status, Some(429));
    assert_eq!(hint.retry_after_s, Some(7));
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: the model provider failed"
    );
}

#[test]
fn row_10_the_detail_is_cut_and_carries_no_control_characters() {
    let c = classify(&raw(
        Watched::Done(Err(AcpError::Rpc {
            code: -32000,
            message: format!("boom\u{7}\n{}", "x".repeat(1000)),
            data: None,
        })),
        false,
        "",
    ));
    let detail = c.detail.unwrap();
    assert!(detail.chars().count() <= MAX_DIRECT_DETAIL_CHARS);
    assert!(!detail.chars().any(char::is_control), "{detail:?}");
}

#[test]
fn row_11_a_closed_or_broken_engine_is_the_process_failing() {
    for err in [
        AcpError::Closed,
        AcpError::Io("broken pipe".into()),
        AcpError::Protocol("not json".into()),
    ] {
        let c = classify(&raw(Watched::Done(Err(err.clone())), false, "so far"));
        assert_eq!(c.outcome, DirectOutcome::StoppedEngineDied, "{err}");
        assert_eq!(c.turn_outcome, TurnOutcome::Failed);
        assert_eq!(c.outcome.label(), "Cline stopped: the Cline process failed");
    }
}

#[test]
fn row_11_a_closed_engine_carries_the_transport_hint() {
    let c = classify(&raw(Watched::Done(Err(AcpError::Closed)), false, ""));
    assert_eq!(
        c.fault_hint.unwrap().transport,
        Some(FaultTransport::Closed)
    );
}

// ---- rows 12 and 13: the watchdogs -----------------------------------------------------------------------------------------------

#[test]
fn row_12_silence_past_the_limit_is_the_silence_stop() {
    let c = classify(&raw(
        Watched::Stalled {
            silent: Duration::from_secs(901),
        },
        false,
        "so far",
    ));
    assert_eq!(c.outcome, DirectOutcome::StoppedSilence);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert!(c.detail.unwrap().contains("901"));
    let hint = c.fault_hint.unwrap();
    assert_eq!(hint.code.as_deref(), Some("stall"));
    assert_eq!(hint.silent_s, Some(901));
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: no output for the silence limit"
    );
}

#[test]
fn row_13_the_deadline_is_the_time_limit() {
    let c = classify(&raw(Watched::TimedOut, true, "so far"));
    assert_eq!(c.outcome, DirectOutcome::StoppedTimeout);
    assert_eq!(c.turn_outcome, TurnOutcome::TimedOut);
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: the run reached its time limit"
    );
}

// ---- the end sequence's create-only objects: a key that already held an object ---------------------------------------------------

/// The bundle, `summary.json` and `test-report.json` are written once (`If-None-Match: *`). A 412 means another writer got there
/// first: agentd did not write the object, and the run says so on row 14 with the harness's own stop sentence, so the label names the
/// object. (agentd then writes its own object over the foreign one; the classification is the same whether that worked or not.) No new outcome and no new label text: `label_for` reads the reason out of the detail.
#[test]
fn a_conflict_on_an_object_agentd_writes_is_stopped_other_with_the_object_in_the_label() {
    let c = upload_conflict("turns/1/summary.json");
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert_eq!(c.fault_hint, None);
    let detail = c.detail.clone().unwrap();
    assert!(detail.starts_with(HARNESS_STOP_PREFIX), "{detail}");
    assert!(detail.contains("turns/1/summary.json"), "{detail}");
    assert!(detail.chars().count() <= MAX_DIRECT_DETAIL_CHARS);
    assert!(!detail.contains('\n'));
    let label = c.outcome.label_for(Some(&detail));
    assert_eq!(
        label,
        "Cline stopped: the harness ended the run (the object turns/1/summary.json already existed before agentd wrote it)"
    );
    assert_eq!(banned_claim_word(&label), None);
    assert_eq!(banned_claim_word(&detail), None);
    for object in ["turns/1/bundle", "turns/1/test-report.json"] {
        let c = upload_conflict(object);
        assert_eq!(c.outcome, DirectOutcome::StoppedOther, "{object}");
        assert_eq!(c.turn_outcome, TurnOutcome::Failed, "{object}");
        let label = c.outcome.label_for(c.detail.as_deref());
        assert!(label.contains(object), "{label}");
        assert!(
            label.starts_with("Cline stopped: the harness ended the run ("),
            "{label}"
        );
    }
    // The event of the conflict: the object and one plain sentence, never the body of either object.
    let p = upload_conflict_payload("turns/1/summary.json");
    assert_eq!(p["kind"], "upload-conflict");
    assert_eq!(p["object"], "turns/1/summary.json");
    assert_eq!(p["turn"], 1);
    let text = p["text"].as_str().unwrap();
    assert!(text.contains("turns/1/summary.json"), "{text}");
    assert_eq!(banned_claim_word(text), None);
}

/// The sentence a final error carries when agentd's own object could not be written over a foreign one: fixed lead, the cause scrubbed
/// to one line, and no word that claims the work is good.
#[test]
fn the_sentence_for_an_object_that_could_not_be_written_over_is_one_scrubbed_line() {
    let text = not_replaced_text("s3 PUT s3://b/k returned 500 InternalError: fake\nsecond line");
    assert!(
        text.starts_with("agentd's own object could not be written over it: "),
        "{text}"
    );
    assert!(text.contains("returned 500 InternalError"), "{text}");
    assert!(!text.contains('\n'), "{text}");
    assert_eq!(banned_claim_word(&text), None);
    // The foreign object's sentence stays the one the label is read from: this sentence is only added to the final error.
    let c = upload_conflict("turns/1/bundle");
    assert!(!c.detail.unwrap().contains("could not be written over"));
}

// ---- row 14 and the shared rules --------------------------------------------------------------------------------------------------

#[test]
fn row_14_an_unknown_stop_reason_is_named_in_the_detail() {
    let c = classify(&raw(done("brand_new_reason"), false, "text"));
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(c.turn_outcome, TurnOutcome::Completed);
    assert!(c.detail.unwrap().contains("brand_new_reason"));
    assert_eq!(
        c.outcome.label(),
        "Cline stopped: the engine reported an unknown stop reason"
    );
}

#[test]
fn a_setup_error_wins_over_everything_else() {
    let mut r = raw(Watched::TimedOut, true, "text");
    r.setup_error = Some("worktree: git failed".into());
    assert_eq!(classify(&r).outcome, DirectOutcome::StoppedSetupFailed);
}

#[test]
fn a_stop_reason_is_reported_only_for_a_prompt_that_answered() {
    assert_eq!(
        stop_reason(&raw(done("end_turn"), false, "x")).as_deref(),
        Some("end_turn")
    );
    assert_eq!(stop_reason(&raw(Watched::TimedOut, false, "x")), None);
    assert_eq!(
        stop_reason(&raw(Watched::Done(Err(AcpError::Closed)), false, "x")),
        None
    );
}

#[test]
fn no_label_or_detail_claims_the_work_was_checked() {
    for word in BANNED_CLAIM_WORDS {
        assert!(!word.is_empty());
    }
    for outcome in DirectOutcome::ALL {
        assert_eq!(banned_claim_word(outcome.label()), None, "{outcome:?}");
    }
    // Every row's detail, over the whole table of inputs above, stays clear of the same words.
    let inputs = [
        raw(done("end_turn"), false, "m"),
        raw(done("max_tokens"), false, "m"),
        raw(done("refusal"), false, "m"),
        raw(done("cancelled"), true, "m"),
        raw(done("cancelled"), false, "m"),
        raw(Watched::TimedOut, false, "m"),
        raw(
            Watched::Stalled {
                silent: Duration::from_secs(5),
            },
            false,
            "m",
        ),
        raw(Watched::Done(Err(AcpError::Closed)), false, "m"),
        raw(Watched::Done(Err(AcpError::CancelIgnored)), true, "m"),
        raw(done("unknown"), false, "m"),
    ];
    for r in &inputs {
        let c = classify(r);
        if let Some(detail) = c.detail {
            assert_eq!(banned_claim_word(&detail), None, "{detail}");
        }
    }
}

#[test]
fn control_characters_are_removed_from_a_message_but_newlines_and_tabs_stay() {
    assert_eq!(
        clean_message("a\u{0}b\u{7}c\nd\te\u{1b}[0m"),
        "abc\nd\te[0m"
    );
    assert_eq!(clean_message(""), "");
}

#[test]
fn known_secret_values_are_replaced_wherever_they_appear() {
    let secrets = [
        "sEcReT/key+value==".to_string(),
        "tok-123456789".to_string(),
    ];
    assert_eq!(
        redact_known(
            "a sEcReT/key+value== b tok-123456789 c sEcReT/key+value==",
            &secrets
        ),
        "a <redacted> b <redacted> c <redacted>"
    );
    assert_eq!(redact_known("nothing here", &secrets), "nothing here");
    assert_eq!(redact_known("anything", &[]), "anything");
}

#[test]
fn a_secret_too_short_to_be_one_is_not_redacted_it_would_eat_ordinary_words() {
    assert_eq!(
        redact_known(
            "a b c",
            &["a".to_string(), "".to_string(), "abc".to_string()]
        ),
        "a b c"
    );
}

// ---- a summary the harness could not write ------------------------------------------------------------------------

#[test]
fn a_done_the_harness_could_not_record_reads_as_the_harness_stopping_the_run() {
    let c = summary_not_written(
        "/var/lib/agentd/run1/direct/turns/1: Read-only file system (os error 30)",
    );
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert!(c.fault_hint.is_none());
    let detail = c.detail.as_deref().unwrap();
    assert!(detail.starts_with(HARNESS_STOP_PREFIX), "{detail}");
    let label = DirectOutcome::StoppedOther.label_for(Some(detail));
    assert!(
        label.starts_with(
            "Cline stopped: the harness ended the run (agentd could not write summary.json:"
        ),
        "{label}"
    );
    assert!(!label.contains("reported done"), "{label}");
    assert!(banned_claim_word(detail).is_none(), "{detail}");
}

#[test]
fn a_checkout_agentd_will_not_run_git_in_reads_as_the_harness_stopping_the_run() {
    let c = repository_not_trusted(
        ".git is a file naming another directory, not the repository's own directory",
    );
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert!(c.fault_hint.is_none());
    let detail = c.detail.as_deref().unwrap();
    assert!(detail.starts_with(HARNESS_STOP_PREFIX), "{detail}");
    let label = DirectOutcome::StoppedOther.label_for(Some(detail));
    assert!(
        label.starts_with(
            "Cline stopped: the harness ended the run (agentd did not commit or bundle the checkout: .git is a file"
        ),
        "{label}"
    );
    assert!(!label.contains("reported done"), "{label}");
    assert!(banned_claim_word(detail).is_none(), "{detail}");
}
