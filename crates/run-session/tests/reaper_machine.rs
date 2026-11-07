//! `statemachines/direct-run-ended.asl.json` walked over the real `run-ended` op (defect F1/2). The run-ended rule used to invoke
//! the function asynchronously: Lambda retried twice within three minutes and dropped the event, so a run whose execution had
//! failed while the function could not start stayed `active`. The rule now starts this Standard machine, which calls the function
//! every five minutes until it answers.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::lambda::serve_event;
use serde_json::{Value, json};
use support::asl::{Lambdas, Wait, reaper_definition, walk};
use support::fakes::*;

/// run-session as the machine sees it: the real `serve_event` over the in-memory world, with the first `fail_first` calls
/// failing the way Lambda reports the failures it can have.
struct Function {
    mem: Mem,
    fail_first: u32,
    error: (&'static str, &'static str),
    calls: u32,
    builder: tokio::runtime::Handle,
}

impl Function {
    fn new(mem: &Mem, fail_first: u32, error: (&'static str, &'static str)) -> Function {
        Function {
            mem: mem.clone(),
            fail_first,
            error,
            calls: 0,
            builder: tokio::runtime::Handle::current(),
        }
    }
}

impl Lambdas for Function {
    fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, (String, String)> {
        assert_eq!(function, "run-session");
        self.calls += 1;
        if self.calls <= self.fail_first {
            return Err((self.error.0.into(), self.error.1.into()));
        }
        let deps = self.mem.deps();
        let h = self.builder.clone();
        let (result, _log) =
            tokio::task::block_in_place(|| h.block_on(serve_event(&deps, payload)));
        result.map_err(|e| (e.error_type().to_string(), e.to_string()))
    }
    fn wait(&mut self, _function: &str, _token: &str) -> Wait {
        unreachable!("the reaper does not wait for a token")
    }
}

const INIT: (&str, &str) = (
    "Runtime.ExitError",
    "RequestId: ecde32ad Error: Runtime exited with error: exit status 1",
);

fn failed_event() -> Value {
    json!({"version": "0", "id": "e-1", "source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": "FAILED", "name": RUN, "stateMachineArn": MACHINE, "executionArn": format!("{MACHINE}:{RUN}"),
            "stopDate": 1_790_000_100_000u64, "error": "Runtime.ExitError",
            "cause": "{\"errorType\":\"Runtime.ExitError\",\"errorMessage\":\"Runtime exited with error: exit status 1\"}"}})
}

fn active_run() -> Mem {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, "p", MODEL);
    mem
}

#[tokio::test(flavor = "multi_thread")]
async fn a_healthy_function_clears_the_run_on_the_first_call() {
    let mem = active_run();
    let mut f = Function::new(&mem, 0, INIT);
    let out = walk(&reaper_definition(), failed_event(), &mut f);
    assert_eq!(out.trace, ["Reap"]);
    assert!(out.retries.is_empty());
    assert_eq!(out.failed, None);
    assert_eq!(
        out.output,
        failed_event(),
        "the result is discarded: the output is the input"
    );
    assert_eq!(mem.run().status, GoalStatus::Cleared);
}

/// 2026-10-14: the first three calls died at init (EventBridge's asynchronous invocation made three attempts in three minutes and
/// stopped). The machine keeps going, and the run is cleared when the function answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_cannot_start_is_called_again_until_it_answers_and_then_clears_the_run() {
    let mem = active_run();
    let mut f = Function::new(&mem, 3, INIT);
    let out = walk(&reaper_definition(), failed_event(), &mut f);
    assert_eq!(out.trace, ["Reap"]);
    assert_eq!(out.retries, [("Reap".to_string(), 3)]);
    assert_eq!(mem.run().status, GoalStatus::Cleared);
    let aborted = serde_json::to_value(&*mem.run().aborted.unwrap()).unwrap();
    assert_eq!(aborted["status"], "FAILED");
    assert_eq!(aborted["error"], "Runtime.ExitError");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_is_down_for_days_still_gets_its_reap() {
    // 5 minutes between calls: 999 failed calls are 3.4 days.
    let mem = active_run();
    let mut f = Function::new(&mem, 999, INIT);
    let out = walk(&reaper_definition(), failed_event(), &mut f);
    assert_eq!(out.retries, [("Reap".to_string(), 999)]);
    assert_eq!(mem.run().status, GoalStatus::Cleared);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_failure_but_a_refusal_is_retried_whatever_its_name() {
    // The names the live failure and the reviews listed, and ones nobody listed: the point of the machine is to outlast a damaged
    // deployment, so it does not enumerate what may pass.
    for (error, message) in [
        ("RunSessionUnavailable", "DynamoDB 503"),
        ("RunSessionConflict", "run changed"),
        ("Lambda.ServiceException", "Lambda is unavailable"),
        ("Lambda.TooManyRequestsException", "Rate Exceeded."),
        (
            "Lambda.ResourceConflictException",
            "The function is Pending",
        ),
        ("Lambda.Unknown", "unknown"),
        ("Sandbox.Timedout", "Task timed out after 300.00 seconds"),
        ("Runtime.OutOfMemory", "out of memory"),
        ("States.Permissions", "not authorized to invoke"),
        ("States.Timeout", "timeout"),
        ("SomeErrorNobodyListed", "x"),
    ] {
        let mem = active_run();
        let mut f = Function::new(&mem, 2, (error, message));
        let out = walk(&reaper_definition(), failed_event(), &mut f);
        assert_eq!(out.retries, [("Reap".to_string(), 2)], "{error}");
        assert_eq!(mem.run().status, GoalStatus::Cleared, "{error}");
    }
}

/// 1001 failed calls (the first and 1000 retries) end the execution in the Fail state: the failure is visible, the run is still
/// active, and the cause names the last error.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_stays_down_through_every_retry_fails_the_execution_visibly() {
    let mem = active_run();
    let mut f = Function::new(&mem, 5_000, INIT);
    let out = walk(&reaper_definition(), failed_event(), &mut f);
    assert_eq!(out.trace, ["Reap", "ReaperGaveUp"]);
    assert_eq!(out.retries, [("Reap".to_string(), 1000)]);
    assert_eq!(f.calls, 1001);
    let (error, cause) = out.failed.unwrap();
    assert_eq!(error, "RunNotReaped");
    assert!(cause.contains("Runtime exited with error"), "{cause}");
    assert_eq!(mem.run().status, GoalStatus::Active);
    // The same event, delivered again once the function works, clears the run.
    let mut healed = Function::new(&mem, 0, INIT);
    walk(&reaper_definition(), failed_event(), &mut healed);
    assert_eq!(mem.run().status, GoalStatus::Cleared);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_that_can_never_be_reaped_is_dropped_without_a_retry() {
    let mem = active_run();
    let foreign = json!({"source": "aws.states", "detail-type": "Step Functions Execution Status Change",
        "detail": {"status": "FAILED", "name": RUN,
            "stateMachineArn": "arn:aws:states:us-east-1:700000000011:stateMachine:hamh-live-run"}});
    let mut f = Function::new(&mem, 0, INIT);
    let out = walk(&reaper_definition(), foreign, &mut f);
    assert_eq!(out.trace, ["Reap", "Dropped"]);
    assert!(out.retries.is_empty());
    assert_eq!(f.calls, 1);
    assert_eq!(
        out.caught,
        [("Reap".to_string(), "RunSessionInvalid".to_string())]
    );
    assert_eq!(mem.run().status, GoalStatus::Active);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_is_already_over_or_was_never_created_is_left_alone_and_the_machine_ends() {
    let done = active_run();
    // The run was already cleared when the event arrived: the machine ends and nothing more is written.
    let before = done.events().len();
    let mut f = Function::new(&done, 0, INIT);
    walk(&reaper_definition(), failed_event(), &mut f);
    let cleared = done.events().len();
    assert!(cleared > before);
    let again = walk(&reaper_definition(), failed_event(), &mut f);
    assert_eq!(again.trace, ["Reap"]);
    assert_eq!(
        done.events().len(),
        cleared,
        "a repeated event writes no second run.aborted"
    );
    // No Run item at all.
    let none = Mem::default();
    let mut f = Function::new(&none, 0, INIT);
    let out = walk(&reaper_definition(), failed_event(), &mut f);
    assert_eq!(out.trace, ["Reap"]);
    assert_eq!(out.failed, None);
}
