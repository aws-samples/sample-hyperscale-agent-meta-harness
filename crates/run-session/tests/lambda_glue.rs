//! The Lambda's inputs and its log line: a Step Functions task payload is an op, an EventBridge status change is
//! `run-ended`, and nothing in the log line carries a token, a prompt or a credential.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::utility::UtilityPurpose;
use harness_protocol::goal::GoalStatus;
use run_session::lambda::{
    PurposesSetting, UtilitySetting, config_from_lookup, purposes_from_lookup, serve_event,
    utility_from_lookup,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use support::fakes::*;

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn full_env() -> BTreeMap<String, String> {
    env(&[
        ("HAMH_STAGE", "direct"),
        ("BUCKET_NAME", BUCKET),
        (
            "INTERNAL_API_URL",
            "https://abc.execute-api.us-east-1.amazonaws.com/direct",
        ),
        ("TENANT", "testuser"),
        ("WORKER_MAX_LIFETIME_S", "28800"),
        ("RUN_STATE_MACHINE_ARN", MACHINE),
        ("AWS_REGION", "us-east-1"),
    ])
}

#[test]
fn the_configuration_comes_from_the_environment_and_only_the_direct_stage_runs_it() {
    let e = full_env();
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert_eq!(c.bucket, BUCKET);
    assert_eq!(c.region, "us-east-1");
    assert_eq!(c.runtime_arn, RUNTIME);
    assert_eq!(c.max_lifetime_s, 28_800);
    assert_eq!(c.run_state_machine_arn, MACHINE);
    for (k, v) in [("HAMH_STAGE", "live"), ("HAMH_STAGE", "drill")] {
        let mut e = full_env();
        e.insert(k.into(), v.into());
        assert!(
            config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into())
                .unwrap_err()
                .contains("must be direct")
        );
    }
    for missing in [
        "BUCKET_NAME",
        "INTERNAL_API_URL",
        "TENANT",
        "WORKER_MAX_LIFETIME_S",
        "RUN_STATE_MACHINE_ARN",
        "HAMH_STAGE",
    ] {
        let mut e = full_env();
        e.remove(missing);
        let err = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap_err();
        assert!(err.contains(missing), "{err}");
    }
    let mut e = full_env();
    e.insert("WORKER_MAX_LIFETIME_S".into(), "eight hours".into());
    assert!(config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).is_err());
}

#[tokio::test]
async fn an_op_payload_runs_the_op_and_logs_op_run_and_outcome_only() {
    let mem = Mem::default();
    mem.seed(repo_target(), "a secret prompt", MODEL);
    let (r, log) = serve_event(&mem.deps(), json!({"op": "prepare", "runId": RUN})).await;
    assert_eq!(r.unwrap()["repo"], "hamh-e2e-python-calc");
    let line: Value = serde_json::from_str(&log.line()).unwrap();
    assert_eq!(
        line,
        json!({"function": "run-session", "op": "prepare", "runId": RUN, "outcome": "ok"})
    );
}

#[tokio::test]
async fn a_failure_logs_the_error_type_and_the_reason_but_never_the_token() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", "us.mistral.large-9");
    let (r, log) = serve_event(
        &mem.deps(),
        json!({"op": "start-session", "runId": RUN, "taskToken": "SECRET-TASK-TOKEN", "staged": staged()}),
    )
    .await;
    assert_eq!(r.unwrap_err().error_type(), "RunSessionInvalid");
    let line = log.line();
    assert!(
        line.contains("RunSessionInvalid") && line.contains("start-session"),
        "{line}"
    );
    assert!(!line.contains("SECRET-TASK-TOKEN"), "{line}");
}

#[tokio::test]
async fn an_input_that_is_no_op_is_invalid_and_does_not_retry() {
    let mem = Mem::default();
    for input in [
        json!({"op": "nope", "runId": RUN}),
        json!({"runId": RUN}),
        json!({"op": "prepare"}),
        json!([1, 2]),
    ] {
        let (r, log) = serve_event(&mem.deps(), input.clone()).await;
        assert_eq!(r.unwrap_err().error_type(), "RunSessionInvalid", "{input}");
        assert!(log.line().contains("RunSessionInvalid"));
    }
}

#[tokio::test]
async fn a_status_change_event_is_run_ended() {
    let mem = Mem::default();
    mem.seed(repo_target(), "p", MODEL);
    let event = json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": "ABORTED", "name": RUN, "stateMachineArn": MACHINE, "executionArn": "x", "stopDate": 1_790_000_100_000u64}});
    let (r, log) = serve_event(&mem.deps(), event).await;
    assert_eq!(r.unwrap()["cleared"], true);
    assert_eq!(mem.run().status, GoalStatus::Cleared);
    assert!(log.line().contains("run-ended"));
    // another machine's end is refused
    let other = json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": "ABORTED", "name": RUN, "stateMachineArn": "arn:aws:states:us-east-1:700000000011:stateMachine:hamh-live-run"}});
    let (r, _) = serve_event(&mem.deps(), other).await;
    assert_eq!(r.unwrap_err().error_type(), "RunSessionInvalid");
}

// ---- UTILITY_MODEL: the model of the helper calls --------------------------------------------------------------------

const HAIKU: &str = "us.anthropic.claude-haiku-5-5";

fn setting(value: Option<&str>, region: &str) -> UtilitySetting {
    let mut e = full_env();
    if let Some(v) = value {
        e.insert("UTILITY_MODEL".into(), v.into());
    }
    utility_from_lookup(&|k| e.get(k).cloned(), region)
}

/// The deploy resolves the catalog default into the variable (`infra/lib/config.ts`), so inside the function an absent or blank value
/// means the helper calls are off, as does `off`. A value that names no usable model also leaves them off, with one log line that
/// names the setting; the function still starts, because a naming fault must not stop a run.
#[test]
fn utility_model_env() {
    for off in [
        None,
        Some(""),
        Some("   "),
        Some("off"),
        Some("OFF"),
        Some(" Off "),
    ] {
        let s = setting(off, "us-east-1");
        assert!(s.model.is_none(), "{off:?}");
        assert!(s.warning.is_none(), "{off:?}: off is a choice, not a fault");
    }
    for bad in [
        "sonnet",
        "nope",
        "us.anthropic.claude-sonnet-5-5",
        "haiku-9",
    ] {
        let s = setting(Some(bad), "us-east-1");
        assert!(s.model.is_none(), "{bad}");
        let warning = s.warning.unwrap_or_else(|| panic!("{bad}: no warning"));
        assert_eq!(warning.lines().count(), 1, "one line: {warning}");
        let v: Value = serde_json::from_str(&warning).unwrap();
        assert_eq!(v["function"], "run-session");
        assert_eq!(v["event"], "utility-model-invalid");
        assert!(
            v["detail"].as_str().unwrap().contains("UTILITY_MODEL"),
            "{v}"
        );
    }
    for good in [HAIKU, "haiku", " HAIKU "] {
        let s = setting(Some(good), "us-east-1");
        assert_eq!(
            s.model.as_ref().map(|m| m.id.as_str()),
            Some(HAIKU),
            "{good:?}"
        );
        assert!(s.warning.is_none(), "{good:?}");
    }
    // A row that runs in another Region than the stage's is not used (the deploy refuses it too).
    let s = setting(Some(HAIKU), "us-west-2");
    assert!(s.model.is_none());
    let warning = s.warning.unwrap();
    assert!(
        warning.contains("us-east-1") && warning.contains("us-west-2"),
        "{warning}"
    );
}

#[test]
fn the_configuration_carries_the_utility_setting_and_a_bad_one_does_not_stop_the_function() {
    let mut e = full_env();
    e.insert("UTILITY_MODEL".into(), HAIKU.into());
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert_eq!(c.utility.as_ref().map(|m| m.id.as_str()), Some(HAIKU));

    let c = config_from_lookup(|k| full_env().get(k).cloned(), RUNTIME.into()).unwrap();
    assert!(c.utility.is_none());

    e.insert("UTILITY_MODEL".into(), "nope".into());
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert!(
        c.utility.is_none(),
        "the function starts with the helper calls off"
    );
}

// ---- UTILITY_PURPOSES: which helper functions the utility model serves ---------------------------------------------------------

fn purposes(value: Option<&str>) -> PurposesSetting {
    let mut e = full_env();
    if let Some(v) = value {
        e.insert("UTILITY_PURPOSES".into(), v.into());
    }
    purposes_from_lookup(&|k| e.get(k).cloned())
}

/// Absent or blank means the name alone (a function deployed before the tags keeps its behaviour). A list names the purposes it serves;
/// case and blanks around a word do not matter, an empty piece (a trailing comma) is ignored, and a repeated word counts once. A word the
/// build does not know makes the whole value invalid: one line says so, naming the variable, and the function serves the name alone
/// (tags off), because it must start and a run does not depend on its tags. The deploy sets the value `off` when the utility model is
/// switched off (nothing is called then, so the purposes do not matter): the whole value `off`, after blanks and in any case, is the same
/// as an absent value and prints no line; `off` inside a list is still a word the build does not know.
#[test]
fn utility_purposes_env() {
    use UtilityPurpose::{RunName, RunTags};
    for (value, want) in [
        (None, vec![RunName]),
        (Some(""), vec![RunName]),
        (Some("   "), vec![RunName]),
        (Some(","), vec![RunName]),
        (Some("run-name"), vec![RunName]),
        (Some("run-name,run-tags"), vec![RunName, RunTags]),
        (Some("run-tags,run-name"), vec![RunName, RunTags]),
        (Some(" Run-Tags , RUN-NAME "), vec![RunName, RunTags]),
        (Some("run-name,,run-tags,"), vec![RunName, RunTags]),
        (Some("run-tags,run-tags"), vec![RunTags]),
        (Some("run-tags"), vec![RunTags]),
        (Some("off"), vec![RunName]),
        (Some(" OFF "), vec![RunName]),
        (Some("Off"), vec![RunName]),
    ] {
        let s = purposes(value);
        assert_eq!(s.purposes, want, "{value:?}");
        assert!(s.warning.is_none(), "{value:?}: {:?}", s.warning);
    }
    for bad in [
        "nope",
        "run-name,nope",
        "run-tags,tags",
        "run_tags",
        "run-name run-tags",
        "off,run-tags",
        "run-tags,off",
        "off,off",
        "off,",
        "of",
    ] {
        let s = purposes(Some(bad));
        assert_eq!(s.purposes, vec![RunName], "{bad}: fail closed to the name");
        let warning = s.warning.unwrap_or_else(|| panic!("{bad}: no warning"));
        assert_eq!(warning.lines().count(), 1, "one line: {warning}");
        let v: Value = serde_json::from_str(&warning).unwrap();
        assert_eq!(v["function"], "run-session");
        assert_eq!(v["event"], "utility-purposes-invalid");
        assert!(
            v["detail"].as_str().unwrap().contains("UTILITY_PURPOSES"),
            "{v}"
        );
    }
    // The warning is bounded however long the value is.
    let long = "x".repeat(5000);
    assert!(purposes(Some(&long)).warning.unwrap().len() < 400);
}

#[test]
fn the_configuration_carries_the_purposes_and_a_bad_value_does_not_stop_the_function() {
    use UtilityPurpose::{RunName, RunTags};
    let mut e = full_env();
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert_eq!(c.utility_purposes, vec![RunName]);
    assert!(c.serves(RunName) && !c.serves(RunTags));

    e.insert("UTILITY_PURPOSES".into(), "run-name,run-tags".into());
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert_eq!(c.utility_purposes, vec![RunName, RunTags]);
    assert!(c.serves(RunTags));

    e.insert("UTILITY_PURPOSES".into(), "nope".into());
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert_eq!(
        c.utility_purposes,
        vec![RunName],
        "the function starts with the tags off"
    );

    // The value the deploy sets when the utility model is off.
    e.insert("UTILITY_PURPOSES".into(), "off".into());
    let c = config_from_lookup(|k| e.get(k).cloned(), RUNTIME.into()).unwrap();
    assert_eq!(c.utility_purposes, vec![RunName]);
}
