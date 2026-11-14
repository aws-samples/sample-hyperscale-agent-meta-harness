//! Protocol 1.30.0, the direct-run runtime contract:
//! the `InvokeAgentRuntime` body agentd receives, the outcome labels, `summary.json`, the evidence the packager
//! renders, the new members of `run.completed` and the S3 keys of one direct run. Golden JSON goes in and the same
//! JSON comes out; unknown members are refused where the contract says so.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::common::{Protocol, Usage};
use harness_protocol::direct::{
    self, DEFAULT_SNAPSHOT_EVERY_S, DIRECT_AGENT_ID, DIRECT_ENGINE, DIRECT_PREFIX,
    DIRECT_PREFIX_UNDERSCORE, DIRECT_STAGE, DIRECT_TASK_ID, DIRECT_TURN, DirectEvidence,
    DirectInvocationError, DirectInvocationRequest, DirectKind, DirectModelSpec, DirectObservation,
    DirectObservationKind, DirectOutcome, DirectRuleRef, DirectSummary, DirectSummaryError,
    HeadSource, InputRef, MAX_CLINE_MESSAGE_CHARS, MAX_DIRECT_BODY_BYTES, MAX_DIRECT_DETAIL_CHARS,
    MAX_DIRECT_OBSERVATIONS, MAX_DIRECT_SUMMARY_BYTES, MAX_DIRECT_TIMEOUT_S, MAX_OBSERVATION_CHARS,
    SUMMARY_SCHEMA_VERSION, validate_direct_invocation,
};
use harness_protocol::export::all_schemas;
use harness_protocol::orchestration::RunCompletedPayload;
use harness_protocol::runtime::{PackagesToken, RepoSource, TurnOutcome};
use serde_json::{Value, json};

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const BUCKET: &str = "hamh-direct-harness-111122223333";

/// A package token stand-in, built at run time so no token-shaped literal sits in the source.
fn token() -> String {
    ["tok", "test", "0123456789"].join("-")
}

fn golden_request() -> Value {
    json!({
        "kind": "direct",
        "runId": RUN,
        "agentId": "direct",
        "turn": 1,
        "runtimeSessionId": format!("{RUN}-direct-g1"),
        "credentials": {
            "Version": 1,
            "AccessKeyId": "ASIA-test-0123456789",
            "SecretAccessKey": "test-secret-not-real",
            "SessionToken": "test-session-token-not-real",
            "Expiration": "2026-10-13T19:10:00Z"
        },
        "nonce": "test-nonce-not-real",
        "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct",
        "region": "us-east-1",
        "engine": "cline",
        "model": {
            "id": "us.anthropic.claude-opus-5-5",
            "region": "us-east-1",
            "protocol": "converse",
            "contextTokens": 1_000_000,
            "maxOutputTokens": 128_000
        },
        "request": {
            "uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"),
            "sha256": "9f".repeat(32)
        },
        "repo": {
            "kind": "s3Bundle",
            "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")
        },
        "timeoutS": 27_900,
        "snapshotEveryS": 600,
        "packagesToken": token(),
        "packagesTokenExpiresAt": 1_790_000_000u64
    })
}

fn request() -> DirectInvocationRequest {
    serde_json::from_value(golden_request()).unwrap()
}

/// The golden request with one change made by `f` on the wire form.
fn mutated(f: impl FnOnce(&mut Value)) -> DirectInvocationRequest {
    let mut v = golden_request();
    f(&mut v);
    serde_json::from_value(v).unwrap()
}

// ---- C0-4: one golden per type ------------------------------------------------------------------

#[test]
fn the_golden_invocation_request_round_trips_byte_for_byte() {
    let r = request();
    assert_eq!(r.kind, DirectKind::Direct);
    assert_eq!(r.agent_id, DIRECT_AGENT_ID);
    assert_eq!(r.turn, DIRECT_TURN);
    assert_eq!(r.model.protocol, Protocol::Converse);
    assert_eq!(
        r.repo,
        RepoSource::S3Bundle {
            uri: format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")
        }
    );
    assert_eq!(r.packages_token, Some(PackagesToken::new(token())));
    assert_eq!(serde_json::to_value(&r).unwrap(), golden_request());
    assert_eq!(validate_direct_invocation(&r), Ok(()));
}

#[test]
fn optional_members_of_the_invocation_are_left_out_of_the_wire_form() {
    let mut v = golden_request();
    for key in [
        "providerSilenceS",
        "snapshotEveryS",
        "packagesToken",
        "packagesTokenExpiresAt",
    ] {
        v.as_object_mut().unwrap().remove(key);
    }
    let r: DirectInvocationRequest = serde_json::from_value(v.clone()).unwrap();
    assert!(r.provider_silence_s.is_none() && r.snapshot_every_s.is_none());
    assert!(r.packages_token.is_none() && r.packages_token_expires_at.is_none());
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    assert_eq!(validate_direct_invocation(&r), Ok(()));
    // The silence limit round-trips when set.
    let r = mutated(|v| v["providerSilenceS"] = json!(900));
    assert_eq!(r.provider_silence_s, Some(900));
    assert_eq!(serde_json::to_value(&r).unwrap()["providerSilenceS"], 900);
}

#[test]
fn unknown_members_of_the_invocation_are_refused_not_ignored() {
    for path in [
        vec!["extra"],
        vec!["model", "extra"],
        vec!["request", "extra"],
    ] {
        let mut v = golden_request();
        let mut at = &mut v;
        for p in &path[..path.len() - 1] {
            at = &mut at[*p];
        }
        at[path[path.len() - 1]] = json!(1);
        let err = serde_json::from_value::<DirectInvocationRequest>(v).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{path:?}: {err}");
    }
}

#[test]
fn the_kind_member_must_be_direct_and_must_be_present() {
    let mut v = golden_request();
    v["kind"] = json!("turn");
    assert!(serde_json::from_value::<DirectInvocationRequest>(v).is_err());
    let mut v = golden_request();
    v.as_object_mut().unwrap().remove("kind");
    assert!(serde_json::from_value::<DirectInvocationRequest>(v).is_err());
}

#[test]
fn input_ref_and_model_spec_round_trip_alone() {
    let r = InputRef {
        uri: format!("s3://{BUCKET}/runs/{RUN}/input/request.json"),
        sha256: "ab".repeat(32),
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v, json!({"uri": r.uri, "sha256": r.sha256}));
    assert_eq!(serde_json::from_value::<InputRef>(v).unwrap(), r);
    let m = DirectModelSpec {
        id: "us.openai.gpt-6.1-sol".into(),
        region: "us-west-2".into(),
        protocol: Protocol::Converse,
        context_tokens: 400_000,
        max_output_tokens: 64_000,
        default_reasoning_effort: None,
    };
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"id": "us.openai.gpt-6.1-sol", "region": "us-west-2", "protocol": "converse",
               "contextTokens": 400_000, "maxOutputTokens": 64_000})
    );
    assert_eq!(serde_json::from_value::<DirectModelSpec>(v).unwrap(), m);
}

#[test]
fn debug_never_prints_the_nonce_the_credentials_or_the_package_token() {
    let shown = format!("{:?}", request());
    for secret in [
        "test-nonce-not-real",
        "test-secret-not-real",
        "test-session-token-not-real",
        &token(),
    ] {
        assert!(!shown.contains(secret), "Debug printed {secret}");
    }
    assert!(shown.contains("<redacted>"));
    // What is safe to show is shown.
    assert!(shown.contains(RUN));
}

// ---- validate_direct_invocation: the nine steps in order ----------------------------------------------

#[test]
fn each_validation_failure_names_the_first_broken_rule() {
    use DirectInvocationError as E;
    type Case = (&'static str, Box<dyn Fn(&mut Value)>, E);
    let cases: Vec<Case> = vec![
        // 1
        (
            "agent id",
            Box::new(|v| v["agentId"] = json!("planner")),
            E::BadAgentId,
        ),
        ("turn", Box::new(|v| v["turn"] = json!(257)), E::BadTurn),
        (
            "run id underscore",
            Box::new(|v| v["runId"] = json!("run_1")),
            E::BadRunId,
        ),
        (
            "run id empty",
            Box::new(|v| v["runId"] = json!("")),
            E::BadRunId,
        ),
        (
            "role session name over 64",
            Box::new(|v| v["runId"] = json!("a".repeat(58))),
            E::RoleSessionNameTooLong,
        ),
        // 2
        (
            "session id short",
            Box::new(|v| v["runtimeSessionId"] = json!("s".repeat(32))),
            E::BadRuntimeSessionId,
        ),
        (
            "session id long",
            Box::new(|v| v["runtimeSessionId"] = json!("s".repeat(257))),
            E::BadRuntimeSessionId,
        ),
        // 3
        (
            "creds version",
            Box::new(|v| v["credentials"]["Version"] = json!(2)),
            E::NotSessionCredentials,
        ),
        (
            "creds long-lived key",
            Box::new(|v| v["credentials"]["AccessKeyId"] = json!("AKIA-test-0123")),
            E::NotSessionCredentials,
        ),
        (
            "nonce empty",
            Box::new(|v| v["nonce"] = json!("")),
            E::MissingNonce,
        ),
        // 4
        (
            "token slash",
            Box::new(|v| v["packagesToken"] = json!("a/b")),
            E::BadPackagesToken,
        ),
        (
            "token empty",
            Box::new(|v| v["packagesToken"] = json!("")),
            E::BadPackagesToken,
        ),
        // 5
        (
            "api http",
            Box::new(|v| v["internalApiUrl"] = json!("http://x.example/direct")),
            E::InternalApiNotHttps,
        ),
        // 6
        (
            "timeout 0",
            Box::new(|v| v["timeoutS"] = json!(0)),
            E::BadTimeout,
        ),
        (
            "timeout over",
            Box::new(|v| v["timeoutS"] = json!(27_901)),
            E::BadTimeout,
        ),
        (
            "silence 0",
            Box::new(|v| v["providerSilenceS"] = json!(0)),
            E::BadProviderSilence,
        ),
        (
            "silence over",
            Box::new(|v| v["providerSilenceS"] = json!(3_601)),
            E::BadProviderSilence,
        ),
        (
            "snapshot 59",
            Box::new(|v| v["snapshotEveryS"] = json!(59)),
            E::BadSnapshotInterval,
        ),
        (
            "snapshot 3601",
            Box::new(|v| v["snapshotEveryS"] = json!(3_601)),
            E::BadSnapshotInterval,
        ),
        // 7
        (
            "model id space",
            Box::new(|v| v["model"]["id"] = json!("us anthropic")),
            E::BadModelId,
        ),
        (
            "model id empty",
            Box::new(|v| v["model"]["id"] = json!("")),
            E::BadModelId,
        ),
        (
            "model region",
            Box::new(|v| v["model"]["region"] = json!("us-east")),
            E::BadModelRegion,
        ),
        (
            "context window",
            Box::new(|v| v["model"]["contextTokens"] = json!(0)),
            E::BadModelWindow,
        ),
        (
            "max output",
            Box::new(|v| v["model"]["maxOutputTokens"] = json!(0)),
            E::BadModelWindow,
        ),
        // 8
        (
            "engine upper",
            Box::new(|v| v["engine"] = json!("Cline")),
            E::BadEngine,
        ),
        (
            "engine empty",
            Box::new(|v| v["engine"] = json!("")),
            E::BadEngine,
        ),
        // 9
        (
            "request other run",
            Box::new(|v| {
                v["request"]["uri"] = json!(format!("s3://{BUCKET}/runs/OTHER/input/request.json"))
            }),
            E::BadRequestRef,
        ),
        (
            "request other key",
            Box::new(|v| {
                v["request"]["uri"] = json!(format!("s3://{BUCKET}/runs/{RUN}/input/other.json"))
            }),
            E::BadRequestRef,
        ),
        (
            "request not s3",
            Box::new(|v| {
                v["request"]["uri"] =
                    json!(format!("https://{BUCKET}/runs/{RUN}/input/request.json"))
            }),
            E::BadRequestRef,
        ),
        (
            "request sha upper",
            Box::new(|v| v["request"]["sha256"] = json!("AB".repeat(32))),
            E::BadRequestRef,
        ),
        (
            "request sha short",
            Box::new(|v| v["request"]["sha256"] = json!("ab".repeat(31))),
            E::BadRequestRef,
        ),
        (
            "repo local",
            Box::new(|v| v["repo"] = json!({"kind": "local", "path": "/tmp/repo"})),
            E::BadRepo,
        ),
        (
            "repo other bucket",
            Box::new(|v| {
                v["repo"]["uri"] = json!(format!(
                    "s3://other-bucket-name/runs/{RUN}/input/repo.bundle"
                ))
            }),
            E::BadRepo,
        ),
        (
            "repo other key",
            Box::new(|v| {
                v["repo"]["uri"] = json!(format!("s3://{BUCKET}/runs/{RUN}/input/tree.json"))
            }),
            E::BadRepo,
        ),
    ];
    for (name, change, want) in cases {
        let r = mutated(|v| change(v));
        assert_eq!(validate_direct_invocation(&r), Err(want), "{name}");
    }
}

#[test]
fn the_limits_themselves_are_accepted() {
    let r = mutated(|v| {
        v["timeoutS"] = json!(MAX_DIRECT_TIMEOUT_S);
        v["providerSilenceS"] = json!(3_600);
        v["snapshotEveryS"] = json!(0);
        v["runId"] = json!("a".repeat(57));
        v["runtimeSessionId"] = json!("s".repeat(256));
        v["request"]["uri"] = json!(format!(
            "s3://{BUCKET}/runs/{}/input/request.json",
            "a".repeat(57)
        ));
        v["repo"]["uri"] = json!(format!(
            "s3://{BUCKET}/runs/{}/input/repo.bundle",
            "a".repeat(57)
        ));
    });
    assert_eq!(validate_direct_invocation(&r), Ok(()));
    assert_eq!(
        validate_direct_invocation(&mutated(|v| {
            v["timeoutS"] = json!(1);
            v["providerSilenceS"] = json!(1);
            v["snapshotEveryS"] = json!(60);
        })),
        Ok(())
    );
    assert_eq!(
        validate_direct_invocation(&mutated(|v| v["snapshotEveryS"] = json!(3_600))),
        Ok(())
    );
}

#[test]
fn the_first_of_two_faults_is_the_one_reported() {
    // Step 1 before step 3, step 3 before step 6, step 6 before step 9.
    let r = mutated(|v| {
        v["turn"] = json!(257);
        v["nonce"] = json!("");
    });
    assert_eq!(
        validate_direct_invocation(&r),
        Err(DirectInvocationError::BadTurn)
    );
    let r = mutated(|v| {
        v["nonce"] = json!("");
        v["timeoutS"] = json!(0);
    });
    assert_eq!(
        validate_direct_invocation(&r),
        Err(DirectInvocationError::MissingNonce)
    );
    let r = mutated(|v| {
        v["timeoutS"] = json!(0);
        v["engine"] = json!("Cline");
        v["request"]["sha256"] = json!("zz");
    });
    assert_eq!(
        validate_direct_invocation(&r),
        Err(DirectInvocationError::BadTimeout)
    );
    let r = mutated(|v| {
        v["engine"] = json!("Cline");
        v["request"]["sha256"] = json!("zz");
    });
    assert_eq!(
        validate_direct_invocation(&r),
        Err(DirectInvocationError::BadEngine)
    );
}

#[test]
fn error_messages_name_the_rule_and_never_a_value() {
    let r = mutated(|v| v["credentials"]["AccessKeyId"] = json!("AKIA-test-secret-looking-0123"));
    let err = validate_direct_invocation(&r).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("ASIA"), "{text}");
    assert!(!text.contains("secret-looking"), "{text}");
    let r = mutated(|v| v["packagesToken"] = json!("tok/with/slash"));
    let text = validate_direct_invocation(&r).unwrap_err().to_string();
    assert!(!text.contains("tok/with"), "{text}");
}

#[test]
fn the_body_limit_and_the_constants_are_the_contract_values() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
    assert_eq!(DIRECT_AGENT_ID, "direct");
    assert_eq!(DIRECT_TURN, 1);
    assert_eq!(DIRECT_TASK_ID, "root");
    assert_eq!(DIRECT_ENGINE, "cline");
    assert_eq!(MAX_DIRECT_TIMEOUT_S, 27_900);
    assert_eq!(DEFAULT_SNAPSHOT_EVERY_S, 60);
    assert_eq!(MAX_DIRECT_BODY_BYTES, 64 * 1024);
    assert_eq!(SUMMARY_SCHEMA_VERSION, 1);
    assert_eq!(MAX_DIRECT_SUMMARY_BYTES, 1024 * 1024);
    assert_eq!(MAX_CLINE_MESSAGE_CHARS, 200_000);
    assert_eq!(MAX_DIRECT_DETAIL_CHARS, 1_000);
    assert_eq!(
        (DIRECT_STAGE, DIRECT_PREFIX, DIRECT_PREFIX_UNDERSCORE),
        ("direct", "hamh-direct", "hamh_direct")
    );
    // 28,800 s of microVM lifetime less the 900 s reserve.
    assert_eq!(MAX_DIRECT_TIMEOUT_S, 28_800 - 900);
    // The golden request is far below the body limit.
    assert!(serde_json::to_vec(&request()).unwrap().len() < MAX_DIRECT_BODY_BYTES / 8);
}

// ---- outcomes and labels (section 3.5) --------------------------------------------------------------------

/// word, label: the frozen text. Every string here is pinned; a change needs the maintainers' decision.
const OUTCOMES: &[(DirectOutcome, &str, &str)] = &[
    (
        DirectOutcome::StoppedSetupFailed,
        "stopped_setup_failed",
        "Cline did not start",
    ),
    (
        DirectOutcome::ReportedDone,
        "reported_done",
        "Cline reported done",
    ),
    (
        DirectOutcome::StoppedNoMessage,
        "stopped_no_message",
        "Cline stopped: it ended its turn without a message",
    ),
    (
        DirectOutcome::StoppedMaxTokens,
        "stopped_max_tokens",
        "Cline stopped: it reached the output limit",
    ),
    (
        DirectOutcome::StoppedMaxRequests,
        "stopped_max_requests",
        "Cline stopped: it reached its request limit",
    ),
    (
        DirectOutcome::StoppedRefusal,
        "stopped_refusal",
        "Cline stopped: the model refused",
    ),
    (
        DirectOutcome::StoppedByUser,
        "stopped_by_user",
        "Cline stopped: you stopped the run",
    ),
    (
        DirectOutcome::StoppedEngineCancelled,
        "stopped_engine_cancelled",
        "Cline stopped: Cline ended the run itself",
    ),
    (
        DirectOutcome::StoppedProviderError,
        "stopped_provider_error",
        "Cline stopped: the model provider failed",
    ),
    (
        DirectOutcome::StoppedEngineDied,
        "stopped_engine_died",
        "Cline stopped: the Cline process failed",
    ),
    (
        DirectOutcome::StoppedSilence,
        "stopped_silence",
        "Cline stopped: no output for the silence limit",
    ),
    (
        DirectOutcome::StoppedTimeout,
        "stopped_timeout",
        "Cline stopped: the run reached its time limit",
    ),
    (
        DirectOutcome::StoppedOther,
        "stopped_other",
        "Cline stopped: the engine reported an unknown stop reason",
    ),
    (
        DirectOutcome::StoppedSessionLost,
        "stopped_session_lost",
        "Cline stopped: the session was lost",
    ),
    (
        DirectOutcome::StoppedNoChanges,
        "stopped_no_changes",
        "Cline stopped: it ended its turn without changing a file",
    ),
];

#[test]
fn the_outcome_table_pins_every_wire_word_and_every_label() {
    assert_eq!(OUTCOMES.len(), 15);
    assert_eq!(DirectOutcome::ALL.len(), OUTCOMES.len());
    for (outcome, word, label) in OUTCOMES {
        assert_eq!(outcome.as_str(), *word);
        assert_eq!(outcome.label(), *label, "{word}");
        assert_eq!(serde_json::to_value(outcome).unwrap(), json!(word));
        let back: DirectOutcome = serde_json::from_value(json!(word)).unwrap();
        assert_eq!(back, *outcome);
        assert!(
            DirectOutcome::ALL.contains(outcome),
            "{word} missing from ALL"
        );
    }
    // Every outcome is in the table exactly once.
    let mut words: Vec<&str> = OUTCOMES.iter().map(|(_, w, _)| *w).collect();
    words.sort_unstable();
    words.dedup();
    assert_eq!(words.len(), 15);
}

#[test]
fn only_one_outcome_says_reported_done_and_every_other_label_starts_cline_stopped_or_did_not_start()
{
    for (outcome, _, label) in OUTCOMES {
        if *outcome == DirectOutcome::ReportedDone {
            assert!(outcome.is_reported_done());
            assert_eq!(*label, "Cline reported done");
        } else {
            assert!(!outcome.is_reported_done());
            assert!(
                label.starts_with("Cline stopped: ") || *label == "Cline did not start",
                "{label}"
            );
            assert!(!label.to_lowercase().contains("done"), "{label}");
        }
    }
}

#[test]
fn no_label_claims_the_harness_checked_anything() {
    for outcome in DirectOutcome::ALL {
        assert_eq!(
            direct::banned_claim_word(outcome.label()),
            None,
            "{}",
            outcome.label()
        );
    }
}

#[test]
fn the_banned_word_check_matches_whole_words_in_any_case() {
    for (text, hit) in [
        ("All Verified by the harness", Some("verified")),
        ("tests were GATED", Some("gated")),
        ("the evaluated result", Some("evaluated")),
        ("fully tested.", Some("tested")),
        ("(tested)", Some("tested")),
        // Whole words only: these contain a banned word inside a longer word.
        ("attested by a notary", None),
        ("unverified claim", None),
        ("Cline reported done", None),
        ("", None),
    ] {
        assert_eq!(direct::banned_claim_word(text), hit, "{text:?}");
    }
}

// ---- summary.json (section 3.6) -------------------------------------------------------------------------------

fn golden_summary() -> Value {
    json!({
        "schemaVersion": 1,
        "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done",
        "label": "Cline reported done",
        "detail": null,
        "stopReason": "end_turn",
        "turnOutcome": "completed",
        "clineMessage": "Added percent_change(a, b) and its tests.",
        "error": null,
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1", "pinned": "us.anthropic.claude-opus-5-5"},
        "engine": {"name": "cline", "version": "3.0.71", "binarySha256": "cd".repeat(32)},
        "rules": [{"name": "00-my-rules.md", "sha256": "ef".repeat(32), "bytes": 2943}],
        "rulesPreflight": {"ran": true, "ms": 812},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": {"inputTokens": 1000, "outputTokens": 200, "cacheReadInputTokens": 30, "cacheWriteInputTokens": 4},
        "toolCalls": 41,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T19:02:11Z", "wallS": 3731, "promptS": 3690, "maxSilentGapS": 84},
        "git": {"baseSha": "1".repeat(40), "headSha": "2".repeat(40), "committedByHarness": true, "filesChanged": 12,
                "bundleUri": format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/bundle"), "bundleBytes": 123_456},
        "snapshots": {"count": 5, "lastAt": "2026-10-13T18:50:00Z"},
        "packages": {"enabled": true, "requests": 57, "bytes": 123_456_789,
                     "byPrefix": {"go": 0, "maven": 0, "npm": 10, "pypi": 40}, "statuses": {"200": 55, "404": 2}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.30.0"}
    })
}

fn summary() -> DirectSummary {
    serde_json::from_value(golden_summary()).unwrap()
}

#[test]
fn the_golden_summary_round_trips_byte_for_byte() {
    let s = summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.turn_outcome, TurnOutcome::Completed);
    assert!(s.git.committed_by_harness);
    assert_eq!(s.packages.statuses.get(&200), Some(&55));
    assert_eq!(serde_json::to_value(&s).unwrap(), golden_summary());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_summary_of_a_failed_setup_has_nulls_where_nothing_was_measured() {
    let v = json!({
        "schemaVersion": 1, "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "stopped_setup_failed", "label": "Cline did not start",
        "detail": "request.json sha256 differs", "stopReason": null, "turnOutcome": "failed",
        "clineMessage": null, "error": "request.json sha256 differs",
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1"},
        "engine": {"name": "cline"},
        "rules": [], "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 0},
        "nudge": {"requested": false, "honored": false},
        "usage": null, "toolCalls": 0,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T18:00:01Z", "wallS": 1},
        "git": {"committedByHarness": false, "filesChanged": 0},
        "snapshots": {"count": 0},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.30.0"}
    });
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    // Usage is null, never zeros standing in for it; unmeasured parts are absent, not zero.
    assert!(s.usage.is_none() && s.git.head_sha.is_none() && s.git.bundle_uri.is_none());
    assert!(s.model.pinned.is_none() && s.engine.version.is_none() && s.timing.prompt_s.is_none());
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn the_research_member_is_left_out_when_the_run_had_none_and_read_back_when_it_had() {
    // A run on a stage without research writes the bytes it wrote before; a run that used the lane carries the note.
    assert!(summary().research.is_none());
    assert!(
        serde_json::to_value(summary())
            .unwrap()
            .get("research")
            .is_none()
    );
    let golden: Value = serde_json::from_str(include_str!(
        "../../../fixtures/research/summary-research.json"
    ))
    .unwrap();
    let mut v = golden_summary();
    v["research"] = golden["research"].clone();
    let s: DirectSummary = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(s.research.as_ref().unwrap().counts.unwrap().searches, 2);
    assert_eq!(s.validate(), Ok(()));
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    // The note is small: it cannot be the member that pushes a summary over its cap.
    assert!(serde_json::to_vec(&golden["research"]).unwrap().len() < 400);
}

#[test]
fn a_reader_ignores_unknown_summary_keys() {
    let mut v = golden_summary();
    v["somethingNew"] = json!({"a": 1});
    v["git"]["alsoNew"] = json!(true);
    let s: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(s.validate(), Ok(()));
    assert_eq!(serde_json::to_value(&s).unwrap(), golden_summary());
}

#[test]
fn the_snapshot_failure_and_refused_counters_appear_only_when_not_zero() {
    let mut s = summary();
    s.snapshots.failures = 2;
    s.packages.refused = 3;
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["snapshots"]["failures"], 2);
    assert_eq!(v["packages"]["refused"], 3);
    let back: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
    let plain = serde_json::to_value(summary()).unwrap();
    assert!(plain["snapshots"].get("failures").is_none());
    assert!(plain["packages"].get("refused").is_none());
}

#[test]
fn validate_refuses_a_summary_the_contract_does_not_allow() {
    use DirectSummaryError as E;
    type Case = (&'static str, Box<dyn Fn(&mut DirectSummary)>, E);
    let cases: Vec<Case> = vec![
        (
            "schema version",
            Box::new(|s| s.schema_version = 2),
            E::SchemaVersion,
        ),
        (
            "agent",
            Box::new(|s| s.agent_id = "planner".into()),
            E::Identity,
        ),
        ("turn", Box::new(|s| s.turn = 257), E::Identity),
        (
            "label differs from the table",
            Box::new(|s| s.label = "Cline reported done and the tests passed".into()),
            E::LabelMismatch,
        ),
        (
            "detail too long",
            Box::new(|s| s.detail = Some("d".repeat(MAX_DIRECT_DETAIL_CHARS + 1))),
            E::DetailTooLong,
        ),
        (
            "message too long",
            Box::new(|s| s.cline_message = Some("m".repeat(MAX_CLINE_MESSAGE_CHARS + 1))),
            E::MessageTooLong,
        ),
        (
            "document too big",
            Box::new(|s| {
                s.rules = (0..10_000)
                    .map(|i| DirectRuleRef {
                        name: format!("{i:04}-{}.md", "n".repeat(60)),
                        sha256: "ab".repeat(32),
                        bytes: Some(1),
                    })
                    .collect();
            }),
            E::TooLarge,
        ),
    ];
    for (name, change, want) in cases {
        let mut s = summary();
        change(&mut s);
        assert_eq!(s.validate(), Err(want), "{name}");
    }
    // The limits themselves pass.
    let mut s = summary();
    s.detail = Some("d".repeat(MAX_DIRECT_DETAIL_CHARS));
    s.cline_message = Some("m".repeat(MAX_CLINE_MESSAGE_CHARS));
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn text_cutting_helpers_count_characters_not_bytes() {
    assert_eq!(direct::tail_chars("abcdef", 3), "def");
    assert_eq!(direct::tail_chars("abc", 8), "abc");
    assert_eq!(direct::head_chars("abcdef", 3), "abc");
    assert_eq!(direct::head_chars("abc", 8), "abc");
    // A multi-byte character is never split.
    assert_eq!(direct::tail_chars("ééééé", 2), "éé");
    assert_eq!(direct::head_chars("日本語です", 2), "日本");
    assert_eq!(direct::tail_chars("", 4), "");
    assert_eq!(direct::head_chars("abc", 0), "");
}

#[test]
fn the_evidence_the_packager_renders_is_taken_from_the_summary() {
    let s = summary();
    let e = s.to_evidence(HeadSource::Final);
    assert_eq!(e.outcome, DirectOutcome::ReportedDone);
    assert_eq!(e.label, "Cline reported done");
    assert_eq!(e.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        e.cline_message.as_deref(),
        Some("Added percent_change(a, b) and its tests.")
    );
    assert_eq!(e.model_id, "us.anthropic.claude-opus-5-5");
    assert_eq!(e.engine_version.as_deref(), Some("3.0.71"));
    assert_eq!(e.rules.len(), 1);
    assert_eq!(e.rules[0].name, "00-my-rules.md");
    assert_eq!(e.wall_s, Some(3731));
    assert_eq!(e.usage, s.usage);
    assert_eq!(e.head_source, HeadSource::Final);
    assert_eq!(
        s.to_evidence(HeadSource::Snapshot).head_source,
        HeadSource::Snapshot
    );
}

#[test]
fn direct_evidence_round_trips_and_leaves_unset_members_out() {
    let e = DirectEvidence {
        outcome: DirectOutcome::StoppedSilence,
        label: DirectOutcome::StoppedSilence.label().into(),
        detail: Some("no output for 900 s".into()),
        stop_reason: None,
        cline_message: None,
        model_id: "us.anthropic.claude-sonnet-5-5".into(),
        engine_version: None,
        rules: vec![],
        usage: None,
        wall_s: None,
        head_source: HeadSource::Snapshot,
        final_model_id: None,
        model_switches: 0,
        test_report: None,
        review: None,
        flow: None,
        engine: None,
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(
        v,
        json!({
            "outcome": "stopped_silence",
            "label": "Cline stopped: no output for the silence limit",
            "detail": "no output for 900 s",
            "modelId": "us.anthropic.claude-sonnet-5-5",
            "rules": [],
            "headSource": "snapshot"
        })
    );
    assert_eq!(serde_json::from_value::<DirectEvidence>(v).unwrap(), e);
    // The full form.
    let full = DirectEvidence {
        usage: Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 4,
        }),
        wall_s: Some(10),
        engine_version: Some("3.0.71".into()),
        stop_reason: Some("end_turn".into()),
        cline_message: Some("done".into()),
        rules: vec![DirectRuleRef {
            name: "a.md".into(),
            sha256: "ab".repeat(32),
            bytes: None,
        }],
        head_source: HeadSource::Final,
        ..e
    };
    let v = serde_json::to_value(&full).unwrap();
    assert_eq!(v["headSource"], "final");
    assert_eq!(
        v["rules"][0],
        json!({"name": "a.md", "sha256": "ab".repeat(32)})
    );
    assert_eq!(v["usage"]["cacheWriteInputTokens"], 4);
    assert_eq!(serde_json::from_value::<DirectEvidence>(v).unwrap(), full);
}

#[test]
fn head_source_has_two_wire_words() {
    assert_eq!(
        serde_json::to_value(HeadSource::Final).unwrap(),
        json!("final")
    );
    assert_eq!(
        serde_json::to_value(HeadSource::Snapshot).unwrap(),
        json!("snapshot")
    );
    assert!(serde_json::from_value::<HeadSource>(json!("other")).is_err());
}

// ---- run.completed (C0-2) --------------------------------------------------------------------------------------------

#[test]
fn run_completed_of_the_old_line_keeps_its_bytes() {
    let old = json!({"status": "complete"});
    let p: RunCompletedPayload = serde_json::from_value(old.clone()).unwrap();
    assert!(p.outcome.is_none() && p.label.is_none() && p.detail.is_none());
    assert_eq!(serde_json::to_value(&p).unwrap(), old);
    assert_eq!(
        serde_json::to_value(RunCompletedPayload {
            status: None,
            outcome: None,
            label: None,
            detail: None,
            observations: vec![]
        })
        .unwrap(),
        json!({})
    );
}

#[test]
fn run_completed_of_a_direct_run_carries_the_outcome_and_the_label() {
    let v = json!({
        "status": "complete",
        "outcome": "stopped_silence",
        "label": "Cline stopped: no output for the silence limit",
        "detail": "no output for 900 s"
    });
    let p: RunCompletedPayload = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(p.outcome, Some(DirectOutcome::StoppedSilence));
    assert_eq!(
        p.label.as_deref(),
        Some(DirectOutcome::StoppedSilence.label())
    );
    assert_eq!(serde_json::to_value(&p).unwrap(), v);
    // A blocked run may carry no outcome at all.
    let b: RunCompletedPayload = serde_json::from_value(json!({"status": "blocked"})).unwrap();
    assert!(b.outcome.is_none());
}

// ---- S3 keys (section 5.1) -----------------------------------------------------------------------------------------------

#[test]
fn the_s3_keys_of_one_direct_run_are_the_contract_keys() {
    use harness_protocol::direct::s3;
    let t = format!("runs/{RUN}/agents/direct/turns/1/");
    assert_eq!(s3::turn_prefix(RUN), t);
    assert_eq!(s3::bundle_key(RUN), format!("{t}bundle"));
    assert_eq!(s3::summary_key(RUN), format!("{t}summary.json"));
    assert_eq!(s3::snapshot_bundle_key(RUN), format!("{t}snapshot.bundle"));
    assert_eq!(s3::snapshot_json_key(RUN), format!("{t}snapshot.json"));
    assert_eq!(s3::acp_transcript_key(RUN), format!("{t}acp.ndjson.gz"));
    assert_eq!(
        s3::evidence_key(RUN),
        format!("runs/{RUN}/evidence/direct.json")
    );
    // The inputs are the existing keys of the run.
    assert_eq!(
        s3::request_key(RUN),
        format!("runs/{RUN}/input/request.json")
    );
    assert_eq!(
        s3::repo_bundle_key(RUN),
        format!("runs/{RUN}/input/repo.bundle")
    );
    // The bundle key is the one worker-api pins and the store helper builds.
    assert_eq!(
        s3::bundle_key(RUN),
        format!(
            "{}bundle",
            harness_protocol::store::s3::turn_prefix(RUN, "direct", 1)
        )
    );
    // Everything the session writes stays under the prefix the worker-session role may write.
    for k in [
        s3::bundle_key(RUN),
        s3::summary_key(RUN),
        s3::snapshot_bundle_key(RUN),
        s3::snapshot_json_key(RUN),
        s3::acp_transcript_key(RUN),
    ] {
        assert!(k.starts_with(&format!("runs/{RUN}/agents/direct/")), "{k}");
    }
}

#[test]
fn s3_uris_are_made_and_split_without_guessing() {
    use harness_protocol::direct::s3;
    let uri = s3::uri(BUCKET, &s3::bundle_key(RUN));
    assert_eq!(
        uri,
        format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/bundle")
    );
    assert_eq!(
        s3::split_uri(&uri),
        Some((
            BUCKET,
            format!("runs/{RUN}/agents/direct/turns/1/bundle").as_str()
        ))
    );
    for bad in [
        "",
        "s3://",
        "s3://bucket",
        "s3://bucket/",
        "s3:///key",
        "http://bucket/key",
        "s3://Bad_Bucket/key",
        "s3://ab/key",
    ] {
        assert_eq!(s3::split_uri(bad), None, "{bad:?}");
    }
}

// ---- schemas (C0-3) -------------------------------------------------------------------------------------------------------

fn schema(file: &str) -> Value {
    all_schemas()
        .into_iter()
        .find(|(n, _)| *n == file)
        .unwrap()
        .1
}

#[test]
fn the_runtime_schema_carries_the_direct_types() {
    let s = schema("runtime.schema.json");
    let defs = s["$defs"].as_object().unwrap();
    for name in [
        "DirectInvocationRequest",
        "DirectModelSpec",
        "InputRef",
        "DirectSummary",
        "DirectEvidence",
        "DirectOutcome",
        "HeadSource",
    ] {
        assert!(defs.contains_key(name), "runtime.schema.json lacks {name}");
    }
    // Refusing unknown members is part of the contract, so the schema says so.
    for name in ["DirectInvocationRequest", "DirectModelSpec", "InputRef"] {
        assert_eq!(defs[name]["additionalProperties"], json!(false), "{name}");
    }
    // The summary is read leniently.
    assert_ne!(defs["DirectSummary"]["additionalProperties"], json!(false));
}

#[test]
fn the_outcome_enum_in_the_schemas_is_the_table() {
    let mut want: Vec<&str> = OUTCOMES.iter().map(|(_, w, _)| *w).collect();
    want.sort_unstable();
    for file in ["runtime.schema.json", "orchestration.schema.json"] {
        let s = schema(file);
        let def = &s["$defs"]["DirectOutcome"];
        let mut got: Vec<String> = def["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{file}: DirectOutcome is not a string enum: {def}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        got.sort();
        assert_eq!(got, want, "{file}");
    }
}

#[test]
fn run_completed_in_the_orchestration_schema_has_the_three_optional_members() {
    let s = schema("orchestration.schema.json");
    let props = &s["$defs"]["RunCompletedPayload"]["properties"];
    for k in ["status", "outcome", "label", "detail"] {
        assert!(props.get(k).is_some(), "RunCompletedPayload lacks {k}");
    }
    let required = &s["$defs"]["RunCompletedPayload"]["required"];
    assert!(required.is_null() || required.as_array().unwrap().is_empty());
}

#[test]
fn a_stop_by_the_harness_carries_the_label_its_detail_gives_and_the_old_text_no_longer_matches() {
    let mut s = summary();
    s.outcome = DirectOutcome::StoppedOther;
    s.detail = Some("The harness ended the run: 3 rounds in a row changed no file.".into());
    // the frozen text of the outcome alone is wrong for this detail
    s.label = DirectOutcome::StoppedOther.label().into();
    assert_eq!(s.validate(), Err(DirectSummaryError::LabelMismatch));
    s.label = DirectOutcome::StoppedOther.label_for(s.detail.as_deref());
    assert_eq!(
        s.label,
        "Cline stopped: the harness ended the run (3 rounds in a row changed no file)"
    );
    assert_eq!(s.validate(), Ok(()));
    // any other detail of stopped_other keeps the frozen text
    s.detail = Some("silence 900 s".into());
    assert_eq!(s.validate(), Err(DirectSummaryError::LabelMismatch));
    s.label = DirectOutcome::StoppedOther.label().into();
    assert_eq!(s.validate(), Ok(()));
}

// ---- observations: facts the harness read and records as it found them ----------------------------------------

#[test]
fn a_red_last_log_entry_is_an_observation_with_plain_fields_and_no_claim() {
    let o = DirectObservation::last_log_entry_red("pytest -q", "1", Some("2"));
    assert_eq!(
        serde_json::to_value(&o).unwrap(),
        json!({"kind": "last_log_entry_red", "command": "pytest -q", "exit": "1", "failed": "2",
            "text": "The last entry of the run log is a red run: pytest -q exited 1 and 2 failed."})
    );
    assert_eq!(o.kind, DirectObservationKind::LastLogEntryRed);
    // no failed count above zero: it is left out of the text and of the members
    for failed in [None, Some("0"), Some("unknown")] {
        let o = DirectObservation::last_log_entry_red("make test", "2", failed);
        assert_eq!(
            o.text,
            "The last entry of the run log is a red run: make test exited 2."
        );
        assert_eq!(o.failed, None, "{failed:?}");
    }
    // a run with exit 0 and failed tests is red too
    let o = DirectObservation::last_log_entry_red("pytest -q", "0", Some("3"));
    assert_eq!(
        o.text,
        "The last entry of the run log is a red run: pytest -q exited 0 and 3 failed."
    );
}

#[test]
fn an_observation_never_carries_a_claim_word_a_control_character_or_a_long_command() {
    for word in ["verified", "tested", "gated", "evaluated", "Verified"] {
        let o = DirectObservation::last_log_entry_red(
            &format!("pytest tests/test_{word}.py"),
            "1",
            None,
        );
        assert_eq!(o.command, None, "{word}");
        assert_eq!(
            o.text,
            "The last entry of the run log is a red run: exit 1."
        );
        assert_eq!(direct::banned_claim_word(&o.text), None);
    }
    let o = DirectObservation::last_log_entry_red("pytest\n-q\u{1b}[31m  -x", "1", None);
    assert_eq!(o.command.as_deref(), Some("pytest -q [31m -x"));
    let o = DirectObservation::last_log_entry_red(&"a ".repeat(200), "1", None);
    assert!(
        o.command.as_deref().unwrap().chars().count() <= 121,
        "{:?}",
        o.command
    );
    assert!(
        o.text.chars().count() <= MAX_OBSERVATION_CHARS,
        "{}",
        o.text
    );
    // the exit code and the count are plain tokens or they are not carried
    let o = DirectObservation::last_log_entry_red("pytest -q", "1 2", Some("two failures"));
    assert_eq!(
        (o.exit.as_deref(), o.failed.as_deref()),
        (Some("unknown"), None)
    );
}

#[test]
fn a_summary_with_observations_round_trips_and_one_without_keeps_its_bytes() {
    let mut s = summary();
    assert!(s.observations.is_empty());
    assert!(
        serde_json::to_value(&s)
            .unwrap()
            .get("observations")
            .is_none()
    );
    s.observations = vec![DirectObservation::last_log_entry_red(
        "pytest -q",
        "1",
        Some("2"),
    )];
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["observations"][0]["kind"], "last_log_entry_red");
    let back: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
    assert_eq!(back.validate(), Ok(()));
}

#[test]
fn a_summary_whose_observations_say_too_much_or_claim_the_work_is_refused() {
    let one = DirectObservation::last_log_entry_red("pytest -q", "1", None);
    let mut s = summary();
    s.observations = vec![one.clone(); MAX_DIRECT_OBSERVATIONS + 1];
    assert_eq!(s.validate(), Err(DirectSummaryError::ObservationInvalid));
    s.observations = vec![one.clone(); MAX_DIRECT_OBSERVATIONS];
    assert_eq!(s.validate(), Ok(()));
    let mut long = one.clone();
    long.text = "x".repeat(MAX_OBSERVATION_CHARS + 1);
    s.observations = vec![long];
    assert_eq!(s.validate(), Err(DirectSummaryError::ObservationInvalid));
    let mut claim = one.clone();
    claim.text = "The last entry of the run log is a verified run.".into();
    s.observations = vec![claim];
    assert_eq!(s.validate(), Err(DirectSummaryError::ObservationInvalid));
    let mut claim = one;
    claim.command = Some("gated-release.sh".into());
    s.observations = vec![claim];
    assert_eq!(s.validate(), Err(DirectSummaryError::ObservationInvalid));
}

#[test]
fn run_completed_carries_the_observations_when_there_are_some_and_nothing_when_there_are_none() {
    let none = RunCompletedPayload {
        status: None,
        outcome: Some(DirectOutcome::ReportedDone),
        label: Some("Cline reported done".into()),
        detail: None,
        observations: vec![],
    };
    assert!(
        serde_json::to_value(&none)
            .unwrap()
            .get("observations")
            .is_none()
    );
    let some = RunCompletedPayload {
        observations: vec![DirectObservation::last_log_entry_red(
            "pytest -q",
            "1",
            Some("2"),
        )],
        ..none.clone()
    };
    let v = serde_json::to_value(&some).unwrap();
    assert_eq!(v["observations"][0]["exit"], "1");
    let back: RunCompletedPayload = serde_json::from_value(v).unwrap();
    assert_eq!(back, some);
    // an event written before the member existed reads with none
    let old: RunCompletedPayload =
        serde_json::from_value(json!({"outcome": "reported_done", "label": "Cline reported done"}))
            .unwrap();
    assert!(old.observations.is_empty());
}

use harness_protocol::direct::DirectEngineConfigRef;

// ---- the engine settings of the run -------------------------------

#[test]
fn the_summary_leaves_the_engine_config_member_out_when_there_is_none() {
    let s = summary();
    assert!(s.engine_config.is_none());
    assert!(
        serde_json::to_value(&s)
            .unwrap()
            .get("engineConfig")
            .is_none()
    );
    // the golden of protocol 1.30.0 still round-trips byte for byte
    assert_eq!(serde_json::to_value(&s).unwrap(), golden_summary());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn the_summary_carries_the_engine_config_reference_without_values_and_validate_checks_it() {
    let mut s = summary();
    let reference = DirectEngineConfigRef {
        engine: "cline".into(),
        schema: 1,
        sha256: "ab".repeat(32),
        keys: 3,
    };
    s.engine_config = Some(reference.clone());
    assert_eq!(s.validate(), Ok(()));
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(
        v["engineConfig"],
        json!({"engine": "cline", "schema": 1, "sha256": "ab".repeat(32), "keys": 3})
    );
    let back: DirectSummary = serde_json::from_value(v).unwrap();
    assert_eq!(back.engine_config, Some(reference.clone()));

    let broken = |edit: &dyn Fn(&mut DirectEngineConfigRef)| {
        let mut r = reference.clone();
        edit(&mut r);
        let mut s = summary();
        s.engine_config = Some(r);
        s.validate()
    };
    for (what, edit) in [
        (
            "upper-case hex",
            Box::new(|r: &mut DirectEngineConfigRef| r.sha256 = "AB".repeat(32))
                as Box<dyn Fn(&mut DirectEngineConfigRef)>,
        ),
        ("63 digits", Box::new(|r| r.sha256 = "a".repeat(63))),
        ("65 digits", Box::new(|r| r.sha256 = "a".repeat(65))),
        ("empty", Box::new(|r| r.sha256 = String::new())),
        ("not hex", Box::new(|r| r.sha256 = "g".repeat(64))),
        ("too many keys", Box::new(|r| r.keys = 65)),
        ("schema 0", Box::new(|r| r.schema = 0)),
        (
            "not an engine name",
            Box::new(|r| r.engine = "Cline".into()),
        ),
        ("empty engine", Box::new(|r| r.engine = String::new())),
    ] {
        assert_eq!(
            broken(&*edit),
            Err(DirectSummaryError::EngineConfigInvalid),
            "{what}"
        );
    }
}

// ---- the tool server's note in the summary (protocol 1.40.0, wire B of the tool layer, trimmed to code_search) ----------

use harness_protocol::direct::DirectToolsNote;

fn tools_note_json() -> Value {
    json!({
        "enabled": true,
        "port": 3130,
        "connect": "ok",
        "list": ["code_search"],
        "counts": {
            "calls": {"code_search": 3},
            "failed": 0, "refused": 1, "truncated": 1, "restarts": 0,
            "p50Ms": 181, "p95Ms": 318
        }
    })
}

fn with_tools(note: Value) -> DirectSummary {
    let mut v = golden_summary();
    v["tools"] = note;
    serde_json::from_value(v).unwrap()
}

#[test]
fn old_summaries_parse_and_the_tools_member_is_left_out_when_none() {
    let s = summary();
    assert!(s.tools.is_none());
    let v = serde_json::to_value(&s).unwrap();
    assert!(
        v.get("tools").is_none(),
        "a run on a stage without the layer writes the bytes it wrote before"
    );
    assert_eq!(v, golden_summary());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn the_tools_note_round_trips_byte_for_byte_and_stays_small() {
    let s = with_tools(tools_note_json());
    let note = s.tools.as_ref().unwrap();
    assert!(note.enabled);
    assert_eq!(note.port, Some(3130));
    assert_eq!(note.connect.as_deref(), Some("ok"));
    assert_eq!(note.list, ["code_search"]);
    let counts = note.counts.as_ref().unwrap();
    assert_eq!(counts.calls.get("code_search"), Some(&3));
    assert_eq!(
        (
            counts.failed,
            counts.refused,
            counts.truncated,
            counts.restarts
        ),
        (0, 1, 1, 0)
    );
    assert_eq!((counts.p50_ms, counts.p95_ms), (Some(181), Some(318)));
    assert_eq!(
        serde_json::to_value(&s).unwrap()["tools"],
        tools_note_json()
    );
    assert_eq!(s.validate(), Ok(()));
    // Counts and words only, so it cannot be the member that pushes a summary over its cap.
    assert!(serde_json::to_vec(&tools_note_json()).unwrap().len() < 300);
}

#[test]
fn a_note_of_a_module_that_stayed_off_carries_its_reason_and_no_counts() {
    let off = json!({"enabled": false, "reason": "no-mcp-table"});
    let s = with_tools(off.clone());
    let note = s.tools.as_ref().unwrap();
    assert!(!note.enabled && note.counts.is_none() && note.list.is_empty());
    assert_eq!(note.reason.as_deref(), Some("no-mcp-table"));
    assert_eq!(serde_json::to_value(&s).unwrap()["tools"], off);
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_run_without_a_call_has_no_latency_instead_of_a_zero() {
    // "A zero would claim the server answered instantly."
    let idle = json!({
        "enabled": true, "port": 3130, "connect": "none", "list": ["code_search"],
        "counts": {"calls": {}, "failed": 0, "refused": 0, "truncated": 0, "restarts": 0}
    });
    let s = with_tools(idle.clone());
    let counts = s.tools.as_ref().unwrap().counts.as_ref().unwrap();
    assert_eq!((counts.p50_ms, counts.p95_ms), (None, None));
    assert_eq!(serde_json::to_value(&s).unwrap()["tools"], idle);
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_reader_takes_a_connect_word_and_a_tool_it_does_not_know_yet() {
    // The summary is read by components that may be older than the agentd that wrote it. An unknown enum value would fail the whole parse and
    // lose the run's record, so the words are plain strings and validate() checks their form, not a list.
    let mut note = tools_note_json();
    note["connect"] = json!("reconnected");
    note["list"] = json!(["code_search", "web_search"]);
    note["counts"]["calls"] = json!({"code_search": 3, "web_search": 1});
    let s = with_tools(note.clone());
    assert_eq!(s.validate(), Ok(()));
    assert_eq!(serde_json::to_value(&s).unwrap()["tools"], note);
}

#[test]
fn a_reader_ignores_unknown_members_of_the_note() {
    let mut note = tools_note_json();
    note["somethingNew"] = json!(1);
    note["counts"]["alsoNew"] = json!([1, 2]);
    let s = with_tools(note);
    assert_eq!(
        serde_json::to_value(&s).unwrap()["tools"],
        tools_note_json()
    );
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn validate_refuses_a_tools_note_the_contract_does_not_allow() {
    type Edit = Box<dyn Fn(&mut DirectToolsNote)>;
    let base = || with_tools(tools_note_json()).tools.unwrap();
    let cases: Vec<(&str, Edit)> = vec![
        (
            "reason in capitals",
            Box::new(|n| n.reason = Some("No-Table".into())),
        ),
        (
            "reason with a space",
            Box::new(|n| n.reason = Some("no table".into())),
        ),
        ("empty reason", Box::new(|n| n.reason = Some(String::new()))),
        (
            "reason of 41 characters",
            Box::new(|n| n.reason = Some("a".repeat(41))),
        ),
        ("port 0", Box::new(|n| n.port = Some(0))),
        (
            "connect in capitals",
            Box::new(|n| n.connect = Some("OK".into())),
        ),
        (
            "connect with a space",
            Box::new(|n| n.connect = Some("hand shake".into())),
        ),
        (
            "empty connect",
            Box::new(|n| n.connect = Some(String::new())),
        ),
        (
            "a prefixed name in list",
            Box::new(|n| n.list.push("ham__code_search".into())),
        ),
        (
            "a name with a hyphen",
            Box::new(|n| n.list.push("code-search".into())),
        ),
        (
            "a repeated name",
            Box::new(|n| n.list = vec!["code_search".into(), "code_search".into()]),
        ),
        (
            "17 names",
            Box::new(|n| n.list.extend((1..17).map(|i| format!("t{i}")))),
        ),
        (
            "a call counted for a tool that was never listed",
            Box::new(|n| {
                n.counts
                    .as_mut()
                    .unwrap()
                    .calls
                    .insert("invented_tool".into(), 1);
            }),
        ),
        (
            "a prefixed key in calls",
            Box::new(|n| {
                n.counts.as_mut().unwrap().calls.clear();
                n.counts
                    .as_mut()
                    .unwrap()
                    .calls
                    .insert("ham__code_search".into(), 1);
            }),
        ),
        (
            "counts on a module that was off",
            Box::new(|n| n.enabled = false),
        ),
        (
            "p50 above p95",
            Box::new(|n| {
                let c = n.counts.as_mut().unwrap();
                c.p50_ms = Some(400);
                c.p95_ms = Some(300);
            }),
        ),
    ];
    for (what, edit) in cases {
        let mut note = base();
        edit(&mut note);
        let mut s = summary();
        s.tools = Some(note);
        assert_eq!(
            s.validate(),
            Err(DirectSummaryError::ToolsInvalid),
            "{what}"
        );
    }
    let mut ok = summary();
    ok.tools = Some(base());
    assert_eq!(ok.validate(), Ok(()));
}
