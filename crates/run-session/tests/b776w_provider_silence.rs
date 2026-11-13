//! The infra lane: the 900 s without output after which the harness cancels a prompt is a stage setting, `HAMH_PROVIDER_SILENCE_S` (60 to 3,600 s; the CDK app
//! puts it on the run-session function). Unset leaves agentd's own 900 s: the invocation then carries no `providerSilenceS`, as before.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectInvocationRequest, validate_direct_invocation};
use run_session::lambda::provider_silence_from_lookup;
use run_session::ops::{Op, handle};
use serde_json::json;
use support::fakes::*;

fn lookup(value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |name: &str| {
        (name == "HAMH_PROVIDER_SILENCE_S")
            .then(|| value.map(String::from))
            .flatten()
    }
}

#[test]
fn b776w_the_silence_limit_is_left_alone_when_the_variable_is_absent_or_blank() {
    assert_eq!(provider_silence_from_lookup(&lookup(None)), Ok(None));
    assert_eq!(provider_silence_from_lookup(&lookup(Some(""))), Ok(None));
    assert_eq!(provider_silence_from_lookup(&lookup(Some("  "))), Ok(None));
}

#[test]
fn b776w_the_silence_limit_takes_sixty_to_thirty_six_hundred_seconds_at_both_ends() {
    for (text, want) in [("60", 60), ("900", 900), (" 1800 ", 1800), ("3600", 3600)] {
        assert_eq!(
            provider_silence_from_lookup(&lookup(Some(text))),
            Ok(Some(want)),
            "{text:?}"
        );
    }
}

#[test]
fn b776w_a_silence_limit_out_of_range_stops_the_function_and_names_the_variable() {
    for bad in ["0", "59", "3601", "-5", "15m", "1.5", "soon"] {
        let e = provider_silence_from_lookup(&lookup(Some(bad))).unwrap_err();
        assert!(
            e.starts_with("HAMH_PROVIDER_SILENCE_S must be whole seconds from 60 to 3600"),
            "{bad}: {e}"
        );
    }
}

#[tokio::test]
async fn b776w_a_stage_silence_limit_reaches_the_invocation_of_every_session() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.set_provider_silence(1_800);
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "token-A", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
    let sent = mem.invocations();
    assert_eq!(sent.len(), 1);
    let req: DirectInvocationRequest = serde_json::from_value(sent[0].clone()).unwrap();
    // The protocol's own check accepts the member, and the wire carries it under its name.
    validate_direct_invocation(&req).unwrap();
    assert_eq!(req.provider_silence_s, Some(1_800));
    assert_eq!(sent[0]["providerSilenceS"], 1_800);
}

#[tokio::test]
async fn b776w_without_the_setting_the_invocation_carries_no_silence_member() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "token-A", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap();
    assert!(mem.invocations()[0].get("providerSilenceS").is_none());
}
