# statemachines

## Purpose

Amazon States Language definitions of the two Standard state machines of stage `direct`: `direct`
(the run machine `hamh-direct-run`) and `direct-run-ended` (what clears a run whose execution ended early). The orchestration stack reads
each file as its definition and binds its `${...}` placeholders with DefinitionSubstitutions.

## Ownership

Owned by the root contract. The orchestration stack (`infra/lib/stacks/orchestration-direct.ts`) deploys
them; `crates/run-session` answers every Task they route.

## Local Contracts

- `direct.asl.json` (stage `direct`, machine `hamh-direct-run`, `TimeoutSeconds` 32,400): 20 states, 11 of them Tasks and one Fail state, acyclic, no
  Wait. Input `{runId}`. `Name` is the first state (`StartAt`): a Task with resource `arn:aws:states:::lambda:invoke`, function run-session, `InvocationType` `Event`, payload `{"op": "name-run", "runId.$": "$.runId"}`,
  `ResultPath` null, `TimeoutSeconds` 15 and next state `Prepare`. Lambda accepts the event and answers at once; the machine does not wait for the name, and `Prepare` receives the execution's starting data
  unchanged. Its Retry covers only `Lambda.ServiceException`, `Lambda.TooManyRequestsException` and `Lambda.SdkClientException`: first wait 1 s, 2 retries, backoff 2, full jitter. Its Catch uses `States.ALL`,
  `ResultPath` null and next state `Prepare`; it is the one Catch that avoids `Blocked`, `Plan`, `PrFailed` and the Fail state `RunMachineFailed`, because a label is not worth a blocked run. A run whose name is not
  written keeps its prompt-built name. The call can fail only by not being made; Lambda owns what the function does with the event (its asynchronous retries), never the machine. Then `Prepare` (run-session) -> `NeedsRepo` -> `CreateRepo` (new repository only) -> `StageRepo` (repo-gateway) -> `Init` (a Pass
  that rebuilds the state with `session: {turn: {}, failure: {}}`, `prOut: {}` and `prFailure: {}`: empty objects, never null) ->
  `StartSession` (`lambda:invoke.waitForTaskToken`, `HeartbeatSeconds` 600, `TimeoutSeconds` 28,500, result at `$.session.turn`; every error,
  including `States.HeartbeatTimeout` and `States.Timeout`, is caught into `$.session.failure` and goes to `Plan`) -> `Plan` (`plan-delivery`) ->
  `HasWork` -> `ValidateBundle` -> `ValidationOutcome` -> `HasPr` -> `PushPullRequest` (a failure goes to `PrFailed`, not Blocked) ->
  `Package` (`States.JsonMerge` of the plan's package payload and `$.prLink`, the only intrinsic) -> `Finalize` (run-session). Anything else that
  fails (`Name` excepted) lands in `Blocked` (`mark-blocked`). Placeholders: `RunSessionArn`, `RepoGatewayArn`, `BundleValidatorArn`, `PackagerArn`. Retry sets: run-session
  ops retry `RunSessionConflict`, `RunSessionUnavailable` and Lambda's three errors (2 s, 6 attempts); the delivery Lambdas retry `Unavailable` and
  Lambda's (5 s, 5 attempts, 4 for `Package`). `infra/test/statemachines-direct.test.ts` checks the structure and that every `.$` path is written on every
  path that reaches the state; `crates/run-session/tests/machine.rs` walks the file over fakes (paths: done with a pull request, `--s3-only`, a new
  repository, a lost session with and without a snapshot, a failed validation, a failed pull request push, a failed packager, a retried start, and the `Name` event: sent once per execution on a channel of its own so no call index of the kill tests moves, a dead function does not stop the run, a retried then caught error reaches `Prepare` with the input untouched).
  Every Task catches `States.ALL`; the Catch of `Blocked` writes the error to `$.blockedFailure` and goes to the Fail state `RunMachineFailed` (Error `RunSessionUnreachable`, `CausePath` `$.blockedFailure.Cause`). When run-session cannot answer even `mark-blocked` (its init failed, or it stayed unavailable through every retry), the execution fails with that cause instead of ending inside `Blocked`, and the `direct-run-ended` machine clears the run once the function answers.
- `direct-run-ended.asl.json` (stage `direct`, machine `hamh-direct-run-ended`, `TimeoutSeconds` 864,000): 3 states. `Reap` is a Task that invokes `run-session` with the whole input as Payload (the EventBridge Step Functions Execution Status Change event of the run machine, status `ABORTED`, `FAILED` or `TIMED_OUT`), `ResultPath` null (the output is the input), `TimeoutSeconds` 330. Its Retry is `RunSessionInvalid` with `MaxAttempts` 0, then `States.ALL`: 300 s, `BackoffRate` 1, 1000 retries, so every other failure is retried whatever its name (a conflict during a deploy, `Sandbox.Timedout`, a permission fault). Catch `RunSessionInvalid` goes to `Dropped` (Succeed); any other failure after the last retry goes to the Fail state `ReaperGaveUp` (Error `RunNotReaped`, `CausePath` `$.failure.Cause`), so exhaustion is visible and invoking `run-ended` with the same event clears the run once the function works. The only placeholder is `RunSessionArn`. The rule `hamh-direct-run-ended` starts this machine through the role `hamh-direct-run-ended-events`, because an asynchronous `run-session` invocation from the rule would be retried twice by Lambda and then dropped. Not covered, by design of the event path: an execution whose status event never arrives, and a run whose execution was never started (a scheduled sweep of `ListExecutions` would cover the first). Structure tests: `infra/test/statemachines-direct.test.ts` and `statemachines-direct-run-ended.test.ts`; `crates/run-session/tests/machine.rs` and `reaper_machine.rs` walk both files over fakes.
- `direct.asl.json` loops over the sessions of a run. `Init` sets `loop` at session 1; `StartSession` starts session `$.loop.slice` and catches every error into `NextSlice`; `NextSlice` (run-session `next-slice`, 330 s, `ResultPath $.loop`, catch into `Plan`) stops the session's machine and decides; `SliceChoice` sends `loop.continue` through `ResetSession` (clears `session`, `prOut` and `prFailure`) back to `StartSession`, and anything else to `Plan`. The definition has 23 states, 12 of them Tasks, and `TimeoutSeconds` 2,628,000; a run's own total time (24 hours by default, at most 720) ends it long before. A run has at most 256 sessions, so one execution stays under the 25,000-event history limit. Tests: `infra/test/statemachines-direct.test.ts`; `crates/run-session/tests/slice_loop.rs` walks the real definition over fakes with a clock.

## Work Guidance

## Verification

```bash
cd infra && ../scripts/node.sh pnpm test   # statemachines-direct.test.ts and statemachines-direct-run-ended.test.ts parse the definitions and check every route, retry and catch
scripts/cargo-slot.sh cargo test -p run-session --test machine   # walks direct.asl.json over fakes: done with a pull request, --s3-only, a new repository, a lost session, a failed validation, push, packager and a retried start
```

## Child DOX Index

None.
