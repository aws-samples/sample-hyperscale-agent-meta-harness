//! A run that meets "Insufficient EC2 capacity" ends with a plain message that names the cause and the two ways out. Two test-stage runs with
//! `--compute instances` ended blocked with `InvokeAgentRuntime: service error: RuntimeClientError: Insufficient EC2 capacity` and nothing else: no
//! word that EC2 had no machine, that a later start may get one, or that a run without `--compute instances` does not need one.

#![allow(clippy::unwrap_used)]

use run_session::finalize::{CAPACITY_REASON, blocked_reason};
use serde_json::{Value, json};

const SERVICE: &str =
    "InvokeAgentRuntime: service error: RuntimeClientError: Insufficient EC2 capacity";

fn lambda_failure(message: &str) -> Value {
    json!({
        "Error": "RunSessionUnavailable",
        "Cause": json!({"errorType": "RunSessionUnavailable", "errorMessage": message}).to_string()
    })
}

#[test]
fn b815_a_failed_start_on_no_ec2_capacity_names_the_cause_and_both_ways_out() {
    let reason = blocked_reason(&json!({"failure": lambda_failure(SERVICE)}));
    assert!(reason.starts_with(CAPACITY_REASON), "{reason}");
    assert!(reason.contains("EC2 had no free capacity"), "{reason}");
    assert!(reason.contains("again later"), "{reason}");
    assert!(reason.contains("without --compute instances"), "{reason}");
    // The service's own words stay for an operator, after the plain sentences.
    assert!(
        reason.ends_with(&format!("The service said: {SERVICE}")),
        "{reason}"
    );
}

#[test]
fn b815_a_lost_session_whose_start_met_no_capacity_reads_the_same() {
    let state = json!({
        "session": {"failure": lambda_failure(SERVICE)},
        "plan": {"result": {"reason": "nothing was delivered"}}
    });
    let reason = blocked_reason(&state);
    assert!(reason.starts_with(CAPACITY_REASON), "{reason}");
    assert!(reason.contains(SERVICE), "{reason}");
}

#[test]
fn b815_the_ec2_error_code_reads_the_same() {
    let reason = blocked_reason(&json!({"failure": lambda_failure(
        "CreateCapacityProviderSession: InsufficientInstanceCapacity: We currently do not have sufficient m7g.4xlarge capacity"
    )}));
    assert!(reason.starts_with(CAPACITY_REASON), "{reason}");
}

#[test]
fn b815_another_failure_keeps_its_own_words() {
    let reason =
        blocked_reason(&json!({"failure": lambda_failure("the worker runtime is not deployed")}));
    assert_eq!(reason, "the worker runtime is not deployed");
}

#[test]
fn b815_the_plain_sentences_name_no_internal_word() {
    for word in [
        "RuntimeClientError",
        "InvokeAgentRuntime",
        "capacity provider",
        "m7g",
    ] {
        assert!(
            !CAPACITY_REASON.contains(word),
            "{word} in {CAPACITY_REASON}"
        );
    }
}
