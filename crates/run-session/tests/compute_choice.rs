//! Which machine a run's session starts on: the user's choice from the request, the stage's policy for `auto` and for a
//! request that says nothing, the stage settings from the function environment and the one constant a later decision changes.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{ComputeMode, ComputeOptions};
use run_session::compute::{
    Compute, ComputePolicy, ComputeStage, DEFAULT_COMPUTE_POLICY, InstancesParameters,
    choose_compute, from_lookup,
};
use std::collections::BTreeMap;

fn asked(mode: ComputeMode) -> Option<ComputeOptions> {
    Some(ComputeOptions {
        mode,
        container_signals: None,
    })
}

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

#[test]
fn an_explicit_word_wins_and_auto_and_no_member_follow_the_stage_policy() {
    for policy in [ComputePolicy::Microvm, ComputePolicy::Instances] {
        let follows = match policy {
            ComputePolicy::Microvm => Compute::Microvm,
            ComputePolicy::Instances => Compute::Instances,
        };
        assert_eq!(choose_compute(None, policy), follows);
        assert_eq!(choose_compute(asked(ComputeMode::Auto), policy), follows);
        assert_eq!(
            choose_compute(asked(ComputeMode::Microvm), policy),
            Compute::Microvm
        );
        assert_eq!(
            choose_compute(asked(ComputeMode::Instances), policy),
            Compute::Instances
        );
    }
}

#[test]
fn the_default_policy_is_one_constant_and_it_is_the_microvm_until_the_live_gate_changes_it() {
    // The default becomes `instances` after the live gate by editing this constant and this assertion.
    assert_eq!(DEFAULT_COMPUTE_POLICY, ComputePolicy::Microvm);
    assert_eq!(ComputeStage::default().policy, DEFAULT_COMPUTE_POLICY);
    assert_eq!(ComputeStage::default().max_active, None);
}

#[test]
fn the_words_round_trip() {
    for (word, c) in [
        ("microvm", Compute::Microvm),
        ("instances", Compute::Instances),
    ] {
        assert_eq!(c.word(), word);
        assert_eq!(Compute::from_word(word), Some(c));
    }
    assert_eq!(Compute::from_word("auto"), None);
    assert_eq!(Compute::from_word(""), None);
    for (word, p) in [
        ("microvm", ComputePolicy::Microvm),
        ("instances", ComputePolicy::Instances),
    ] {
        assert_eq!(ComputePolicy::parse(word), Some(p));
        assert_eq!(p.word(), word);
    }
    assert_eq!(ComputePolicy::parse("auto"), None);
    assert_eq!(ComputePolicy::parse("Instances"), None);
}

#[test]
fn no_variable_leaves_the_default_stage_and_no_instances_parameters() {
    let (stage, parameters) = from_lookup(env(&[])).unwrap();
    assert_eq!(stage, ComputeStage::default());
    assert_eq!(parameters, InstancesParameters::default());
}

#[test]
fn the_stage_reads_its_policy_cap_and_the_names_of_its_two_parameters() {
    let (stage, parameters) = from_lookup(env(&[
        ("COMPUTE_POLICY", "instances"),
        ("COMPUTE_MAX_ACTIVE", "6"),
        (
            "INSTANCES_RUNTIME_ARN_PARAMETER",
            "/hamh/direct/worker-python-instances-runtime-arn",
        ),
        (
            "CAPACITY_PROVIDER_ID_PARAMETER",
            "/hamh/direct/instances-capacity-provider-id",
        ),
    ]))
    .unwrap();
    assert_eq!(stage.policy, ComputePolicy::Instances);
    assert_eq!(stage.max_active, Some(6));
    assert_eq!(
        parameters,
        InstancesParameters {
            runtime_arn: Some("/hamh/direct/worker-python-instances-runtime-arn".into()),
            capacity_provider: Some("/hamh/direct/instances-capacity-provider-id".into()),
        }
    );
}

#[test]
fn a_bad_value_is_an_error_that_names_the_variable_and_never_the_function_start() {
    for (pairs, name) in [
        (vec![("COMPUTE_POLICY", "gpu")], "COMPUTE_POLICY"),
        (vec![("COMPUTE_POLICY", "auto")], "COMPUTE_POLICY"),
        (vec![("COMPUTE_MAX_ACTIVE", "0")], "COMPUTE_MAX_ACTIVE"),
        (vec![("COMPUTE_MAX_ACTIVE", "many")], "COMPUTE_MAX_ACTIVE"),
        (vec![("COMPUTE_MAX_ACTIVE", "-1")], "COMPUTE_MAX_ACTIVE"),
    ] {
        let e = from_lookup(env(&pairs)).unwrap_err();
        assert!(e.contains(name), "{pairs:?}: {e}");
    }
}

#[test]
fn an_instances_policy_needs_the_parameter_and_the_provider_by_name() {
    let e = from_lookup(env(&[("COMPUTE_POLICY", "instances")])).unwrap_err();
    assert!(
        e.contains("INSTANCES_RUNTIME_ARN_PARAMETER")
            || e.contains("CAPACITY_PROVIDER_ID_PARAMETER"),
        "{e}"
    );
    let e = from_lookup(env(&[
        ("COMPUTE_POLICY", "instances"),
        ("CAPACITY_PROVIDER_ID_PARAMETER", "/hamh/direct/q"),
    ]))
    .unwrap_err();
    assert!(e.contains("INSTANCES_RUNTIME_ARN_PARAMETER"), "{e}");
    let e = from_lookup(env(&[
        ("COMPUTE_POLICY", "instances"),
        ("INSTANCES_RUNTIME_ARN_PARAMETER", "/hamh/direct/p"),
    ]))
    .unwrap_err();
    assert!(e.contains("CAPACITY_PROVIDER_ID_PARAMETER"), "{e}");
    // A stage that names both parameters but keeps the microVM as its policy is fine: users can still ask for `instances`.
    assert!(
        from_lookup(env(&[
            ("CAPACITY_PROVIDER_ID_PARAMETER", "/hamh/direct/q"),
            ("INSTANCES_RUNTIME_ARN_PARAMETER", "/hamh/direct/p"),
        ]))
        .is_ok()
    );
}

// ---- the names the infra stack sets ---------------------------------------------------------------

/// What `infra/lib/stacks/orchestration-direct.ts` puts on the function when the stage has the Instances part: the NAMES of two SSM parameters,
/// never the capacity provider's id itself (the id exists only after the stack that creates the provider has been deployed).
const INFRA_RUNTIME_PARAMETER: &str = "/hamh/direct/worker-python-instances-runtime-arn";
const INFRA_PROVIDER_PARAMETER: &str = "/hamh/direct/instances-capacity-provider-id";

#[test]
fn the_variables_the_infra_stack_sets_make_a_valid_instances_stage() {
    for policy in [None, Some("microvm"), Some("instances")] {
        let mut pairs = vec![
            ("INSTANCES_RUNTIME_ARN_PARAMETER", INFRA_RUNTIME_PARAMETER),
            ("CAPACITY_PROVIDER_ID_PARAMETER", INFRA_PROVIDER_PARAMETER),
        ];
        if let Some(p) = policy {
            pairs.push(("COMPUTE_POLICY", p));
        }
        let r = from_lookup(env(&pairs));
        assert!(
            r.is_ok(),
            "{policy:?}: the function would not start on the variables the stack sets: {r:?}"
        );
    }
}

#[test]
fn a_provider_id_in_the_environment_is_not_the_stage_setting() {
    // The id is read from the parameter at the time a session starts. A `CAPACITY_PROVIDER_ID` variable is not read, so it cannot stand in for
    // the parameter of a stage whose policy is `instances`.
    let e = from_lookup(env(&[
        ("COMPUTE_POLICY", "instances"),
        ("INSTANCES_RUNTIME_ARN_PARAMETER", INFRA_RUNTIME_PARAMETER),
        ("CAPACITY_PROVIDER_ID", "hamh_direct_instances-AbCdEfGhIj"),
    ]))
    .unwrap_err();
    assert!(e.contains("CAPACITY_PROVIDER_ID_PARAMETER"), "{e}");
}
