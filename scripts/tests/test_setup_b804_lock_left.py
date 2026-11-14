"""The review of the update barrier: what the stage's deploy lock says when an update ends early.

The stage's API service starts no new runs while a deploy holds the lock `/hamh/<stage>/deploy-lock`: a lock without the line `deploys=no` that was written
within the last 30 minutes closes the stage. So a lock that setup leaves behind is no longer only a matter between two setups. These cases hold:

- Setup says that the lock is released only after the delete went through. A release that fails (a sign-in that ran out, a throttle) is said as it is: the
  lock stays, the stage starts no new runs until 30 minutes after its last write, and how to remove it sooner.
- Every exit while setup holds the lock and no build of it is recorded or may have started gives the lock back: a failed step before the build, Ctrl-C, a
  closed terminal.
- Once CodeBuild says the build has succeeded, the lock says `deploys=no`, also when the run then stops because a read failed, and a later run that finishes
  from that build never closes the stage again.
- A run that stops following its build says what the stage does with new runs meanwhile: the build renews the lock by itself while it deploys
  (scripts/lib/deploy.py, tested in test_setup_deploy_settings.py), and afterwards the stage opens when setup runs again or 30 minutes after the last write.
- One failed read of the lock does not end a wait for active runs.

The tests run `setup.sh` from a stand-in checkout against the stand-in AWS CLI (setup_fake_aws.py). Nothing here calls AWS.
"""

from __future__ import annotations

import signal
import subprocess  # nosec B404: the stand-in AWS CLI of the test is started with an argument list, never a shell
import sys
import unittest

import setup_waits
from setup_waits import GroupProcess, limit
from test_setup_b804_wait import LOCK, MACHINE, WaitBase, lock_lines
from test_setup_script import BASH
from test_setup_update_guard import RUNS, UpdateBase

EXPIRED = "An error occurred (ExpiredTokenException) when calling the {op} operation: The security token included in the request is expired"
THROTTLED = "An error occurred (ThrottlingException) when calling the {op} operation: Rate exceeded"
# every call of the stand-in carries the profile, so this fault is a sign-in that ran out: nothing answers any more
SIGN_IN_GONE = [{"match": "--profile work", "exit": 254, "stderr": EXPIRED.format(op="GetParameter")}]
RELEASED = "The deploy lock of the stage is released."
# bash cannot handle a signal that was ignored when it started, and a test runner that was started detached ignores SIGHUP: the program is started with the default again
HUP_DEFAULT = "import os, signal, sys; signal.signal(signal.SIGHUP, signal.SIG_DFL); os.execv(sys.argv[1], sys.argv[1:])"


class LeftBase(WaitBase):
    WAIT = UpdateBase.UPDATE + ("--wait-for-runs", "5")

    def ended(self, proc, seconds: float, what: str):
        setup_waits.wait_for(lambda: proc.status() is not None, seconds, what)
        setup_waits.wait_for(lambda: proc._stopped.is_set(), 10.0, "the output of setup.sh to end")
        return proc.status(), proc.text()

    def builds(self) -> dict:
        return self.world().get("builds", {})

    def following(self, proc, starts: int = 2) -> None:
        """Wait until the run has started its build (the `starts`-th start-build of the stand-in's world) and looked at it once: its Ctrl-C handler is in place."""

        def looked() -> bool:
            try:
                calls = self.normalized_calls()
            except ValueError:  # the stand-in was writing its log line while it was read
                return False
            at = [i for i, c in enumerate(calls) if c[:2] == ["codebuild", "start-build"]]
            return len(at) >= starts and any(c[:2] == ["codebuild", "batch-get-builds"] for c in calls[at[-1] + 1:])

        proc.wait_ready(looked, 120.0, "the run to start its build and look at it once")

    def assert_says_the_lock_stays(self, flat: str) -> None:
        self.assertNotIn("is released", flat, "nothing says that the lock is released")
        self.assertIn(f"The deploy lock {LOCK} could not be released", flat)
        self.assertIn("the stage starts no new runs until 30 minutes after its last write", flat)
        self.assertIn(f"aws ssm delete-parameter --name {LOCK}", flat)
        self.assertIn("./setup.sh --update again on this machine", flat)

    def poll_until_done(self, build: str) -> str:
        """The build goes on in the account while nobody follows it: the stand-in moves it one step for each look."""
        status = ""
        for _ in range(6):
            p = subprocess.run([str(self.stub / "aws"), "--profile", "work", "--region", "us-east-1", "codebuild", "batch-get-builds", "--ids", build,  # nosec B603: the stand-in AWS CLI of the test
                                "--query", "builds[0].buildStatus", "--output", "text"], env=self.env(), capture_output=True, text=True, timeout=limit(60))
            status = p.stdout.strip()
            if status != "IN_PROGRESS":
                break
        return status


class ReleaseSaysWhatHappenedCase(LeftBase):
    """Setup says `released` only after a delete that went through."""

    def test_b804_wait_a_lock_that_is_given_back_is_still_called_released(self) -> None:
        self.deployed()
        self.commit_more()
        self.looks([], RUNS)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("The stage is unchanged and no runner was installed. " + RELEASED, self.flat(p))
        self.assertIsNone(self.lock_now())

    def test_b804_wait_ctrl_c_with_a_sign_in_that_ran_out_says_that_the_lock_stays(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={MACHINE: RUNS})
        proc = self.popen(*self.WAIT, HAMH_SETUP_RUN_POLL_SECONDS="1")
        proc.wait_ready(lambda: "Waiting for 2 active run(s)" in proc.text(), 120.0, "setup.sh to wait for the active runs")
        self.set_scenario(faults=SIGN_IN_GONE)
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 4, done.output)
        lock = self.lock_now()
        self.assertIsNotNone(lock, "TRIGGER PRESENT: the lock could not be deleted and is still in the account")
        self.assertNotIn("deploys", lock_lines(lock), "TRIGGER PRESENT: it closes the stage")
        self.assertIn("No deployment or client change was made. No run was cancelled.", flat)
        self.assert_says_the_lock_stays(flat)

    def test_b804_wait_three_failed_looks_with_a_sign_in_that_ran_out_say_that_the_lock_stays(self) -> None:
        self.deployed()
        self.commit_more()
        # a plain update: the pause before the look that decides gives the test the moment at which the sign-in runs out
        proc = self.popen(*self.UPDATE, HAMH_SETUP_RUN_SETTLE_SECONDS="8")
        proc.wait_ready(lambda: "Looking for runs that are still active" in proc.text(), 120.0, "setup.sh to take the lock and pause")
        self.set_scenario(faults=SIGN_IN_GONE)
        code, text = self.ended(proc, 120.0, "setup.sh to give up after three failed looks")
        flat = " ".join(text.split())
        self.assertEqual(code, 6, text)
        self.assertIn("could not look for the active runs of stage direct", flat)
        self.assertIsNotNone(self.lock_now(), "TRIGGER PRESENT: the lock could not be deleted")
        self.assert_says_the_lock_stays(flat)
        self.assertIn("Your AWS sign-in has run out", flat)

    def test_b804_wait_the_end_of_a_wait_with_a_sign_in_that_ran_out_says_that_the_lock_stays(self) -> None:
        self.deployed()
        self.commit_more()
        # the sign-in runs out while setup waits: the early look, the read of the preflight and the first look of the wait answer, nothing after them does
        self.set_scenario(executions={MACHINE: RUNS}, faults=[
            {"match": "get-parameter --name " + LOCK, "exit": 254, "after": 1, "stderr": EXPIRED.format(op="GetParameter")},
            {"match": "stepfunctions list-executions", "exit": 254, "after": 2, "stderr": EXPIRED.format(op="ListExecutions")},
            {"match": "delete-parameter --name " + LOCK, "exit": 254, "stderr": EXPIRED.format(op="DeleteParameter")},
        ])
        p = self.run_full(*self.UPDATE, "--wait-for-runs", "2", HAMH_SETUP_RUN_MINUTE_SECONDS="1", HAMH_SETUP_RUN_POLL_SECONDS="1")
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertIsNotNone(self.lock_now(), "TRIGGER PRESENT: the lock could not be deleted")
        self.assertIn("No deployment or client change was made. No run was cancelled.", self.flat(p))
        self.assert_says_the_lock_stays(self.flat(p))
        self.assertFalse(self.runner_installed())


class QuietCallersCase(LeftBase):
    """A step that gives the lock back without a sentence of its own about it still says when the lock stays."""

    def test_b804_admit_a_failed_runner_install_whose_release_fails_warns_that_the_lock_stays(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[
            {"match": "cloudformation deploy --stack-name hamh-setup-runner", "exit": 254, "stderr": EXPIRED.format(op="CreateChangeSet")},
            {"match": "ssm delete-parameter --name " + LOCK, "exit": 254, "stderr": EXPIRED.format(op="DeleteParameter")},
        ])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("installing the runner stack failed", self.flat(p))
        self.assertIsNotNone(self.lock_now(), "TRIGGER PRESENT: the lock could not be deleted")
        self.assertNotIn("deploys", lock_lines(self.lock_now()), "TRIGGER PRESENT: it closes the stage")
        warned = " ".join(ln for ln in p.stdout.splitlines() if ln.startswith("warn  "))
        self.assertIn(f"The deploy lock {LOCK} could not be released", warned)
        self.assertIn("the stage starts no new runs until 30 minutes after its last write", warned)
        self.assertNotIn("is released", self.flat(p) + " " + self.flat_out(p))


class WaitReadsCase(LeftBase):
    """A read of the lock that fails during the looks is made again, as a look that fails is."""

    def test_b804_wait_one_failed_read_of_the_lock_does_not_end_the_wait(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions_by_look={MACHINE: [RUNS, RUNS, RUNS, [], []]},
                          # the read of the preflight answers; the first read of the wait is throttled once; every later read answers
                          faults=[{"match": "ssm get-parameter --name " + LOCK, "exit": 254, "after": 1, "times": 1, "stderr": THROTTLED.format(op="GetParameter")}])
        # HAMH_SETUP_LOCK_REFRESH_SECONDS=0: the renewal that is due every ten minutes is due at every look
        p = self.run_full(*self.WAIT, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        text = self.flat_out(p)
        self.assertIn("could not read the deploy lock", text)
        self.assertEqual(text.count("Waiting for "), 2, "the wait went on after the read that failed")
        self.assertIn("the active runs have ended", text)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 2, "the build of the full setup and the build of the update")
        self.assertIsNone(self.lock_now(), "the lock is given back at the end of the update")

    def test_b804_wait_a_write_of_the_lock_that_fails_is_said_and_made_again_at_the_next_look(self) -> None:
        self.deployed()
        self.commit_more()
        n = len(self.world().get("lock_writes", []))
        self.set_scenario(executions_by_look={MACHINE: [RUNS, RUNS, RUNS, [], []]},
                          # the create answers; the first renewal of the wait is throttled once; every later write answers
                          faults=[{"match": "ssm put-parameter --name " + LOCK, "exit": 254, "after": 1, "times": 1, "stderr": THROTTLED.format(op="PutParameter")}])
        p = self.run_full(*self.WAIT, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        text = self.flat_out(p)
        self.assertIn(f"could not write the deploy lock {LOCK} of stage direct again", text)
        self.assertIn("writing it again at the next look", text)
        self.assertIn("the active runs have ended", text)
        waiting = [w for w in self.lock_writes()[n:] if w.get("build") == "" and "deploys" not in w]
        self.assertGreaterEqual(len(waiting), 3, "the create and the renewals after the one that failed")

    def test_b804_wait_a_lock_that_could_not_be_written_for_the_30_minutes_stops_the_update(self) -> None:
        # The stage's API service takes a lock that was not written for 30 minutes for one nobody renews and starts runs again, so what the looks found no longer
        # holds. The stand-in checkout carries the number file with 2 seconds in the place of the 30 minutes.
        self.deployed()
        lib = self.repo / "scripts" / "lib"
        lib.mkdir(parents=True, exist_ok=True)
        (lib / "deploy-lock-stale-seconds.txt").write_text("2\n")
        self.commit_more()
        # the create answers, no write after it does
        self.set_scenario(executions={MACHINE: RUNS},
                          faults=[{"match": "ssm put-parameter --name " + LOCK, "exit": 254, "after": 1, "stderr": THROTTLED.format(op="PutParameter")}])
        p = self.run_full(*self.WAIT, HAMH_SETUP_LOCK_REFRESH_SECONDS="0", HAMH_SETUP_RUN_POLL_SECONDS="1")
        self.assertEqual(p.returncode, 6, self.out(p))
        flat = self.flat(p)
        self.assertIn(f"the deploy lock {LOCK} of stage direct was not written for more than", flat)
        self.assertIn("The stage has started runs again meanwhile, so this run deploys nothing. No deployment or client change was made. No run was cancelled.", flat)
        self.assertIn(RELEASED, flat)
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "only the build of the full setup")
        self.assertEqual(self.stop_calls(), [], "no run is ever cancelled")

    def test_b804_wait_a_lock_that_cannot_be_read_after_the_looks_stops_the_update_and_is_not_called_released(self) -> None:
        self.deployed()
        self.commit_more()
        # the read of the preflight answers, no read after it does: the update must not go on without having read its lock back, and it cannot give the lock back
        self.set_scenario(faults=[{"match": "ssm get-parameter --name " + LOCK, "exit": 254, "after": 1, "stderr": THROTTLED.format(op="GetParameter")}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        flat = self.flat(p)
        self.assertIn(f"could not read the deploy lock {LOCK} of stage direct", flat)
        self.assertIn("A failed read is not taken as this run's lock.", flat)
        self.assertIsNotNone(self.lock_now(), "TRIGGER PRESENT: the lock is still in the account")
        self.assert_says_the_lock_stays(flat)
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "only the build of the full setup")


class GivenBackOnExitCase(LeftBase):
    """Every exit while the lock is held and no build of it is recorded gives the lock back."""

    def test_b804_admit_a_failed_look_at_the_runner_stack_under_the_lock_gives_the_lock_back(self) -> None:
        self.deployed()
        self.commit_more()
        # how many looks at the runner stack an update makes before it takes the lock: a first update of the same shape tells
        n0 = len(self.calls())
        probe = self.run_full(*self.UPDATE)
        self.assertEqual(probe.returncode, 0, self.out(probe))
        later = [" ".join(c) for c in self.calls()[n0:]]
        take = next(i for i, c in enumerate(later) if "put-parameter" in c and LOCK in c and "--overwrite" not in c)
        words = "describe-stacks --stack-name hamh-setup-runner"
        before = sum(1 for c in later[:take] if words in c)
        self.assertTrue(any(words in c for c in later[take:]), "TRIGGER PRESENT: an update looks at the runner stack after it has taken the lock")
        self.commit_more("MORE.md")
        self.set_scenario(faults=[{"match": words, "exit": 254, "after": before, "times": 1, "stderr": THROTTLED.format(op="DescribeStacks")}])
        calls = len(self.normalized_calls())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 1, self.out(p))
        flat = self.flat(p)
        self.assertIn("could not look at the stack hamh-setup-runner", flat)
        self.assertEqual(self.lock_calls_since(calls), ["put-parameter", "delete-parameter"], "TRIGGER PRESENT: the lock was taken before the look failed; it is given back")
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertIn(RELEASED, flat)
        self.assertFalse(self.runner_installed())

    def test_b804_admit_a_closed_terminal_during_the_wait_gives_the_lock_back(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={MACHINE: RUNS})
        proc = GroupProcess([sys.executable, "-c", HUP_DEFAULT, BASH, str(self.repo / "setup.sh"), *self.WAIT], env=self.env(HAMH_SETUP_RUN_POLL_SECONDS="1"),
                            cwd=str(self.tmp), case=self, name="setup.sh")
        proc.wait_ready(lambda: "Waiting for 2 active run(s)" in proc.text(), 120.0, "setup.sh to wait for the active runs")
        self.assertIsNotNone(self.lock_now(), "TRIGGER PRESENT: the lock is held while setup waits")
        done = proc.interrupt(signal.SIGHUP)
        self.assertEqual(done.returncode, 4, done.output)
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertIn(RELEASED, " ".join(done.output.split()))
        self.assertFalse(self.runner_installed())
        self.assertEqual(self.stop_calls(), [], "no run is ever cancelled")

    def test_b804_admit_ctrl_c_during_the_build_of_the_bootstrap_template_gives_the_lock_back(self) -> None:
        # a first setup in an account without a CDK bootstrap: the lock is taken, the runner installed, and a first build prints the bootstrap template.
        # That build takes no lock and can be repeated, so a run that stops following it has no build of its lock: the lock goes back.
        self.set_scenario(bootstrap_present=False, build={"hang": True})
        proc = self.popen(*self.ARGS, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc, starts=1)
        lock = self.lock_now()
        self.assertIsNotNone(lock, "TRIGGER PRESENT: the lock is held while the template build runs")
        self.assertEqual(lock_lines(lock)["build"], "", "TRIGGER PRESENT: and it records no build")
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 6, done.output)
        self.assertIn("Stopped following the build", flat)
        self.assertIsNone(self.lock_now(), "the lock is given back")
        self.assertIn(RELEASED, flat)


class AfterTheBuildCase(LeftBase):
    """Once the build has succeeded, the lock no longer closes the stage, however the run ends."""

    def test_b804_admit_a_failed_read_of_the_result_after_the_build_succeeded_opens_the_stage(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[{"match": "/result.json", "exit": 254, "stderr": EXPIRED.format(op="GetObject")}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        flat = self.flat(p)
        self.assertIn("could not read the result of build", flat)
        fields = lock_lines(self.lock_now() or "")
        build = fields.get("build")
        self.assertTrue(build, "the lock stays with its build, so the next run finishes from it")
        self.assertEqual(self.builds()[build]["status"], "SUCCEEDED", "TRIGGER PRESENT: the build of the update has succeeded")
        self.assertEqual(fields.get("deploys"), "no", "the lock that is left does not close the stage")
        self.assertIn("The build has succeeded, so the stage starts runs again.", flat)
        self.assertNotIn("starts no new runs", flat)

    def test_b804_admit_a_lock_that_cannot_be_written_after_the_build_succeeded_is_said_to_close_the_stage(self) -> None:
        self.deployed()
        self.commit_more()
        # the lock is created and the build recorded in it; every write after that is throttled, and so is the one that would say deploys=no
        self.set_scenario(faults=[{"match": "ssm put-parameter --name " + LOCK, "exit": 254, "after": 2, "stderr": THROTTLED.format(op="PutParameter")},
                                  {"match": "/result.json", "exit": 254, "stderr": EXPIRED.format(op="GetObject")}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        fields = lock_lines(self.lock_now() or "")
        self.assertEqual(self.builds()[fields.get("build")]["status"], "SUCCEEDED", "TRIGGER PRESENT: the build of the update has succeeded")
        self.assertNotIn("deploys", fields, "TRIGGER PRESENT: the write that opens the stage failed, so the lock still closes it")
        warned = " ".join(ln for ln in p.stdout.splitlines() if ln.startswith("warn  "))
        self.assertIn(f"could not write the deploy lock {LOCK} to say that the build has succeeded", warned)
        flat = self.flat(p)
        self.assertNotIn("so the stage starts runs again", flat)
        self.assertIn(f"The stage starts no new runs until you run ./setup.sh --update again, or until 30 minutes after the last write of the deploy lock {LOCK}.", flat)

    def test_b804_admit_a_run_that_finishes_from_a_build_that_succeeded_never_closes_the_stage_again(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"hang": True})
        proc = self.popen(*self.UPDATE, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        self.assertEqual(proc.interrupt(signal.SIGINT).returncode, 6)
        build = lock_lines(self.lock_now())["build"]
        # the build ends well while nobody follows it
        self.set_scenario(build={})
        self.assertEqual(self.poll_until_done(build), "SUCCEEDED")
        self.assertNotIn("deploys", lock_lines(self.lock_now()), "TRIGGER PRESENT: the lock the first run left closes the stage")
        before = len(self.world().get("lock_writes", []))
        self.set_scenario(build={}, faults=[{"match": "result.json", "exit": 255, "times": 1,
                                             "stderr": 'Could not connect to the endpoint URL: "https://s3.us-east-1.amazonaws.com/"'}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertIn("already succeeded", self.out(p))
        writes = self.lock_writes()[before:]
        self.assertTrue(writes, "the run that finishes wrote the lock")
        for w in writes:
            self.assertEqual(w.get("deploys"), "no", "every write of a run that found the build succeeded opens the stage")
        self.assertEqual(lock_lines(self.lock_now()).get("deploys"), "no")
        # and the next run finishes the update
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))


class StoppedFollowingCase(LeftBase):
    """A run that stops following its build says what the stage does with new runs."""

    WORDS = ("The stage starts no new runs while this build deploys: the build renews the deploy lock by itself.",
             "The stage starts runs again when that run sees the build end, or 30 minutes after the build's last write of the lock.")

    def test_b804_admit_ctrl_c_while_following_says_what_the_stage_does_with_new_runs(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"hang": True})
        proc = self.popen(*self.UPDATE, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 6, done.output)
        fields = lock_lines(self.lock_now() or "")
        self.assertTrue(fields.get("build"), "the lock names the build")
        self.assertNotIn("deploys", fields, "TRIGGER PRESENT: the lock closes the stage")
        self.assertEqual(self.builds()[fields["build"]]["status"], "IN_PROGRESS", "TRIGGER PRESENT: the build of the update is still running")
        self.assertIn("keeps running in your AWS account", flat)
        for words in self.WORDS:
            self.assertIn(words, flat)

    def test_b804_admit_a_sign_in_that_runs_out_while_following_says_the_same(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"hang": True})
        proc = self.popen(*self.UPDATE, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        self.set_scenario(build={"hang": True}, faults=SIGN_IN_GONE)
        code, text = self.ended(proc, 120.0, "setup.sh to stop following after its looks failed")
        flat = " ".join(text.split())
        self.assertEqual(code, 6, text)
        self.assertIn("Stopped following the build", flat)
        self.assertIn("Your AWS sign-in may have run out.", flat)
        fields = lock_lines(self.lock_now() or "")
        self.assertEqual(self.builds()[fields["build"]]["status"], "IN_PROGRESS", "TRIGGER PRESENT: the build goes on")
        for words in self.WORDS:
            self.assertIn(words, flat)

    def test_b804_admit_a_build_that_only_makes_the_client_says_nothing_about_new_runs(self) -> None:
        # the stage already runs the commit: the lock says deploys=no from the start, and the stage keeps starting runs
        self.same_commit_stage()
        self.set_scenario(build={"hang": True})
        proc = self.popen(*self.UPDATE, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 6, done.output)
        self.assertEqual(lock_lines(self.lock_now() or "").get("deploys"), "no")
        self.assertNotIn("starts no new runs", flat)


if __name__ == "__main__":
    unittest.main()
