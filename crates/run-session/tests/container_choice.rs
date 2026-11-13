//! The machine of a run when the repository may use containers. The Instances machine cannot run containers, so a signal found in
//! the staged repository keeps an `auto` run on the microVM and refuses an explicit `instances` request. A repository with no signal keeps the
//! choice the request and the stage's policy made before. The decision is a pure function of three facts: what the request asked, the stage's
//! policy and what the repo-gateway reported.
#![allow(clippy::unwrap_used)]

use ham_core::container_signals::{Check, Found};
use harness_protocol::direct::{ComputeMode, ComputeOptions};
use run_session::compute::{
    Compute, ComputePolicy, choose_compute, choose_compute_checked, needs_container_check,
};

const POLICIES: [ComputePolicy; 2] = [ComputePolicy::Microvm, ComputePolicy::Instances];

fn ask(mode: ComputeMode) -> Option<ComputeOptions> {
    Some(ComputeOptions {
        mode,
        container_signals: None,
    })
}

fn found() -> Check {
    Check {
        found: Some(Found {
            signal: "docker-compose.yml".into(),
            path: "deploy/docker-compose.yml".into(),
        }),
    }
}

fn nothing() -> Check {
    Check::default()
}

#[test]
fn b632_the_gateway_is_asked_only_when_the_run_could_end_up_on_instances() {
    // (request, policy, asked)
    for policy in POLICIES {
        assert!(
            needs_container_check(ask(ComputeMode::Instances).as_ref(), policy),
            "an explicit instances request is always checked"
        );
        assert!(
            !needs_container_check(ask(ComputeMode::Microvm).as_ref(), policy),
            "an explicit microvm request never needs the check"
        );
    }
    for request in [None, ask(ComputeMode::Auto)] {
        assert!(!needs_container_check(
            request.as_ref(),
            ComputePolicy::Microvm
        ));
        assert!(needs_container_check(
            request.as_ref(),
            ComputePolicy::Instances
        ));
    }
}

#[test]
fn b632_auto_with_a_signal_stays_on_the_microvm_whatever_the_policy_says() {
    for request in [None, ask(ComputeMode::Auto)] {
        for policy in POLICIES {
            let chosen = choose_compute_checked(request.as_ref(), policy, Some(&found())).unwrap();
            assert_eq!(chosen.compute, Compute::Microvm, "{request:?} {policy:?}");
        }
    }
    let note = choose_compute_checked(None, ComputePolicy::Instances, Some(&found()))
        .unwrap()
        .note
        .unwrap();
    assert!(
        note.contains("deploy/docker-compose.yml") && note.contains("microVM"),
        "the log says why the policy was not followed: {note}"
    );
    let quiet = choose_compute_checked(None, ComputePolicy::Microvm, Some(&found())).unwrap();
    assert_eq!(
        quiet.note, None,
        "the policy was followed, so there is nothing to say"
    );
}

#[test]
fn b632_an_explicit_instances_request_with_a_signal_is_refused_with_the_signal_and_the_way_out() {
    for policy in POLICIES {
        let reason =
            choose_compute_checked(ask(ComputeMode::Instances).as_ref(), policy, Some(&found()))
                .unwrap_err();
        for part in [
            "deploy/docker-compose.yml",
            "\"docker-compose.yml\"",
            "--compute microvm",
            "run.container_signals",
        ] {
            assert!(reason.contains(part), "{part}: {reason}");
        }
    }
}

#[test]
fn b632_a_repository_without_a_signal_keeps_the_choice_it_had_before() {
    for request in [
        None,
        ask(ComputeMode::Auto),
        ask(ComputeMode::Microvm),
        ask(ComputeMode::Instances),
    ] {
        for policy in POLICIES {
            let chosen =
                choose_compute_checked(request.as_ref(), policy, Some(&nothing())).unwrap();
            assert_eq!(
                chosen.compute,
                choose_compute(request.clone(), policy),
                "{request:?} {policy:?}"
            );
            assert_eq!(chosen.note, None);
        }
    }
}

#[test]
fn b632_an_explicit_microvm_request_ignores_the_check() {
    for policy in POLICIES {
        for check in [None, Some(nothing()), Some(found())] {
            let chosen =
                choose_compute_checked(ask(ComputeMode::Microvm).as_ref(), policy, check.as_ref())
                    .unwrap();
            assert_eq!(chosen.compute, Compute::Microvm);
        }
    }
}

#[test]
fn b632_a_run_that_was_not_checked_falls_back_to_the_microvm_or_is_refused() {
    // A repo-gateway that predates the check reports nothing: the run must not slip onto Instances.
    let auto = choose_compute_checked(None, ComputePolicy::Instances, None).unwrap();
    assert_eq!(auto.compute, Compute::Microvm);
    assert!(auto.note.unwrap().contains("not checked"));
    let reason = choose_compute_checked(
        ask(ComputeMode::Instances).as_ref(),
        ComputePolicy::Microvm,
        None,
    )
    .unwrap_err();
    assert!(
        reason.contains("did not check") && reason.contains("--compute microvm"),
        "{reason}"
    );
    // Where the check was not needed, its absence is fine.
    let plain = choose_compute_checked(None, ComputePolicy::Microvm, None).unwrap();
    assert_eq!(plain.compute, Compute::Microvm);
    assert_eq!(plain.note, None);
}
