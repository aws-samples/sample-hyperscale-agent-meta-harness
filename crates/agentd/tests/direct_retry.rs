//! The first slice of provider failover: when the one `session/prompt` of a direct run ends in a provider failure, agentd
//! sends the fixed resume sentence on the SAME engine session, a bounded number of times, and records every try. This file pins the
//! pure parts (which failures are retried and how often, the wait between tries, the fixed sentence, the settings, the `attempts`
//! of `summary.json`); the wire behaviour is pinned in `direct_driver.rs`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpError, Watched};
use agentd::config::DirectConfig;
use agentd::direct::classify::{PromptDone, RawEnd, classify};
use agentd::direct::retry::{
    RESUME_PROMPT, RESUME_PROMPT_INTERRUPTED, RetryClass, RetryPolicy, classify_failure,
};
use agentd::direct::summary::{SummaryInputs, build_summary, nudge_note};
use harness_protocol::common::Protocol;
use harness_protocol::direct::{
    DirectAttempt, DirectAttemptKind, DirectAttemptResult, DirectGit, DirectModelSpec,
    DirectPackages, DirectRulesPreflight, DirectSnapshots, DirectTiming, MAX_DIRECT_ATTEMPTS,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

fn rpc(message: &str, data: Option<Value>) -> AcpError {
    AcpError::Rpc {
        code: -32603,
        message: message.into(),
        data,
    }
}

// ---- which failures are retried -----------------------------------------------------------------------------------------------

#[test]
fn the_engine_s_own_idle_timeout_is_transient() {
    // The run of 2026-10-14 (web sample, GPT-6.1 Sol): `engine error -32603: Internal error: The operation timed out.`, no data.
    for message in [
        "Internal error: The operation timed out.",
        "Internal error: The operation timed out",
    ] {
        assert_eq!(
            classify_failure(&rpc(message, None)),
            RetryClass::Transient,
            "{message}"
        );
        assert_eq!(
            classify_failure(&rpc(
                message,
                Some(json!({"message": "The operation timed out."}))
            )),
            RetryClass::Transient
        );
    }
    assert_eq!(
        classify_failure(&rpc(
            "Internal error: Provider response headers timed out after 300000ms",
            None
        )),
        RetryClass::Transient
    );
}

#[test]
fn the_sdk_s_retryable_flag_a_transient_name_or_a_server_status_is_transient() {
    let cases = [
        json!({"isRetryable": true, "errorName": "SomethingNew"}),
        json!({"errorName": "ThrottlingException"}),
        json!({"errorName": "ServiceUnavailableException", "statusCode": 503}),
        json!({"errorName": "InternalServerException", "statusCode": 500}),
        json!({"errorName": "ModelStreamErrorException"}),
        json!({"errorName": "ModelTimeoutException"}),
        json!({"errorName": "ModelNotReadyException"}),
        json!({"errorName": "TimeoutError"}),
        // The shapes a cut stream takes on the 3.0.71 binary (fake Bedrock, 2026-10-14): a reset after a chunk of text, and an exception frame.
        json!({"code": "ECONNRESET", "isRetryable": true, "phase": "stream"}),
        json!({"code": "ECONNRESET"}),
        json!({"errorName": "ModelStreamErrorException", "statusCode": 424, "isRetryable": true, "phase": "stream"}),
        json!({"errorName": "ServiceUnavailableException", "statusCode": 503, "isRetryable": true, "phase": "stream"}),
        json!({"statusCode": 429}),
        json!({"statusCode": 502}),
        json!({"statusCode": 529, "isRetryable": false}),
        json!({"statusCode": 408}),
        json!({"statusCode": 424}),
    ];
    for data in cases {
        assert_eq!(
            classify_failure(&rpc("Internal error: x", Some(data.clone()))),
            RetryClass::Transient,
            "{data}"
        );
    }
}

#[test]
fn a_request_the_provider_refused_is_permanent_and_gets_one_more_try() {
    // The sub-agent of a run can fail with it while the main agent finishes (measured on the 3.0.71 binary, 2026-10-14), so one try on
    // the same session is worth its cost; the same request failing again is not transient.
    for data in [
        json!({"errorName": "ValidationException", "statusCode": 400, "isRetryable": false}),
        // A 403 with no name is not known to be the route: the credentials may be at fault.
        json!({"statusCode": 403}),
        json!({"isRetryable": false}),
    ] {
        assert_eq!(
            classify_failure(&rpc("Internal error: x", Some(data.clone()))),
            RetryClass::Permanent,
            "{data}"
        );
    }
    // No structure and no known wording: the shape of the failure is not known, so one try.
    assert_eq!(
        classify_failure(&rpc("Internal error: something odd", None)),
        RetryClass::Permanent
    );
}

#[test]
fn a_route_the_provider_refuses_is_unavailable_and_gets_no_resume() {
    for data in [
        json!({"errorName": "AccessDeniedException", "statusCode": 403}),
        json!({"errorName": "ResourceNotFoundException", "statusCode": 404}),
        json!({"statusCode": 404}),
    ] {
        assert_eq!(
            classify_failure(&rpc("Internal error: x", Some(data.clone()))),
            RetryClass::Unavailable,
            "{data}"
        );
    }
    for message in [
        "Internal error: The provided model identifier is invalid.",
        "Internal error: Invocation of model ID anthropic.claude-opus-5-5 with on-demand throughput isn't supported. Retry your request with the ID or ARN of an inference profile that contains this model.",
        "Internal error: User: arn:aws:sts::1:assumed-role/x/y is not authorized to perform: bedrock:InvokeModelWithResponseStream on resource: arn:aws:bedrock:us-east-1:1:inference-profile/global.x",
        "Internal error: You don't have access to the model with the specified model ID.",
    ] {
        assert_eq!(
            classify_failure(&rpc(message, None)),
            RetryClass::Unavailable,
            "{message}"
        );
    }
    assert_eq!(RetryClass::Unavailable.as_str(), "unavailable");
    let policy = RetryPolicy::new(3, Duration::from_secs(5));
    assert_eq!(policy.allowed(RetryClass::Unavailable), 0);
    // A token that expired between its signing and its use is the credentials, which agentd renews: it passes.
    for name in ["ExpiredTokenException", "RequestExpired"] {
        assert_eq!(
            classify_failure(&rpc(
                "Internal error: The security token included in the request is expired",
                Some(json!({"errorName": name, "statusCode": 403}))
            )),
            RetryClass::Transient,
            "{name}"
        );
    }
}

#[test]
fn the_real_error_for_a_profile_id_that_does_not_exist_is_unavailable() {
    let e = rpc(
        "Internal error: The provided model identifier is invalid.",
        Some(json!({
            "message": "The provided model identifier is invalid.",
            "errorName": "ValidationException",
            "statusCode": 400,
            "isRetryable": false,
            "phase": "request"
        })),
    );
    assert_eq!(classify_failure(&e), RetryClass::Unavailable);
}

#[test]
fn a_block_by_the_provider_s_filter_and_the_output_limit_are_never_retried() {
    for message in [
        "Internal error: Model returned no content because the response was blocked by a content filter. Retrying is unlikely to help — try rephrasing the request.",
        "Internal error: Model reached the maximum output token limit before completing the turn",
    ] {
        assert_eq!(
            classify_failure(&rpc(message, Some(json!({"isRetryable": true})))),
            RetryClass::Never,
            "{message}: the words win over the flag"
        );
    }
}

#[test]
fn a_history_the_provider_refuses_is_its_own_class_whatever_the_retry_flag_says() {
    // Class 4 of the recovery ladder: the conversation the engine replays is what fails, not the route.
    let messages = [
        // The Astra shape, captured live in A15 on 2026-10-15.
        "Internal error: The format of the value at messages.7.content.0.toolUse.input is invalid. Provide a json object for the field and try again.",
        "Internal error: 2 validation errors detected: Value at 'messages.36.member.content.2.member.toolUse.name' failed to satisfy constraint",
        "Internal error: context window exceeded",
        "Internal error: Input is too long for requested model",
        "Internal error: prompt exceeds the context length of the model",
        "Internal error: input exceeds the maximum context",
    ];
    for message in messages {
        for data in [
            json!({"statusCode": 400, "errorName": "ValidationException", "isRetryable": false}),
            json!({"isRetryable": true}),
        ] {
            assert_eq!(
                classify_failure(&rpc(message, Some(data.clone()))),
                RetryClass::HistoryUnusable,
                "{message} {data}"
            );
        }
    }
    // A 400 about the request's own shape that names no message of the history stays a permanent failure, and the word alone is not enough.
    for message in [
        "Internal error: Malformed input request",
        "Internal error: Too many messages in the request",
        "Internal error: the messages.foo field is invalid",
    ] {
        assert_eq!(
            classify_failure(&rpc(
                message,
                Some(
                    json!({"statusCode": 400, "errorName": "ValidationException", "isRetryable": false})
                )
            )),
            RetryClass::Permanent,
            "{message}"
        );
    }
    assert_eq!(RetryClass::HistoryUnusable.as_str(), "history_unusable");
    assert_eq!(
        RetryPolicy::new(3, Duration::from_secs(5)).allowed(RetryClass::HistoryUnusable),
        0
    );
}

#[test]
fn only_a_json_rpc_internal_error_is_retried_here() {
    // Auth (-32000), invalid params, a closed pipe, a protocol break, a cancel that was ignored: not a provider failure this slice
    // handles. A dead engine needs a restart, which is the second slice.
    for e in [
        AcpError::Rpc {
            code: -32000,
            message: "auth required".into(),
            data: None,
        },
        AcpError::Rpc {
            code: -32602,
            message: "bad".into(),
            data: None,
        },
        AcpError::Closed,
        AcpError::Io("broken pipe".into()),
        AcpError::Protocol("x".into()),
        AcpError::CancelIgnored,
        AcpError::ModelMismatch {
            wanted: "a".into(),
            got: None,
        },
    ] {
        assert_eq!(classify_failure(&e), RetryClass::Never, "{e}");
    }
}

#[test]
fn the_class_names_are_the_ones_summary_json_carries() {
    assert_eq!(RetryClass::Transient.as_str(), "transient");
    assert_eq!(RetryClass::Permanent.as_str(), "permanent");
    assert_eq!(RetryClass::Never.as_str(), "never");
}

// ---- how often and how long -------------------------------------------------------------------------------------------------------

#[test]
fn a_policy_allows_three_transient_tries_and_one_permanent_and_none_when_off() {
    let p = RetryPolicy::new(3, Duration::from_secs(5));
    assert_eq!(p.allowed(RetryClass::Transient), 3);
    assert_eq!(p.allowed(RetryClass::Permanent), 1);
    assert_eq!(p.allowed(RetryClass::Never), 0);
    let off = RetryPolicy::off();
    for c in [
        RetryClass::Transient,
        RetryClass::Permanent,
        RetryClass::Never,
    ] {
        assert_eq!(off.allowed(c), 0);
    }
    let two = RetryPolicy::new(2, Duration::from_secs(5));
    assert_eq!(
        (
            two.allowed(RetryClass::Transient),
            two.allowed(RetryClass::Permanent)
        ),
        (2, 1)
    );
    let zero = RetryPolicy::new(0, Duration::from_secs(5));
    assert_eq!(zero.allowed(RetryClass::Transient), 0);
}

#[test]
fn the_wait_doubles_from_the_base_up_to_the_cap() {
    let p = RetryPolicy::new(8, Duration::from_secs(5));
    let e = rpc("Internal error: The operation timed out.", None);
    let secs: Vec<u64> = (1..=6).map(|n| p.pause_before(n, &e).as_secs()).collect();
    assert_eq!(secs, [5, 10, 20, 40, 60, 60], "capped at 60 s");
}

#[test]
fn a_retry_after_from_the_provider_raises_the_wait_up_to_two_minutes() {
    let p = RetryPolicy::new(3, Duration::from_secs(5));
    let with = |v: Value| {
        rpc(
            "Internal error: x",
            Some(json!({"statusCode": 429, "retryAfterS": v})),
        )
    };
    assert_eq!(p.pause_before(1, &with(json!(30))).as_secs(), 30);
    assert_eq!(
        p.pause_before(1, &with(json!(3))).as_secs(),
        5,
        "never below the base wait"
    );
    assert_eq!(
        p.pause_before(1, &with(json!(900))).as_secs(),
        120,
        "capped at two minutes"
    );
    assert_eq!(
        p.pause_before(1, &with(json!("soon"))).as_secs(),
        5,
        "a value that is not a number is ignored"
    );
}

#[test]
fn a_jittered_wait_stays_within_a_fifth_above_the_plain_one() {
    let p = RetryPolicy::standard(3, Duration::from_secs(5));
    let e = rpc("Internal error: The operation timed out.", None);
    for n in 1..=3 {
        let plain = p.pause_before(n, &e);
        for _ in 0..50 {
            let j = p.jittered(plain);
            assert!(j >= plain && j <= plain + plain / 5, "{plain:?} -> {j:?}");
        }
    }
    assert_eq!(
        RetryPolicy::new(3, Duration::from_secs(5)).jittered(Duration::from_secs(5)),
        Duration::from_secs(5),
        "the test policy adds none"
    );
}

#[test]
fn the_resume_sentence_is_one_fixed_line_that_says_what_happened_and_asks_for_no_verdict() {
    assert_eq!(
        RESUME_PROMPT,
        "A previous attempt on this work tree was interrupted by a provider failure. Inspect the work tree and continue the task."
    );
    assert!(!RESUME_PROMPT.contains('\n'));
    // The sentence names no marker and does not ask whether the work is complete.
    for banned in ["DONE", "verified", "complete", "finished"] {
        assert!(!RESUME_PROMPT.contains(banned), "{banned}");
    }
}

#[test]
fn the_sentence_for_an_end_the_engine_chose_does_not_blame_the_provider() {
    assert_eq!(
        RESUME_PROMPT_INTERRUPTED,
        "A previous attempt on this work tree was interrupted. Inspect the work tree and continue the task."
    );
    assert!(!RESUME_PROMPT_INTERRUPTED.contains("provider"));
    for banned in ["DONE", "verified", "complete", "finished"] {
        assert!(!RESUME_PROMPT_INTERRUPTED.contains(banned), "{banned}");
    }
}

// ---- the settings --------------------------------------------------------------------------------------------------------------------

fn from(pairs: &[(&str, &str)]) -> DirectConfig {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    DirectConfig::from_vars(&|k| m.get(k).cloned())
}

#[test]
fn the_defaults_are_three_tries_and_a_five_second_base_and_the_switch_turns_them_off() {
    let d = from(&[]);
    assert_eq!(d.provider_retries, 3);
    assert_eq!(d.provider_retry_pause, Duration::from_secs(5));
    assert_eq!(d, DirectConfig::default());
    let off = from(&[("AGENTD_DIRECT_PROVIDER_RETRIES", "0")]);
    assert_eq!(
        off.provider_retries, 0,
        "0 is the kill switch, not a bad value"
    );
    let two = from(&[
        ("AGENTD_DIRECT_PROVIDER_RETRIES", "2"),
        ("AGENTD_DIRECT_RETRY_PAUSE_MS", "250"),
    ]);
    assert_eq!(two.provider_retries, 2);
    assert_eq!(two.provider_retry_pause, Duration::from_millis(250));
    let bad = from(&[
        ("AGENTD_DIRECT_PROVIDER_RETRIES", "many"),
        ("AGENTD_DIRECT_RETRY_PAUSE_MS", "0"),
    ]);
    assert_eq!(bad.provider_retries, 3, "a bad number is the default");
    assert_eq!(
        bad.provider_retry_pause,
        Duration::from_secs(5),
        "a zero pause is the default"
    );
    let big = from(&[("AGENTD_DIRECT_PROVIDER_RETRIES", "50")]);
    assert_eq!(
        big.provider_retries, 6,
        "capped so a mistake cannot loop for hours"
    );
}

// ---- summary.json ------------------------------------------------------------------------------------------------------------------

fn attempt(n: u32, kind: DirectAttemptKind, result: DirectAttemptResult) -> DirectAttempt {
    DirectAttempt {
        n,
        kind,
        started_at: "2026-10-14T15:29:11Z".into(),
        ended_at: "2026-10-14T15:49:53Z".into(),
        result,
        retry_class: None,
        detail: None,
        backoff_s: None,
        model_id: None,
    }
}

fn inputs(attempts: Vec<DirectAttempt>) -> SummaryInputs {
    let raw = RawEnd {
        watched: Watched::Done(Ok(PromptDone {
            stop_reason: "end_turn".into(),
            usage: None,
        })),
        cancel_sent: false,
        message: "Done.".into(),
        setup_error: None,
    };
    SummaryInputs {
        run_id: "01M4XABCDEFGHJKMNPQRSTVWXY".into(),
        classified: classify(&raw),
        stop_reason: Some("end_turn".into()),
        message: Some("Done.".into()),
        model: DirectModelSpec {
            id: "us.openai.gpt-6.1-sol".into(),
            region: "us-east-1".into(),
            protocol: Protocol::Converse,
            context_tokens: 1_000_000,
            max_output_tokens: 128_000,
            default_reasoning_effort: None,
        },
        pinned: Some("us.openai.gpt-6.1-sol".into()),
        engine_name: "cline".into(),
        engine_version: Some("3.0.71".into()),
        engine_binary_sha256: None,
        engine_identity: None,
        rules: vec![],
        rules_preflight: DirectRulesPreflight {
            ran: false,
            ms: None,
        },
        request_sha256: "cd".repeat(32),
        prompt_bytes: 47,
        nudge: nudge_note(None),
        usage: None,
        tool_calls: 3,
        timing: DirectTiming {
            started_at: "2026-10-14T15:29:11Z".into(),
            ended_at: "2026-10-14T15:49:53Z".into(),
            wall_s: 1241,
            prompt_s: Some(1239),
            max_silent_gap_s: Some(337),
        },
        git: DirectGit {
            base_sha: None,
            head_sha: None,
            committed_by_harness: false,
            files_changed: 0,
            bundle_uri: None,
            bundle_bytes: None,
        },
        snapshots: DirectSnapshots::default(),
        packages: DirectPackages::default(),
        error: None,
        attempts,
        switches: vec![],
        final_model: None,
        failover: None,
        outage_s: None,
        reasoning_effort: None,
        reasoning_source: None,
    }
}

#[test]
fn a_run_with_one_try_has_no_attempts_member_so_the_summaries_of_before_do_not_change() {
    let summary = build_summary(inputs(vec![attempt(
        1,
        DirectAttemptKind::Prompt,
        DirectAttemptResult::Ok,
    )]));
    assert!(summary.attempts.is_empty(), "one try is not worth a list");
    let text = serde_json::to_string(&summary).unwrap();
    assert!(!text.contains("attempts"), "{text}");
    summary.validate().unwrap();
}

#[test]
fn a_run_that_retried_lists_every_try_in_order_with_the_class_and_the_wait() {
    let mut first = attempt(
        1,
        DirectAttemptKind::Prompt,
        DirectAttemptResult::ProviderError,
    );
    first.retry_class = Some("transient".into());
    first.detail = Some("engine error -32603: Internal error: The operation timed out.".into());
    first.backoff_s = Some(5);
    let second = attempt(2, DirectAttemptKind::Resume, DirectAttemptResult::Ok);
    let summary = build_summary(inputs(vec![first, second]));
    summary.validate().unwrap();
    let v: Value = serde_json::to_value(&summary).unwrap();
    let list = v["attempts"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["n"], 1);
    assert_eq!(list[0]["kind"], "prompt");
    assert_eq!(list[0]["result"], "provider_error");
    assert_eq!(list[0]["retryClass"], "transient");
    assert_eq!(list[0]["backoffS"], 5);
    assert_eq!(list[0]["startedAt"], "2026-10-14T15:29:11Z");
    assert_eq!(list[1]["kind"], "resume");
    assert_eq!(list[1]["result"], "ok");
    assert!(list[1].get("retryClass").is_none() && list[1].get("backoffS").is_none());
    // The reader of an older summary still parses: the member is optional.
    let mut old = v.clone();
    old.as_object_mut().unwrap().remove("attempts");
    let back: harness_protocol::direct::DirectSummary = serde_json::from_value(old).unwrap();
    assert!(back.attempts.is_empty());
}

#[test]
fn the_list_is_cut_to_its_limit_and_a_long_detail_to_1000_characters() {
    let many: Vec<DirectAttempt> = (1..=(MAX_DIRECT_ATTEMPTS as u32 + 5))
        .map(|n| {
            let mut a = attempt(
                n,
                if n == 1 {
                    DirectAttemptKind::Prompt
                } else {
                    DirectAttemptKind::Resume
                },
                DirectAttemptResult::ProviderError,
            );
            a.detail = Some("x".repeat(1_500));
            a
        })
        .collect();
    let summary = build_summary(inputs(many));
    assert_eq!(summary.attempts.len(), MAX_DIRECT_ATTEMPTS);
    assert!(
        summary
            .attempts
            .iter()
            .all(|a| a.detail.as_deref().unwrap().chars().count() <= 1_000)
    );
    summary.validate().unwrap();
}
