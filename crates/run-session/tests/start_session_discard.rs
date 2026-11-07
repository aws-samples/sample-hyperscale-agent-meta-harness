//! A discard that lands while `start-session` works. The op checks that the run is active once, at its first
//! step; the discard (`ham cancel --discard`) clears the run, stops the execution and starts the reaper, which ends the session it finds. A
//! reaper that looks before the Agent item exists finds nothing and finishes, and an aborted execution runs no later state, so a session
//! that this op starts afterwards has no one to end it: its credentials stay live and, on Instances, its machine and disk bill until the
//! provider's own limits end them. The op therefore reads the run again after it wrote the Agent item and before it invokes, and once more
//! after the invoke returned (the reaper may have stopped a session that did not exist yet): a run that is no longer active has its
//! session ended by `end_session`, the same function the reaper calls, and the op returns `Invalid`.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::goal::GoalStatus;
use harness_protocol::store::AgentStatus;
use run_session::compute::ComputePolicy;
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn start(mem: &Mem) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .unwrap();
    handle(&mem.deps(), &op).await
}

fn seeded(instances: bool) -> Mem {
    let mem = Mem::default();
    if instances {
        mem.seed_with_members(
            repo_target(),
            "p",
            MODEL,
            json!({"compute": {"mode": "instances"}}),
        );
        mem.instances_stage(ComputePolicy::Microvm, Some(3));
    } else {
        mem.seed(repo_target(), "p", MODEL);
    }
    mem
}

#[tokio::test]
async fn a_run_that_stays_active_starts_as_before() {
    for instances in [false, true] {
        let mem = seeded(instances);
        start(&mem).await.unwrap();
        assert_eq!(mem.invoke_targets().len(), 1, "instances={instances}");
        assert_eq!(
            mem.agent().unwrap().status,
            AgentStatus::Running,
            "instances={instances}"
        );
        assert!(mem.ending().is_empty(), "{:?}", mem.ending());
    }
}

#[tokio::test]
async fn a_discard_after_the_first_check_ends_the_session_before_anything_is_invoked() {
    for instances in [false, true] {
        let mem = seeded(instances);
        // The first read of the Run item (the check at the top of the op) sees an active run; the discard lands right after it.
        mem.flip_run_after_reads(1, GoalStatus::Cleared);
        let e = start(&mem).await.unwrap_err();
        assert!(
            matches!(e, RunSessionError::Invalid(_)) && e.to_string().contains("no longer active"),
            "instances={instances}: {e:?}"
        );
        assert!(
            mem.invoke_targets().is_empty(),
            "instances={instances}: no machine was started for a discarded run"
        );
        let agent = mem.agent().expect("the Agent item was written");
        assert_eq!(agent.status, AgentStatus::Stopped, "instances={instances}");
        assert!(
            agent.creds.as_ref().is_some_and(|c| c.revoked),
            "instances={instances}: the credential binding is revoked"
        );
        if instances {
            assert_eq!(
                mem.ending(),
                [
                    "stop",
                    "delete_provider_session",
                    "release_slot",
                    "end_agent"
                ],
                "the same order the reaper uses"
            );
            assert!(mem.slot_holders().is_empty(), "the slot is free again");
        } else {
            assert_eq!(mem.ending().last().map(String::as_str), Some("end_agent"));
        }
    }
}

#[tokio::test]
async fn a_discard_while_the_invoke_runs_ends_the_session_that_the_invoke_started() {
    for instances in [false, true] {
        let mem = seeded(instances);
        // The reaper ran while the first call to an Instances machine was still launching it, found nothing to stop, and finished.
        mem.flip_run_on_invoke(GoalStatus::Cleared);
        let e = start(&mem).await.unwrap_err();
        assert!(
            matches!(e, RunSessionError::Invalid(_)) && e.to_string().contains("no longer active"),
            "instances={instances}: {e:?}"
        );
        assert_eq!(
            mem.invoke_targets().len(),
            1,
            "instances={instances}: the invoke had already gone out"
        );
        assert!(
            mem.ending().contains(&"stop".to_string()),
            "instances={instances}: the session the invoke started is stopped: {:?}",
            mem.ending()
        );
        assert_eq!(
            mem.agent().unwrap().status,
            AgentStatus::Stopped,
            "instances={instances}"
        );
        if instances {
            assert_eq!(
                mem.provider_deletes().len(),
                1,
                "the machine's provider session is deleted"
            );
            assert!(mem.slot_holders().is_empty());
        }
    }
}

// ---- the invoke can end in an error after it created the machine, and the end itself can fail (review of the fix, 2026-10-26) --------

#[tokio::test]
async fn a_discard_during_an_invoke_that_then_fails_or_answers_nonsense_still_ends_the_session() {
    for instances in [false, true] {
        for nonsense in [false, true] {
            let mem = seeded(instances);
            mem.flip_run_on_invoke(GoalStatus::Cleared);
            if nonsense {
                // The call returned, but not with the acknowledgement the op reads.
                *mem.0.invoke_reply.lock().unwrap() = Some(b"{}".to_vec());
            } else {
                // The call timed out after the machine was created.
                mem.fail(
                    "invoke",
                    RunSessionError::Unavailable("the call timed out".into()),
                );
            }
            let e = start(&mem).await.unwrap_err();
            let label = format!("instances={instances} nonsense={nonsense}: {e:?}");
            assert!(
                matches!(e, RunSessionError::Invalid(_))
                    && e.to_string().contains("no longer active"),
                "{label}"
            );
            assert!(
                mem.ending().contains(&"stop".to_string()),
                "{label}: {:?}",
                mem.ending()
            );
            assert_eq!(mem.agent().unwrap().status, AgentStatus::Stopped, "{label}");
            if instances {
                assert_eq!(mem.provider_deletes().len(), 1, "{label}");
                assert!(mem.slot_holders().is_empty(), "{label}");
            }
        }
    }
}

#[tokio::test]
async fn an_invoke_that_fails_for_an_active_run_keeps_its_own_error() {
    let mem = seeded(false);
    mem.fail(
        "invoke",
        RunSessionError::Unavailable("AgentCore 503".into()),
    );
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    assert!(mem.ending().is_empty(), "{:?}", mem.ending());
    assert_eq!(mem.agent().unwrap().status, AgentStatus::Running);
}

#[tokio::test]
async fn a_failed_end_is_tried_once_more_at_once_because_nothing_else_may_be_left_to_try_it() {
    // The reaper of the discard already finished (it found nothing), and the execution is aborted: no retry of this op will come.
    let mem = seeded(true);
    mem.flip_run_after_reads(1, GoalStatus::Cleared);
    mem.fail(
        "stop",
        RunSessionError::Unavailable("StopRuntimeSession throttled".into()),
    );
    let e = start(&mem).await.unwrap_err();
    assert!(
        matches!(e, RunSessionError::Invalid(_)) && e.to_string().contains("no longer active"),
        "{e:?}"
    );
    assert_eq!(mem.agent().unwrap().status, AgentStatus::Stopped);
    assert!(mem.slot_holders().is_empty());
    assert_eq!(mem.provider_deletes().len(), 1);
}

#[tokio::test]
async fn an_end_that_fails_twice_is_the_ops_error_and_leaves_the_agent_open_for_the_reaper() {
    let mem = seeded(false);
    mem.flip_run_after_reads(1, GoalStatus::Cleared);
    for _ in 0..2 {
        mem.fail(
            "stop",
            RunSessionError::Unavailable("StopRuntimeSession throttled".into()),
        );
    }
    let e = start(&mem).await.unwrap_err();
    assert!(matches!(e, RunSessionError::Unavailable(_)), "{e:?}");
    assert_eq!(mem.agent().unwrap().status, AgentStatus::Running);
    assert!(mem.invoke_targets().is_empty(), "nothing was invoked");
}
