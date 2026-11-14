"""`./setup.sh --update` takes the stage's deploy lock first and then looks for the runs of every user, and `--wait-for-runs` waits for them.

The stage's API service starts no new runs while a deploy holds the lock (it reads the SSM parameter `/hamh/<stage>/deploy-lock` before it creates a
run; crates/control-api `admission`). So the order in setup is: the early look (it stops a plain update in seconds, before the question), the
question, the lock, a pause, and then the look that decides: two looks in a row that answer and show no run. A run found there ends a plain update with
exit 7 and the lock given back. With `--wait-for-runs MINUTES` setup waits instead: one line for each look that shows runs, the lock written again while it
waits, and at the limit the lock is given back and the run ends with exit 7, "No deployment or client change was made. No run was cancelled."
Setup never cancels a run.

A hold of the lock that changes nothing on the stage (an update of the commit the stage already runs, which builds only the client; the removal of a runner
that was left; the time after the build has succeeded) carries the line `deploys=no`, so the API service does not refuse runs during it.

The tests run `setup.sh` from a stand-in checkout against the stand-in AWS CLI (setup_fake_aws.py). The scenario key `executions_by_look` names what each
look at the running executions finds in turn, and the stand-in keeps every text written to the lock (`lock_writes`) and the lock as it was at each look
(`looks_seen`).
"""

from __future__ import annotations

import re
import signal
import unittest
from pathlib import Path

from test_setup_update_guard import LIST_RUNS, RUNS, UpdateBase
from test_setup_update_runner import NOW, RunnerUpdateBase

REPO = Path(__file__).resolve().parents[2]
LOCK = "/hamh/direct/deploy-lock"
MACHINE = "hamh-direct-run"
TWO_SENTENCES = "No deployment or client change was made. No run was cancelled."


def lock_lines(text: str) -> dict:
    return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


class WaitBase(RunnerUpdateBase):
    def looks(self, *found: list) -> None:
        self.set_scenario(executions_by_look={MACHINE: [list(f) for f in found]})

    def flat_out(self, p) -> str:
        return " ".join(p.stdout.split())

    def lock_now(self) -> str | None:
        return self.world().get("params", {}).get(LOCK)

    def lock_writes(self) -> list:
        return [lock_lines(t) for t in self.world().get("lock_writes", [])]

    def calls_since(self, n: int) -> list:
        return self.normalized_calls()[n:]

    def is_lock_create(self, call: list) -> bool:
        return call[:2] == ["ssm", "put-parameter"] and LOCK in call and "--overwrite" not in call

    def stop_calls(self) -> list:
        return [c for c in self.normalized_calls() if c[:2] == ["stepfunctions", "stop-execution"]]

    def lock_calls_since(self, n: int) -> list:
        """The operations on the stage's deploy lock among the calls after the first `n`: put-parameter, delete-parameter (reads left out)."""
        return [c[1] for c in self.calls_since(n) if c[0] == "ssm" and LOCK in c and c[1] in ("put-parameter", "delete-parameter")]

    def same_commit_stage(self) -> str:
        """A stage whose record says that it runs the commit of this checkout with these inputs: an update of it builds only the client."""
        self.set_scenario(nine_stacks=True)
        head = self.deployed()
        cfg = self.deploy_starts()[0]["SETUP_CONFIG_SHA256"]
        self.seed_record(status="deployed", commit=head, tree="f" * 64, version="5.15", accounts=cfg, operator="x", azs="use1-az1", updated=NOW)
        return head


class OrderCase(WaitBase):
    """Decision 4: the lock first, then the look for the runs of all users."""

    def test_b804_wait_the_lock_is_taken_before_the_look_that_decides(self) -> None:
        self.deployed()
        self.commit_more()
        n = len(self.normalized_calls())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        later = self.calls_since(n)
        looks = [i for i, c in enumerate(later) if c[:2] == LIST_RUNS]
        self.assertEqual(len(looks), 3, "the early look, and two looks in a row after the lock")
        take = next(i for i, c in enumerate(later) if self.is_lock_create(c))
        install = next(i for i, c in enumerate(later) if c[:2] == ["cloudformation", "deploy"])
        self.assertLess(looks[0], take, "the early look comes before the question and the lock")
        self.assertLess(take, looks[1], "the look that decides comes after the lock")
        self.assertLess(looks[2], install, "and before the runner is installed")
        # at both looks that decide, the stage's lock was this update's and said nothing that opens the stage
        seen = self.world()["looks_seen"][-2:]
        for look in seen:
            self.assertIsNotNone(look["lock"], "the lock exists at the look that decides")
            self.assertNotIn("deploys", lock_lines(look["lock"]))
            self.assertEqual(lock_lines(look["lock"])["build"], "", "no build is started before the looks are done")

    def test_b804_wait_a_run_that_started_after_the_early_look_stops_a_plain_update_and_gives_the_lock_back(self) -> None:
        self.deployed()
        self.commit_more()
        mutations = len(self.mutations())
        calls = len(self.normalized_calls())
        self.looks([], RUNS)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = self.flat(p)
        self.assertIn("the update is stopped: 2 running run(s) on hamh-direct-run: " + " ".join(RUNS), lines)
        self.assertIn("The stage is unchanged and no runner was installed.", lines)
        self.assertIn("--wait-for-runs MINUTES waits for them", lines)
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "only the build of the full setup")
        self.assertEqual(self.lock_calls_since(calls), ["put-parameter", "delete-parameter"], "the lock was taken and given back")
        self.assertEqual(len(self.mutations()), mutations, "and nothing was created, uploaded, deleted or started")
        self.assertEqual(self.stop_calls(), [])

    def test_b804_wait_a_run_found_by_the_second_look_in_a_row_stops_the_update_too(self) -> None:
        self.deployed()
        self.commit_more()
        self.looks([], [], RUNS[:1])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("1 running run(s) on hamh-direct-run: " + RUNS[0], self.flat(p))
        self.assertIsNone(self.lock_now())
        self.assertFalse(self.runner_installed())

    def test_b804_wait_the_pause_before_the_look_that_decides_is_longer_than_the_time_limit_of_the_api_function(self) -> None:
        """A start that read "no lock" a moment before the lock was created ends inside the API function's time limit: the pause outlasts it."""
        full = (REPO / "scripts/setup/full.sh").read_text(encoding="utf-8")
        pause = int(re.search(r"HAMH_SETUP_RUN_SETTLE_SECONDS:-(\d+)\}", full).group(1))
        control = (REPO / "infra/lib/stacks/control.ts").read_text(encoding="utf-8")
        api = control[control.index('new lambda.CfnFunction(this, "ControlApi"'):]
        timeout = int(re.search(r"timeout: (\d+)", api).group(1))
        self.assertGreaterEqual(pause, timeout + 15, f"the pause ({pause} s) leaves room after the function's {timeout} s")
        self.assertEqual(int(re.search(r"HAMH_SETUP_RUN_POLL_SECONDS:-(\d+)\}", full).group(1)), 30, "one look every 30 seconds")

    def test_b804_wait_a_look_that_fails_is_never_taken_as_no_run(self) -> None:
        self.deployed()
        self.commit_more()
        # the early look answers; every look after the lock is throttled
        self.set_scenario(faults=[{"match": "stepfunctions list-executions", "exit": 254, "after": 1,
                                   "stderr": "An error occurred (ThrottlingException) when calling the ListExecutions operation: Rate exceeded"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        lines = self.flat(p)
        self.assertIn("could not look for the active runs of stage direct", lines)
        self.assertIn("A look that fails is not taken as no run.", lines)
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertFalse(self.runner_installed())
        self.assertEqual(len(self.list_run_calls()), 4, "the early look and three looks after the lock")

    def test_b804_wait_one_failed_look_is_made_again_and_the_update_goes_on(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[{"match": "stepfunctions list-executions", "exit": 254, "after": 1, "times": 1,
                                   "stderr": "An error occurred (ThrottlingException) when calling the ListExecutions operation: Rate exceeded"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("looking again", self.flat_out(p))
        self.assertEqual(len(self.list_run_calls()), 4, "the early look, the failed one, and two in a row that answer")

    def test_b804_wait_credentials_that_may_not_list_runs_keep_the_rule_of_the_early_look(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[{"match": "stepfunctions list-executions", "exit": 254,
                                   "stderr": "An error occurred (AccessDeniedException) when calling the ListExecutions operation: not authorized to perform: states:ListExecutions"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        text = self.flat_out(p)
        self.assertEqual(text.count("could not list the running executions of hamh-direct-run"), 1, "said once, by the early look")
        self.assertIn("The build checks again before it changes the worker runtime.", text)

    def test_b804_wait_the_looks_are_not_made_where_the_early_look_is_not_made(self) -> None:
        # an update of the commit the stage runs builds only the client: no look at all
        self.same_commit_stage()
        n = len(self.list_run_calls())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.list_run_calls()), n, "a client-only update does not look for runs")
        # --allow-active-runs: one look for the plan, none after the lock
        self.commit_more()
        n = len(self.list_run_calls())
        p = self.run_full(*self.UPDATE, "--allow-active-runs")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.list_run_calls()), n + 1)


class WaitOptionCase(WaitBase):
    """Decision 5: one wait for everyone's runs."""

    WAIT = UpdateBase.UPDATE + ("--wait-for-runs", "5")

    def test_b804_wait_the_option_waits_until_the_runs_have_ended_and_then_updates(self) -> None:
        self.deployed()
        self.commit_more()
        self.looks(RUNS, RUNS, RUNS[:1], [], [])
        p = self.run_full(*self.WAIT)
        self.assertEqual(p.returncode, 0, self.out(p))
        text = self.flat_out(p)
        self.assertIn("active runs: 2 running run(s): " + " ".join(RUNS) + ". The update waits up to 5 minutes for them to end; new runs are refused meanwhile. No run is cancelled.", text)
        self.assertIn("Waiting for 2 active run(s) to end: 5 min left. New runs are refused meanwhile. No run is cancelled.", text)
        self.assertIn("Waiting for 1 active run(s) to end: 5 min left. New runs are refused meanwhile. No run is cancelled.", text)
        self.assertEqual(text.count("Waiting for "), 2, "one line for each look that shows runs")
        self.assertIn("the active runs have ended", text)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2, "the build of the full setup and the build of the update")
        self.assertEqual(len(self.list_run_calls()), 5, "the early look and four after the lock, the last two in a row without a run")
        self.assertIsNone(self.lock_now(), "the lock is given back at the end of the update")
        self.assertEqual(self.stop_calls(), [], "no run is ever cancelled")

    def test_b804_wait_with_the_option_the_early_look_does_not_stop_the_update(self) -> None:
        self.deployed()
        self.commit_more()
        self.looks(RUNS, [], [])
        p = self.run_full(*self.WAIT)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("the update is stopped", self.out(p))

    def test_b804_wait_at_the_limit_the_lock_is_given_back_and_no_run_is_cancelled(self) -> None:
        self.deployed()
        self.commit_more()
        mutations = len(self.mutations())
        calls = len(self.normalized_calls())
        self.set_scenario(executions={MACHINE: RUNS})
        # a "minute" of one second and a look every second: the limit of two minutes comes after two seconds
        p = self.run_full(*self.UPDATE, "--wait-for-runs", "2", HAMH_SETUP_RUN_MINUTE_SECONDS="1", HAMH_SETUP_RUN_POLL_SECONDS="1")
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = self.flat(p)
        self.assertIn("2 run(s) are still active after 2 minutes: " + " ".join(RUNS), lines)
        self.assertIn(TWO_SENTENCES, lines)
        # a "minute" is one second here: on a loaded host the first look comes after the first second and the line reads 1 min left
        self.assertRegex(self.flat_out(p), r"Waiting for 2 active run\(s\) to end: [12] min left\. New runs are refused meanwhile\. No run is cancelled\.")
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "only the build of the full setup")
        self.assertEqual(self.lock_calls_since(calls), ["put-parameter", "delete-parameter"], "the lock was taken and given back")
        self.assertEqual(len(self.mutations()), mutations, "and nothing was created, uploaded, deleted or started")
        self.assertEqual(self.stop_calls(), [], "no run is ever cancelled")
        self.assertEqual(self.receipt()["state"], "failed")

    def test_b804_wait_ctrl_c_during_the_wait_gives_the_lock_back_and_ends_with_exit_4(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={MACHINE: RUNS})
        proc = self.popen(*self.WAIT, HAMH_SETUP_RUN_POLL_SECONDS="1")
        proc.wait_ready(lambda: "Waiting for 2 active run(s)" in proc.text(), 120.0, "setup.sh to wait for the active runs")
        self.assertIsNotNone(self.lock_now(), "the lock is held while setup waits")
        done = proc.interrupt(signal.SIGINT)
        self.assertEqual(done.returncode, 4, done.output)
        flat = " ".join(done.output.split())
        self.assertIn("Stopped while it looked for active runs (you stopped it).", flat)
        self.assertIn(TWO_SENTENCES, flat)
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.stop_calls(), [], "no run is ever cancelled")

    def test_b804_wait_the_lock_is_written_again_while_setup_waits(self) -> None:
        self.deployed()
        self.commit_more()
        self.looks(RUNS, RUNS, RUNS, RUNS, [], [])
        n = len(self.world().get("lock_writes", []))
        # HAMH_SETUP_LOCK_REFRESH_SECONDS=0: every look is due for a renewal (600 seconds by default)
        p = self.run_full(*self.WAIT, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        waiting = [w for w in self.lock_writes()[n:] if w.get("build") == "" and "deploys" not in w]
        self.assertGreaterEqual(len(waiting), 4, "the create and a write for each look of the wait")
        self.assertEqual(len({w["token"] for w in waiting}), 1, "one hold of the lock")
        full = (REPO / "scripts/setup/full.sh").read_text(encoding="utf-8")
        gate = full[full.index("full_gate_keep() {"):]
        self.assertIn("HAMH_SETUP_LOCK_REFRESH_SECONDS:-600", gate[:gate.index("\n}\n")], "by default every ten minutes: well inside the 30 minutes of a lock nobody renews")

    def test_b804_wait_a_look_that_fails_during_the_wait_is_made_again(self) -> None:
        self.deployed()
        self.commit_more()
        self.looks(RUNS, RUNS, [], [])
        self.set_scenario(executions_by_look={MACHINE: [RUNS, RUNS, [], []]},
                          faults=[{"match": "stepfunctions list-executions", "exit": 254, "after": 2, "times": 1,
                                   "stderr": "An error occurred (ThrottlingException) when calling the ListExecutions operation: Rate exceeded"}])
        p = self.run_full(*self.WAIT)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("looking again", self.flat_out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2)

    def test_b804_wait_a_lock_that_another_setup_replaced_ends_the_wait_with_nothing_changed(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={MACHINE: RUNS}, replace_lock_at_look=3)
        p = self.run_full(*self.WAIT, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("another setup took it while this run looked for active runs", self.flat(p))
        self.assertEqual(lock_lines(self.lock_now())["token"], "f" * 32, "the other setup's lock is left alone")
        self.assertFalse(self.runner_installed())

    def test_b804_wait_the_option_is_a_usage_error_outside_an_update_and_with_a_bad_number(self) -> None:
        base = ("--profile", "work", "--region", "us-east-1")
        cases = [
            (("--full", *base, "--yes", "--acknowledge-costs", "--wait-for-runs", "5"), "--wait-for-runs goes with --update"),
            (("--teardown", *base, "--wait-for-runs", "5"), "--wait-for-runs goes with --update"),
            (("--status", *base, "--wait-for-runs", "5"), "--wait-for-runs goes with --update"),
            ((*base, "--wait-for-runs", "5"), "--wait-for-runs goes with --update"),
            (("--update", *base, "--wait-for-runs", "5", "--allow-active-runs"), "give --wait-for-runs or --allow-active-runs, not both"),
            (("--update", *base, "--wait-for-runs", "0"), "--wait-for-runs must be a whole number of minutes from 1 to 1440 (got: 0)"),
            (("--update", *base, "--wait-for-runs", "1441"), "--wait-for-runs must be a whole number of minutes from 1 to 1440 (got: 1441)"),
            (("--update", *base, "--wait-for-runs", "ten"), "--wait-for-runs must be a whole number of minutes from 1 to 1440 (got: ten)"),
            (("--update", *base, "--wait-for-runs", "1.5"), "--wait-for-runs must be a whole number of minutes from 1 to 1440 (got: 1.5)"),
            (("--update", *base, "--wait-for-runs="), "--wait-for-runs must be a whole number of minutes from 1 to 1440 (got: )"),
            (("--update", *base, "--wait-for-runs"), "--wait-for-runs needs a value"),
        ]
        for args, words in cases:
            with self.subTest(args=args):
                p = self.run_full(*args)
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn(words, p.stderr)
                self.assertEqual(self.calls(), [], "a usage error makes no AWS call")

    def test_b804_wait_a_number_with_leading_zeros_is_read_as_a_decimal_number(self) -> None:
        self.deployed()
        self.commit_more()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--wait-for-runs=08", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("waits up to 8 minutes for the active runs", self.flat_out(p))

    def test_b804_wait_help_and_the_plan_name_the_option(self) -> None:
        text = " ".join(self.run_setup("--help").stdout.split())
        start = text.index("--update build and deploy")
        about = text[start:text.index("--revision ID", start)]
        self.assertIn("--wait-for-runs MINUTES", about)
        self.assertIn("No run is cancelled.", about)
        self.deployed()
        self.commit_more()
        with_option = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--wait-for-runs", "45", "--dry-run")
        self.assertEqual(with_option.returncode, 0, self.out(with_option))
        plan = self.flat_out(with_option)
        self.assertIn("guard: the update takes the stage's deploy lock, so the stage starts no new runs, and waits up to 45 minutes for the active runs to end (--wait-for-runs). No run is cancelled.", plan)
        self.assertNotIn("Active runs stop ./setup.sh --update with exit 7 before anything is installed or built.", plan)
        without = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertIn("Active runs stop ./setup.sh --update with exit 7 before anything is installed or built.", self.flat_out(without))
        self.assertNotIn("--wait-for-runs)", self.flat_out(without))

    def test_b804_wait_the_plan_of_a_client_only_update_says_that_it_waits_for_no_run(self) -> None:
        self.same_commit_stage()
        self.set_scenario(executions={MACHINE: RUNS})
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--wait-for-runs", "45", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.flat_out(p)
        self.assertIn("guard: this update changes nothing on the stage, so it waits for no run (--wait-for-runs) and the stage keeps starting runs.", plan)
        self.assertNotIn("waits up to 45 minutes", plan)

    def test_b804_wait_the_early_refusal_names_the_option(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={MACHINE: RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("./setup.sh --update --wait-for-runs MINUTES waits for them", self.flat(p))


class DeploysNothingCase(WaitBase):
    """The line `deploys=no`: a hold of the lock that changes nothing on the stage does not close it to new runs."""

    def test_b804_wait_a_deploying_update_marks_its_lock_only_after_its_build_has_succeeded(self) -> None:
        self.deployed()
        self.commit_more()
        n = len(self.world().get("lock_writes", []))
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()[n:]
        self.assertGreaterEqual(len(writes), 3, writes)
        built = [w for w in writes if w.get("build")]
        self.assertTrue(built, "the build is recorded in the lock")
        first_open = next(i for i, w in enumerate(writes) if w.get("deploys") == "no")
        for w in writes[:first_open]:
            self.assertNotIn("deploys", w, "while the build may deploy, the lock closes the stage")
        build = writes[first_open].get("build")
        self.assertTrue(build, "the first write that opens the stage names the build that succeeded")
        self.assertIn(build, [w.get("build") for w in writes[:first_open]], "that build was recorded in a lock that closed the stage while it ran")
        for w in writes[first_open:]:
            self.assertEqual(w.get("deploys"), "no", "once the stage is complete, no later write closes it again")

    def test_b804_wait_a_client_only_update_never_closes_the_stage(self) -> None:
        self.same_commit_stage()
        n = len(self.world().get("lock_writes", []))
        # runs are active: they do not stop an update that changes nothing on the stage, and the stage must not refuse new ones either
        self.set_scenario(executions={MACHINE: RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("This update builds only the client", self.out(p))
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")
        writes = self.lock_writes()[n:]
        self.assertTrue(writes, "the runner route holds the lock while it builds the client")
        for w in writes:
            self.assertEqual(w.get("deploys"), "no", w)

    def test_b804_wait_a_forced_deploy_of_the_same_commit_closes_the_stage(self) -> None:
        self.same_commit_stage()
        n = len(self.world().get("lock_writes", []))
        p = self.run_full(*self.UPDATE, "--force-deploy")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("deploys", self.lock_writes()[n], "the lock a deploying update creates has no such line")

    def test_b804_wait_the_first_setup_of_a_stage_marks_its_lock_only_after_its_build(self) -> None:
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()
        self.assertNotIn("deploys", writes[0])
        self.assertEqual(writes[-1].get("deploys"), "no")

    def test_b804_wait_the_line_is_the_one_the_api_service_reads(self) -> None:
        runner = (REPO / "scripts/setup/runner.sh").read_text(encoding="utf-8")
        self.assertIn("printf '\\ndeploys=no'", runner)
        admission = (REPO / "crates/control-api/src/admission.rs").read_text(encoding="utf-8")
        self.assertIn('const DEPLOYS_NOTHING: &str = "deploys=no";', admission)

    def test_b804_wait_the_30_minutes_of_a_lock_nobody_renews_are_one_number_in_one_file(self) -> None:
        number = (REPO / "scripts/lib/deploy-lock-stale-seconds.txt").read_text(encoding="utf-8")
        self.assertEqual(number, "1800\n")
        runner = (REPO / "scripts/setup/runner.sh").read_text(encoding="utf-8")
        line = next(ln for ln in runner.splitlines() if ln.startswith("RN_LOCK_STALE_SECONDS="))
        self.assertIn("scripts/lib/deploy-lock-stale-seconds.txt", line, "setup reads the file")
        self.assertTrue(line.rstrip().endswith(" 1800)"), "and falls back to the same number when a partial copy of the tree lacks it: " + line)
        admission = (REPO / "crates/control-api/src/admission.rs").read_text(encoding="utf-8")
        self.assertIn("include_str!(", admission)
        self.assertIn('"../../../scripts/lib/deploy-lock-stale-seconds.txt"', admission, "the API service embeds the same file")
        # that the public tree carries the file is held where the export rules are tested: a test that names the rules file is not exported


if __name__ == "__main__":
    unittest.main()
