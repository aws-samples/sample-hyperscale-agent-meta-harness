"""The guard and the clean-up of `./setup.sh --update`.

An update lists the running executions of the stage's run state machine before it installs the runner or builds anything, and refuses with exit 7 when one
is running, so the refusal comes in seconds and the account is as it was. An update that stops before it changed the stage (the build's own guard, or a
build phase before the deploy) removes the runner it installed and says the stage is unchanged. An update that stops when the stage may hold partial
changes keeps the runner for the retry and says so. The full setup keeps its own behavior.

The tests run `setup.sh` from a stand-in checkout against the stand-in AWS CLI (setup_fake_aws.py); the stand-in lists the executions that the scenario key
`executions` names for the state machine.
"""

from __future__ import annotations

import unittest

from test_setup_full_state import StateBase

RUNS = ["01RUNAAAAAAAAAAAAAAAAAAAAA", "01RUNBBBBBBBBBBBBBBBBBBBBB"]
GUARD_TEXT = "DEPLOY-FAIL guard 2 running run(s) on hamh-direct-run pin the worker runtime: " + " ".join(RUNS)
LIST_RUNS = ["stepfunctions", "list-executions"]


class UpdateBase(StateBase):
    UPDATE = ("--update", "--profile", "work", "--region", "us-east-1", "--yes")

    def deployed(self) -> str:
        first = self.run_full(*self.ARGS)
        self.assertEqual(first.returncode, 0, self.out(first))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"], "a full setup removes its runner")
        return self.commit()

    def runner_installed(self) -> bool:
        return "hamh-setup-runner" in self.world()["stacks"]

    def flat(self, p) -> str:
        return " ".join(p.stderr.split())

    def list_run_calls(self) -> list:
        return [c for c in self.normalized_calls() if c[:2] == LIST_RUNS]


class EarlyGuardCase(UpdateBase):
    """D10: the refusal comes before the runner is installed and before anything is built."""

    def test_an_update_with_active_runs_is_refused_before_the_runner_is_installed(self) -> None:
        self.deployed()
        self.commit_more()
        mutations_before = len(self.mutations())
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = self.flat(p)
        self.assertIn("2 running run(s) on hamh-direct-run pin the worker runtime: " + " ".join(RUNS), lines)
        self.assertIn("The stage is unchanged and no runner was installed.", lines)
        self.assertIn("ham cancel <runId>", lines)
        self.assertEqual(len(self.mutations()), mutations_before, "nothing is created, uploaded, deleted or started before the refusal")
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "only the build of the full setup")
        self.assertFalse(self.runner_installed())
        self.assertNotIn("remove everything", lines)

    def test_the_refusal_comes_before_the_question(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--no-input")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertNotIn("no one to ask", self.out(p))

    def test_a_dry_run_of_an_update_with_active_runs_says_the_update_would_be_refused(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("running run(s)", p.stderr)
        self.assertNotIn("Dry run", p.stdout)

    def test_the_look_names_the_run_state_machine_of_the_stage_and_asks_for_running_executions_only(self) -> None:
        self.deployed()
        self.commit_more()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.list_run_calls()
        # The early look, then two looks in a row after the stage's deploy lock is taken (test_setup_b804_wait.py)
        self.assertEqual(len(calls), 3, calls)
        for words in calls:
            self.assertEqual(words[words.index("--state-machine-arn") + 1], f"arn:aws:states:us-east-1:{self.ACCOUNT}:stateMachine:hamh-direct-run")
            self.assertEqual(words[words.index("--status-filter") + 1], "RUNNING")

    def test_the_look_is_made_before_the_runner_is_installed(self) -> None:
        self.deployed()
        self.commit_more()
        n = len(self.calls())
        self.assertEqual(self.run_full(*self.UPDATE).returncode, 0)
        later = [c for c in self.normalized_calls()[n:]]
        names = [" ".join(c[:2]) for c in later]
        self.assertIn("stepfunctions list-executions", names)
        self.assertLess(names.index("stepfunctions list-executions"), names.index("cloudformation deploy"), names)

    def test_an_update_with_no_active_run_goes_on_and_prints_no_warning_about_the_look(self) -> None:
        self.deployed()
        self.commit_more()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("running executions", self.out(p))

    def test_a_missing_run_state_machine_means_no_run_is_active(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[{"match": "stepfunctions list-executions", "exit": 254,
                                   "stderr": "An error occurred (StateMachineDoesNotExist) when calling the ListExecutions operation: State Machine Does Not Exist"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("running executions", self.out(p))

    def test_a_look_that_fails_is_a_warning_and_the_build_still_guards(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[{"match": "stepfunctions list-executions", "exit": 254,
                                   "stderr": "An error occurred (AccessDeniedException) when calling the ListExecutions operation: not authorized to perform: states:ListExecutions"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        text = " ".join(p.stdout.split())
        self.assertIn("could not list the running executions of hamh-direct-run", text)
        self.assertIn("AccessDeniedException", text)
        self.assertIn("The build checks again before it changes the worker runtime.", text)

    def test_the_full_setup_does_not_look_at_the_runs(self) -> None:
        self.deployed()
        self.assertEqual(self.list_run_calls(), [])

    def test_a_status_and_a_doctor_do_not_look_at_the_runs(self) -> None:
        self.deployed()
        self.assertEqual(self.run_full("--status", "--profile", "work", "--region", "us-east-1").returncode, 0)
        self.assertEqual(self.list_run_calls(), [])


class AllowActiveRunsCase(UpdateBase):
    """`--allow-active-runs`: the way to go past the early look. The build's own guard then decides, and it refuses only an update that changes the worker runtime."""

    PLAN_LINE = "active runs are not checked before the build; the build refuses only when the worker runtime changes"
    SWITCH = UpdateBase.UPDATE + ("--allow-active-runs",)

    def test_the_switch_skips_the_early_look_and_the_update_goes_on(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.SWITCH)
        self.assertEqual(p.returncode, 0, self.out(p))
        # One read-only look lists the runs in the plan; it does not stop the update
        self.assertLessEqual(len(self.list_run_calls()), 1, "at most one look at the running executions, for the plan")
        self.assertIn(RUNS[0], p.stdout, "the plan names the runs that keep running under the update")
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2, "the build of the full setup and the build of the update")

    def test_without_the_switch_the_same_stage_is_refused_early(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)
        self.assertEqual(len(self.list_run_calls()), 1)

    def test_the_build_guard_still_stops_a_change_of_the_worker_runtime(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={"hamh-direct-run": RUNS},
                          build={"final": "FAILED", "result": "fail", "failed_phase": "guard", "detail": GUARD_TEXT})
        p = self.run_full(*self.SWITCH)
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = self.flat(p)
        self.assertIn(GUARD_TEXT, lines)
        self.assertIn("The stage is unchanged. The runner is removed.", lines)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2, "the refusal came from the build, not from the look that lists the runs in the plan")
        self.assertFalse(self.runner_installed())

    def test_the_plan_names_the_switch_only_when_it_is_set(self) -> None:
        self.deployed()
        self.commit_more()
        with_switch = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--allow-active-runs", "--dry-run")
        self.assertEqual(with_switch.returncode, 0, self.out(with_switch))
        self.assertIn(self.PLAN_LINE, " ".join(with_switch.stdout.split()))
        without = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(without.returncode, 0, self.out(without))
        self.assertNotIn(self.PLAN_LINE, " ".join(without.stdout.split()))

    def test_the_plan_line_stands_in_place_of_the_guard_line_that_promises_the_early_stop(self) -> None:
        self.deployed()
        self.commit_more()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--allow-active-runs", "--dry-run")
        plan = " ".join(p.stdout.split())
        self.assertNotIn("Active runs stop ./setup.sh --update with exit 7 before anything is installed or built.", plan)

    def test_the_switch_belongs_to_update_and_is_a_usage_error_everywhere_else(self) -> None:
        for args in (("--full", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs"), ("--teardown", "--profile", "work", "--region", "us-east-1"),
                     ("--status", "--profile", "work", "--region", "us-east-1"), ("--doctor", "--profile", "work", "--region", "us-east-1"),
                     ("--client-only", "--profile", "work", "--region", "us-east-1"), ("--profile", "work", "--region", "us-east-1")):
            with self.subTest(args=args[0]):
                p = self.run_full(*args, "--allow-active-runs")
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn("--allow-active-runs goes with --update", p.stderr)
                self.assertEqual(self.calls(), [], "a usage error makes no AWS call")

    def test_help_names_the_switch_in_the_update_entry(self) -> None:
        text = " ".join(self.run_setup("--help").stdout.split())
        start = text.index("--update build and deploy")
        about = text[start:text.index("--revision ID", start)]
        self.assertIn("--allow-active-runs", about)
        self.assertIn("the build then refuses only an update that changes the worker runtime", about)


class StageUnchangedCase(UpdateBase):
    """D11: an update that stops before it changed the stage removes its runner; one that may have changed the stage keeps it."""

    def fail_build(self, phase: str, detail: str) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": phase, "detail": detail})

    def test_a_refusal_by_the_build_guard_removes_the_runner_and_says_the_stage_is_unchanged(self) -> None:
        self.deployed()
        self.commit_more()
        self.fail_build("guard", GUARD_TEXT)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = self.flat(p)
        self.assertIn(GUARD_TEXT, lines)
        self.assertIn("The stage is unchanged. The runner is removed.", lines)
        self.assertIn("You wait until the active runs end.", lines)
        self.assertIn("./setup.sh --update to retry", lines)
        self.assertNotIn("remove everything", lines)
        self.assertNotIn("The runner stays installed", lines)
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.receipt()["state"], "failed")
        self.assertEqual(self.receipt()["failedPhase"], "guard")

    def test_the_build_log_pointer_is_not_given_for_a_log_group_that_went_with_the_runner(self) -> None:
        self.deployed()
        self.commit_more()
        self.fail_build("guard", GUARD_TEXT)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertNotIn("CloudWatch log group", self.out(p))
        self.assertIn("The full log is here", self.out(p))

    def assert_phase_before_the_deploy_is_unchanged(self, phase: str) -> None:
        self.deployed()
        self.commit_more()
        self.fail_build(phase, f"SETUP-FAIL {phase} the stand-in failed here")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        lines = self.flat(p)
        self.assertIn("The stage is unchanged. The runner is removed.", lines)
        self.assertIn("./setup.sh --update to retry", lines)
        self.assertNotIn("You wait until the active runs end", lines)
        self.assertNotIn("partial deployment changes", lines)
        self.assertFalse(self.runner_installed())

    def test_a_failure_in_the_inputs_phase_removes_the_runner_too(self) -> None:
        self.assert_phase_before_the_deploy_is_unchanged("inputs")

    def test_a_failure_in_the_fetch_phase_removes_the_runner_too(self) -> None:
        self.assert_phase_before_the_deploy_is_unchanged("fetch")

    def test_a_failure_in_the_toolchain_phase_removes_the_runner_too(self) -> None:
        self.assert_phase_before_the_deploy_is_unchanged("toolchain")

    def test_a_failure_in_the_account_file_phase_removes_the_runner_too(self) -> None:
        self.assert_phase_before_the_deploy_is_unchanged("account-file")

    def test_a_failure_in_the_deploy_keeps_the_runner_and_says_the_stage_may_hold_partial_changes(self) -> None:
        self.deployed()
        self.commit_more()
        self.fail_build("deploy", "DEPLOY-FAIL stack hamh-direct-network")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        lines = self.flat(p)
        self.assertIn("The runner stays installed", lines)
        self.assertIn("The stage may contain partial deployment changes.", lines)
        self.assertNotIn("The stage is unchanged", lines)
        self.assertNotIn("The runner is removed", lines)
        self.assertTrue(self.runner_installed())

    def assert_phase_keeps_the_runner(self, phase: str) -> None:
        # a phase after the deploy: the deploy finished, so the stage runs the new commit and only this step failed
        self.deployed()
        head = self.commit_more()
        self.fail_build(phase, f"SETUP-FAIL {phase} the stand-in failed here")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn(f"The stage runs commit {head}: its deploy finished, and only the step {phase} failed.", self.flat(p))
        self.assertNotIn("partial deployment changes", self.flat(p))
        self.assertTrue(self.runner_installed())

    def test_a_failure_in_the_gomod_phase_keeps_the_runner(self) -> None:
        self.assert_phase_keeps_the_runner("gomod")

    def test_a_failure_in_the_client_phase_keeps_the_runner(self) -> None:
        self.assert_phase_keeps_the_runner("client")

    def test_a_failure_in_the_outputs_phase_keeps_the_runner(self) -> None:
        self.assert_phase_keeps_the_runner("outputs")

    def test_a_failure_with_no_result_keeps_the_runner(self) -> None:
        """A build that CodeBuild killed leaves no result: nobody can say how far it got."""
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"final": "TIMED_OUT", "result": "none"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("The stage may contain partial deployment changes.", self.flat(p))
        self.assertTrue(self.runner_installed())

    def test_a_runner_that_cannot_be_removed_is_named_and_the_stage_is_still_unchanged(self) -> None:
        self.deployed()
        self.commit_more()
        self.fail_build("guard", GUARD_TEXT)
        self.set_scenario(empty_bucket_fails=True)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = " ".join(self.out(p).split())
        self.assertIn("RETAINED stack hamh-setup-runner", lines)
        self.assertIn("The stage is unchanged. The runner could not be removed and stays installed", lines)
        self.assertNotIn("The runner is removed.", lines)
        self.assertTrue(self.runner_installed())

    def test_a_failed_full_setup_before_the_deploy_keeps_its_runner_for_the_retry(self) -> None:
        self.fail_build("toolchain", "SETUP-FAIL toolchain the stand-in failed here")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("The runner stays installed", self.flat(p))
        self.assertTrue(self.runner_installed())


if __name__ == "__main__":
    unittest.main()
