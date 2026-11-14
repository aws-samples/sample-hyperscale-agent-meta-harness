"""The stage starts runs again as soon as the deploy of an update has finished, and the lock handling stays right after that point.

The stage's API service starts no new runs while the deploy lock has no line `deploys=no`. The build of setup's runner deploys and then makes the client, and only
setup wrote the line, when CodeBuild said that the whole build had succeeded. The build's deploy now writes it right after the stage's record says deployed
(scripts/lib/deploy.py, held in test_deploy_b843_lock_open.py). These cases hold setup's side of it against the stand-in AWS CLI, whose build writes the line at the
end of its deploy phase when the scenario says `"opens_lock": true`:

- open       while the build goes on after its deploy, the lock in the account says deploys=no; setup says so once and never writes a lock that closes the stage again
- failure    a build that fails in a later phase ends setup with exit 5 and the lock is released; nothing stays closed
- stopped    a run that stops following a build whose deploy has finished says that the stage starts runs again; one that stops while the build deploys says when it
             will, and no longer names the end of the build as the moment
- fallback   a build that does not write the line (its write failed) leaves the stage closed until setup sees the build succeed, as before
- local      with `--build-on local` the lock is released when scripts/deploy.sh has returned, before the client is built on this machine

What these cases prove. Run against the setup files of the release before the change, three of the six fail and three pass. The three that fail hold what setup
says: the line of `open` and the two sentences of `stopped`. The three that pass are regression cases: `failure`, `fallback` and `local` held before the change,
and so did the lock assertions of `open`, because setup already kept a line `deploys=no` that it found in the lock. So this file holds setup's words and guards
its lock handling. The write that opens the lock is held with the real deploy in test_deploy_b843_lock_open.py. The stand-in build here writes the line in the
deploy's place, with `opened_lock_text` of setup_fake_aws.py, and that file holds the function to the bytes the real deploy writes; the two programs themselves
meet only in an update of a deployed stage.

Nothing here calls AWS.
"""

from __future__ import annotations

import signal
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
import test_setup_update_integration as integration  # noqa: E402
from test_setup_b804_lock_left import LeftBase  # noqa: E402
from test_setup_b804_wait import LOCK, lock_lines  # noqa: E402

AFTER_DEPLOY = ("gomod", "scanner-rules", "client", "outputs")
OPEN_SAID = "the deploy has finished: the stage starts runs again while the build goes on"
CLOSED_WORDS = ("The stage starts no new runs while this build deploys: the build renews the deploy lock by itself.",
                "The stage starts runs again when the deploy of this build has finished: the build then writes that into the lock. If that write fails, the stage "
                "starts runs again when that run sees the build end, or 30 minutes after the build's last write of the lock.")


class RunnerRouteCase(LeftBase):
    def update_stage(self, **build) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build=build)

    def last_build(self) -> dict:
        """The build that was started last (the stand-in keeps its builds in the order they were started); nothing while its file is being written."""
        try:
            builds = self.builds()
        except ValueError:
            return {}
        return list(builds.values())[-1] if builds else {}

    def build_of_the_lock(self, writes: list) -> dict:
        ids = [w.get("build") for w in writes if w.get("build")]
        self.assertTrue(ids, "the build is recorded in the lock")
        return self.builds()[ids[-1]]

    def test_b843_the_lock_says_deploys_no_while_the_build_goes_on_after_its_deploy(self) -> None:
        self.update_stage(opens_lock=True, polls_until_done=9)
        n = len(self.world().get("lock_writes", []))
        p = self.run_full(*self.UPDATE, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()[n:]
        seen = self.build_of_the_lock(writes)["lock_seen"]
        phases = [phase for phase, _ in seen]
        self.assertIn("deploy", phases, "TRIGGER PRESENT: a look fell on the deploy phase")
        self.assertTrue(set(AFTER_DEPLOY) & set(phases), "TRIGGER PRESENT: the build went on after its deploy while setup looked: " + str(phases))
        for phase, is_open in seen:
            if phase in AFTER_DEPLOY or phase == "deploy":
                self.assertTrue(is_open, f"the lock says deploys=no once the deploy has finished (phase {phase})")
            else:
                self.assertFalse(is_open, f"the lock closes the stage before the deploy has finished (phase {phase})")
        # setup renews at every look here: a write made from a look before the open has no line, and no write after it takes the line back
        first_open = next(i for i, w in enumerate(writes) if w.get("deploys") == "no")
        for w in writes[first_open:]:
            self.assertEqual(w.get("deploys"), "no", "once the stage is open, no write of setup closes it again")
        self.assertTrue(writes[first_open].get("build"), "the first write of setup that carries the line names the build")
        said = self.said(p)
        self.assertEqual(said.count(OPEN_SAID), 1, "setup says once that the stage starts runs again")
        self.assertLess(said.index(OPEN_SAID), said.index("phase 8: client ok"), "and says it before the client is built")
        self.assertIsNone(self.lock_now(), "the lock is released at the end")

    def test_b843_a_build_that_fails_after_its_deploy_ends_with_the_lock_released(self) -> None:
        self.update_stage(opens_lock=True, result="fail", final="FAILED", failed_phase="client", detail="build-client.sh exit 101: cargo failed")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        flat = self.said(p)
        self.assertTrue(self.last_build().get("lock_opened"), "TRIGGER PRESENT: the build's deploy had opened the lock before the client failed")
        self.assertIn("its deploy finished, and only the step client failed", flat)
        self.assertIsNone(self.lock_now(), "the lock is released: nothing stays closed and nothing stays held")
        self.assertNotIn("starts no new runs", flat[flat.index("failed in phase 'client'"):], "nothing after the failure says that the stage is closed")
        # the retry builds only what is missing and ends well
        self.set_scenario(build={"opens_lock": True})
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertIsNone(self.lock_now())

    def test_b843_a_run_that_stops_following_after_the_deploy_says_that_the_stage_starts_runs_again(self) -> None:
        self.update_stage(opens_lock=True, hang=True)
        proc = self.popen(*self.UPDATE, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        proc.wait_ready(lambda: bool(self.last_build().get("lock_opened")), 120.0, "the build to finish its deploy phase")
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 6, done.output)
        fields = lock_lines(self.lock_now() or "")
        self.assertTrue(fields.get("build"), "the lock stays with its build, so the next run attaches to it")
        self.assertEqual(self.builds()[fields["build"]]["status"], "IN_PROGRESS", "TRIGGER PRESENT: the build is still running")
        self.assertEqual(fields.get("deploys"), "no", "the lock that stays does not close the stage")
        self.assertIn("The deploy of this build has finished, so the stage starts runs again while the build goes on.", flat)
        self.assertNotIn("starts no new runs", flat[flat.index("Stopped following the build"):], "nothing after the stop says that the stage is closed")

    def test_b843_a_run_that_stops_following_while_the_build_deploys_says_when_the_stage_opens(self) -> None:
        # the stand-in build writes no line here (the default): the lock closes the stage when the run stops, as during the deploy of a real build
        self.update_stage(hang=True)
        proc = self.popen(*self.UPDATE, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 6, done.output)
        self.assertNotIn("deploys", lock_lines(self.lock_now() or ""), "TRIGGER PRESENT: the lock closes the stage")
        for words in CLOSED_WORDS:
            self.assertIn(words, flat)
        self.assertNotIn("The stage starts runs again when that run sees the build end", flat, "the end of the build is no longer named as the moment the stage opens")

    def test_b843_a_build_that_does_not_write_the_line_leaves_the_stage_closed_until_setup_sees_it_succeed(self) -> None:
        self.update_stage(polls_until_done=9)
        n = len(self.world().get("lock_writes", []))
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()[n:]
        seen = self.build_of_the_lock(writes)["lock_seen"]
        self.assertTrue(seen and not any(is_open for _, is_open in seen), "TRIGGER PRESENT: the build never wrote the line")
        self.assertNotIn(OPEN_SAID, self.said(p))
        self.assertEqual(writes[-1].get("deploys"), "no", "setup writes the line itself when CodeBuild says the build has succeeded")
        self.assertIsNone(self.lock_now())


class LocalRouteCase(integration.IntBase):
    """`--build-on local`: setup holds the lock for scripts/deploy.sh alone."""

    def test_b843_the_local_route_releases_the_lock_when_the_deploy_has_returned_and_builds_the_client_after_it(self) -> None:
        self.local_stage()
        first = self.run_local()
        self.assertEqual(first.returncode, 0, self.out(first))
        self.write_commit({"infra/change.yaml": "a: 1\n"})
        cargo = len(self.log_lines("cargo.log"))
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        seen = self.locks_seen()[-1]
        self.assertIsNotNone(seen, "TRIGGER PRESENT: scripts/deploy.sh ran under the lock of this run")
        self.assertNotIn("deploys", seen, "and that lock closed the stage while it deployed")
        self.assertGreater(len(self.log_lines("cargo.log")), cargo, "TRIGGER PRESENT: the client was built on this machine")
        self.assertIsNone(self.lock(), "the lock is gone at the end")
        # the release comes right after the deploy pipeline of full_local_flow and before the step that marks the client for building
        text = (REPO / "scripts" / "setup" / "full.sh").read_text(encoding="utf-8")
        flow = text[text.index("\nfull_local_flow() {"):text.index("\n# ---- the stage is there already")]
        deploy_at = flow.index("HAMH_DEPLOY_LOCK_TOKEN=$RN_LOCK_TOKEN scripts/deploy.sh")
        release_at = flow.index("rn_lease_release", deploy_at)
        self.assertLess(release_at, flow.index("BUILD=1", deploy_at), "the lock is given back before the client is built")
        self.assertNotIn("rn_lock_renew", flow[deploy_at:], "nothing renews the lock after the deploy")


if __name__ == "__main__":
    unittest.main()
