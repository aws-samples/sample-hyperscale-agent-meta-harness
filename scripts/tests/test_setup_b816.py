"""The follow-ups of the update fix on the runner route of `./setup.sh`, each held by a case that failed before its fix.

- two setups that follow one build and end at the same moment: the one whose leave write lands last leaves the lock to the other, which has ended; the run that
  finds itself named after its own leave is the last one and removes the runner (1);
- a setup that stops following a shared build (Ctrl-C, a lost connection) takes itself out of the lock, so the setup that is still at work removes the runner (2);
- a look that fails inside the runner-removal guard keeps the runner as RETAINED and ends the run with exit 8, the way a failed removal does (3);
- the exit 7 of a lock that a manual `scripts/deploy.sh` holds says to wait for it, never to attach to it (4);
- `./setup.sh --full` on a deployed stage removes a runner that is left also when the client's folder cannot be written, because that run installs no client (5);
- a first full setup whose build failed after its deploy names `./setup.sh --update`, which builds the client of the commit the stage runs (6);
- SSM has no conditional write: an entry of the shared lock that a write of another setup at the same moment dropped is written again (8).

They run `setup.sh` from a stand-in checkout against the stand-in AWS CLI of setup_fake_aws.py. The other setup is the stand-in's: `on_call` events write the lock as
it would (World.lock_join, lock_leave_back, lock_drop_party) and end it (runner_gone_unless_named).
"""

from __future__ import annotations

import os
import socket
import stat
import unittest

import setup_fake_aws as fake
from test_setup_update_runner import RunnerUpdateBase, fake_now

OTHER = "other-host:4242"
FAILED_READ = "An error occurred (ThrottlingException) when calling the {op} operation: Rate exceeded"


class B816SharedLockCase(RunnerUpdateBase):
    JOIN = "codebuild batch-get-builds"

    def held(self) -> dict:
        fields = self.lock_fields()
        fields.pop("set", None)
        return fields

    def started_and_cut(self) -> dict:
        """This machine starts the build of an update and loses its connection while it follows it (exit 6): the build runs on, and the lock records it."""
        self.set_scenario(connection_lost=True)
        self.assertEqual(self.run_full(*self.UPDATE).returncode, 6)
        held = self.held()
        self.assertTrue(held.get("build"), "TRIGGER PRESENT: the lock records the build")
        return held

    def test_b816_two_setups_of_one_build_that_end_together_leave_no_runner_behind(self) -> None:
        self.deployed()
        self.commit_more()
        # a setup of the same commit on another machine joins this run's build, and ends at the same moment as this run: when this run leaves the lock to it, it has
        # already left the lock to this run (its write lands right after this run's)
        self.set_scenario(on_call=[{"match": self.JOIN, "lock_join": {"party": OTHER}},
                                   {"match": f"holder={OTHER}", "post": True, "lock_leave_back": {"party": OTHER}}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        seen = self.world().get("faults_seen", {})
        self.assertIn("event-0", seen, "TRIGGER PRESENT: the other setup joined the build")
        self.assertIn("event-1", seen, "TRIGGER PRESENT: this run left the lock to the other setup, which left it back at the same moment")
        self.assertFalse(self.runner_installed(), "the setup that finds itself last under the lock removes the runner")
        self.assertIsNone(self.lock(), "and gives the lock back")
        self.assertNotIn("RETAINED", self.out(p))

    def test_b816_a_setup_that_stops_following_a_shared_build_takes_itself_out_of_the_lock(self) -> None:
        self.deployed()
        self.commit_more()
        held = self.started_and_cut()
        # that setup runs on another machine and is still at work; this machine attaches to its build and stops following it too (Ctrl-C, a lost connection): its first
        # look at the build answers, the looks of its follow do not
        self.set_scenario(connection_lost=False)
        self.seed_lock(**{**held, "holder": OTHER, "started": fake_now(30)})
        w = self.open_world()
        w.w["faults_seen"] = {}
        w.save()
        self.set_scenario(faults=[{"match": "codebuild batch-get-builds", "after": 1, "exit": 255,
                                   "stderr": 'Could not connect to the endpoint URL: "https://codebuild.us-east-1.amazonaws.com/"'}])
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 6, self.out(q))
        self.assertIn("Attaching to the running build", self.out(q), "TRIGGER PRESENT: this run followed the other setup's build")
        text = self.lock() or ""
        self.assertIn(OTHER, text, "the lock stays the other setup's, with its build")
        self.assertEqual(self.held().get("build"), held["build"])
        self.assertNotIn(socket.gethostname(), text, "this run took itself out, so the other setup removes the runner when it ends and nobody waits 30 minutes for it")
        self.assertIn("took itself out of the deploy lock", self.said(q))

    def test_b816_a_shared_lock_entry_that_a_write_at_the_same_moment_dropped_is_written_again(self) -> None:
        self.deployed()
        self.commit_more()
        held = self.started_and_cut()
        self.set_scenario(connection_lost=False)
        self.seed_lock(**{**held, "holder": OTHER, "started": fake_now(30)})
        # the other setup renews the lock in the same second as this run writes itself into it, from a read made before (SSM has no conditional write): this run's
        # entry is gone. That setup sees the build end first and ends while this run reads the build's result: it removes the runner when the lock names no setup of
        # this machine.
        self.set_scenario(on_call=[{"match": f"parties={OTHER}@", "post": True, "lock_drop_party": {"party": OTHER}},
                                   {"match": "/result.json", "armed_by": f"parties={OTHER}@",
                                    "runner_gone_unless_named": {"host": socket.gethostname()}}])
        q = self.run_full(*self.UPDATE)
        seen = self.world().get("faults_seen", {})
        self.assertIn("event-0", seen, "TRIGGER PRESENT: the other setup's write dropped this run's entry")
        self.assertIn("event-1", seen, "TRIGGER PRESENT: the other setup ended while this run read the result of the build")
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertTrue(self.installed().is_file(), "the runner and its bucket were still there for this run's client")


class B816RemovalCase(RunnerUpdateBase):
    def lock_reads(self, since: int) -> int:
        return sum(1 for c in self.normalized_calls()[since:] if c[:2] == ["ssm", "get-parameter"] and fake.LOCK_PARAM in c)

    def clear_faults(self) -> None:
        w = self.open_world()
        w.w["faults_seen"] = {}
        w.save()

    def test_b816_a_look_that_fails_in_the_runner_removal_guard_keeps_the_runner_retained_with_exit_8(self) -> None:
        self.deployed()
        # the position of the guard's read of the lock: the second to last read of an update (the release reads it last)
        self.commit_more("COUNT.md")
        n = len(self.normalized_calls())
        self.assertEqual(self.run_full(*self.UPDATE).returncode, 0)
        reads = self.lock_reads(n)
        cases = [("list-builds", {"match": "codebuild list-builds-for-project", "stderr": FAILED_READ.format(op="ListBuildsForProject")}),
                 ("batch-get-builds", {"match": "IN_PROGRESS'].id", "stderr": FAILED_READ.format(op="BatchGetBuilds")}),
                 ("lock", {"match": f"ssm get-parameter --name {fake.LOCK_PARAM}", "after": reads - 2, "times": 1, "stderr": FAILED_READ.format(op="GetParameter")})]
        for k, (what, fault) in enumerate(cases):
            with self.subTest(what=what):
                self.commit_more(f"C{k}.md")
                self.clear_faults()
                self.set_scenario(faults=[dict(fault, exit=254)])
                p = self.run_full(*self.UPDATE)
                self.set_scenario(faults=[])
                self.assertTrue(self.world().get("faults_seen", {}).get("0"), "TRIGGER PRESENT: the guard's look failed")
                self.assertEqual(p.returncode, 8, self.out(p))
                self.assertIn("RETAINED stack hamh-setup-runner", self.out(p))
                self.assertIn("Rate exceeded", self.said(p))
                self.assertTrue(self.runner_installed(), "the runner is never removed on a guess")

    def test_b816_full_on_a_deployed_stage_removes_a_left_runner_when_the_client_folder_cannot_be_written(self) -> None:
        self.deployed()
        w = self.open_world()
        w.make_runner()
        w.save()
        bindir = self.installed().parent
        mode = stat.S_IMODE(bindir.stat().st_mode)
        os.chmod(bindir, 0o500)  # the owner may read and enter it, nobody may write
        self.addCleanup(os.chmod, bindir, mode)
        before = self.installed().read_bytes()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("already deployed", self.out(p), "TRIGGER PRESENT: the stage is deployed, so this run builds and installs nothing")
        self.assertNotIn("you cannot write to the folder", self.said(p))
        self.assertFalse(self.runner_installed(), "the runner that was left is removed")
        self.assertEqual(self.installed().read_bytes(), before, "the client is as it was")
        # the way that already worked stays: --prefix names a folder you can write to
        w = self.open_world()
        w.make_runner()
        w.save()
        q = self.run_full(*self.ARGS, "--prefix", str(self.tmp / "other-prefix"))
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertFalse(self.runner_installed())


class B816MessageCase(RunnerUpdateBase):
    def test_b816_a_lock_that_a_manual_deploy_holds_says_to_wait_for_it_and_not_to_attach(self) -> None:
        first = self.deployed()
        self.commit_more()
        before = len(self.mutations())
        self.seed_lock(token="56" * 16, holder="build-box:4321", identity=f"arn:aws:iam::{self.ACCOUNT}:user/setup-test", route="manual", started=fake_now(60),
                       commit=first, inputs="", build="")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = self.flat(p)
        self.assertIn("route manual", flat, "TRIGGER PRESENT: the holder is a manual deploy")
        self.assertNotIn("git checkout", flat)
        self.assertNotIn("to attach to it", flat)
        self.assertIn("scripts/deploy.sh", flat)
        self.assertIn("Wait for it to end", flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created")

    def test_b816_a_first_setup_whose_build_failed_after_the_deploy_names_the_update(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "client", "detail": "SETUP-FAIL client the stand-in failed here"})
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertIn("only the step client failed", said, "TRIGGER PRESENT: the build failed after its deploy")
        self.assertIn("You run ./setup.sh --update to finish", said)
        self.assertNotIn("You run ./setup.sh --full to finish", said)


if __name__ == "__main__":
    unittest.main()
