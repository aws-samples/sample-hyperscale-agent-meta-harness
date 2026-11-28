"""With: the deploy lock of an update that lets the stage start runs, once its deploy has finished; and the dry plan under both changes.

Two changes of one release meet in the deploy lock `/hamh/<stage>/deploy-lock`:

- `./setup.sh --update --allow-active-runs --allow-new-runs` writes the line `starts=yes` into every text of the lock it holds, and the stage's API service
         starts runs under a lock with that line (test_setup_b841_new_runs.py)
- The build's deploy writes the line `deploys=no` into the lock when it has finished, and setup says that the stage starts runs again
         (test_setup_b843_stage_opens.py)

Each is held by its own file. These cases hold what only the two together show:

- both lines   the lock of such an update carries `starts=yes` in every write and `deploys=no` from the end of the deploy on: the build's write keeps the first
               line, and no write of setup drops either
- the words    under such a lock the stage never stopped starting runs, so nothing says that it "starts runs again": setup says that the stage kept starting
               runs, once when the build's deploy phase ends, when a run stops following after it, and when a run ends after its build has succeeded
- attach       a run that attaches without the option to the build of such an update says the same, because the line of the lock decides
- dry plan a dry run takes no lock and changes nothing with the options, alone and beside the lock of such an update; and the plan `ham update`
               asks for (`--wait-for-runs N --dry-run`) reads as "deploys nothing" for an update that only writes the release label, the update whose
               sentences changed

The cases run setup.sh against the stand-in AWS CLI (setup_fake_aws.py). Its build writes the lock at the end of its deploy phase with `"opens_lock": true`,
through `opened_lock_text`, which test_deploy_b843_lock_open.py holds to the bytes the real deploy writes. Nothing here calls AWS.

This file names no file that the public tree leaves out, and imports none, so that it ships with the other setup tests:
the two readers of a plan below are its own.
"""

from __future__ import annotations

import re
import signal
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
import test_setup_release_label as rl  # noqa: E402
from test_setup_b804_lock_left import EXPIRED, LeftBase  # noqa: E402
from test_setup_b804_wait import MACHINE, lock_lines  # noqa: E402
from test_setup_update_guard import RUNS  # noqa: E402

BOTH = ("--allow-active-runs", "--allow-new-runs")
AFTER_DEPLOY = ("gomod", "scanner-rules", "client", "outputs")
AGAIN = "starts runs again"
KEPT_SAID = "the deploy has finished: the stage kept starting runs (--allow-new-runs), and the build goes on"
KEPT_STOPPED = "The deploy of this build has finished. The stage kept starting runs while it deployed (--allow-new-runs), and the build goes on."
KEPT_SUCCEEDED = "The build has succeeded. The stage kept starting runs while it deployed (--allow-new-runs)."
# how the client reads the plan of setup (plan_deploys_nothing and its two constants)
CLIENT_RULES = REPO / "crates" / "ham-core" / "src" / "update.rs"


def words(name: str) -> list:
    """The strings of the constant `name` of the client's reader of a plan: how the `update:` and the `guard:` line of a plan that deploys nothing begin."""
    found = re.search(r"pub const %s: \[&str; 2\] =\s*\[(.*?)\];" % name, CLIENT_RULES.read_text(encoding="utf-8"), re.S)
    assert found, f"{CLIENT_RULES} has no constant {name}"
    return re.findall(r'"([^"]+)"', found.group(1))


def field(text: str, name: str) -> list:
    """The texts of the plan lines named `name`, as the client reads them: the name and a colon after the indent."""
    out = []
    for line in text.splitlines():
        line = line.lstrip()
        if line.startswith(name + ":"):
            out.append(line[len(name) + 1:].strip())
    return out


class StartsBase(LeftBase):
    def update_stage(self, **build) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build=build)

    def last_build(self) -> dict:
        """The build that was started last; nothing while the stand-in writes its file."""
        try:
            builds = self.builds()
        except ValueError:
            return {}
        return list(builds.values())[-1] if builds else {}

    def build_of_the_lock(self, writes: list) -> dict:
        ids = [w.get("build") for w in writes if w.get("build")]
        self.assertTrue(ids, "the build is recorded in the lock")
        return self.builds()[ids[-1]]

    def assert_both_lines_from_the_open_on(self, writes: list) -> None:
        self.assertTrue(writes, "the run wrote the lock")
        for n, w in enumerate(writes):
            self.assertEqual(w.get("starts"), "yes", f"write {n} of the lock keeps the line starts=yes: {w!r}")
        opened = [n for n, w in enumerate(writes) if w.get("deploys") == "no"]
        self.assertTrue(opened, "a write of setup carries the line deploys=no once the deploy has finished")
        for n, w in enumerate(writes[opened[0]:], opened[0]):
            self.assertEqual(w.get("deploys"), "no", f"write {n}: once the lock says deploys=no, no write of setup takes the line back")


class BothLinesCase(StartsBase):
    """An update with --allow-new-runs whose deploy finishes while setup follows the build."""

    def test_b843_b841_the_lock_keeps_both_lines_and_nothing_says_that_the_stage_starts_runs_again(self) -> None:
        self.update_stage(opens_lock=True, polls_until_done=9)
        n = len(self.world().get("lock_writes", []))
        p = self.run_full(*self.UPDATE, *BOTH, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()[n:]
        build = self.build_of_the_lock(writes)
        self.assertTrue(build.get("lock_opened"), "TRIGGER PRESENT: the deploy of the build wrote deploys=no into the lock")
        phases = [phase for phase, _ in build["lock_seen"]]
        self.assertTrue(set(AFTER_DEPLOY) & set(phases), "TRIGGER PRESENT: the build went on after its deploy while setup looked: " + str(phases))
        self.assertNotIn("deploys", writes[0], "TRIGGER PRESENT: the update deploys, so the lock it takes has no line deploys=no")
        self.assert_both_lines_from_the_open_on(writes)
        said = self.said(p)
        self.assertNotIn(AGAIN, said, "the stage never stopped starting runs under this update")
        self.assertEqual(said.count(KEPT_SAID), 1, "setup says once that the deploy has finished and that the stage kept starting runs")
        self.assertLess(said.index(KEPT_SAID), said.index("phase 8: client ok"), "and says it before the client is built")
        self.assertNotIn("starts no new runs", said)
        self.assertIsNone(self.lock_now(), "the lock is released at the end")

    def test_b843_b841_a_run_that_stops_following_after_the_deploy_leaves_both_lines_and_says_that_the_stage_kept_starting_runs(self) -> None:
        self.update_stage(opens_lock=True, hang=True)
        held = self.held_program("sleep")
        proc = self.popen(*self.UPDATE, *BOTH, HAMH_SETUP_POLL_SECONDS="1")
        self.following(proc)
        proc.wait_ready(lambda: bool(self.last_build().get("lock_opened")), 120.0, "the build to finish its deploy phase")
        self.hold_in_the_pause(proc, held)
        done = proc.interrupt(signal.SIGINT)
        flat = " ".join(done.output.split())
        self.assertEqual(done.returncode, 6, done.output)
        fields = lock_lines(self.lock_now() or "")
        self.assertTrue(fields.get("build"), "the lock stays with its build, so the next run attaches to it")
        self.assertEqual(self.builds()[fields["build"]]["status"], "IN_PROGRESS", "TRIGGER PRESENT: the build is still running")
        self.assertEqual(fields.get("deploys"), "no", "the lock that stays says that its deploy has finished")
        self.assertEqual(fields.get("starts"), "yes", "and it still carries the line of the option: the write of the build's deploy kept it")
        after = flat[flat.index("Stopped following the build"):]
        self.assertIn(KEPT_STOPPED, after)
        self.assertNotIn(AGAIN, after, "the stage never stopped starting runs under this update")
        self.assertNotIn("starts no new runs", after)

    def test_b843_b841_a_run_that_ends_after_its_build_succeeded_says_that_the_stage_kept_starting_runs(self) -> None:
        self.update_stage(opens_lock=True)
        self.set_scenario(faults=[{"match": "/result.json", "exit": 254, "stderr": EXPIRED.format(op="GetObject")}])
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 6, self.out(p))
        fields = lock_lines(self.lock_now() or "")
        self.assertEqual(self.builds()[fields.get("build")]["status"], "SUCCEEDED", "TRIGGER PRESENT: the build of the update has succeeded")
        self.assertEqual((fields.get("deploys"), fields.get("starts")), ("no", "yes"), "the lock that is left carries both lines")
        said = self.said(p)
        self.assertIn("could not read the result of build", said)
        self.assertIn(KEPT_SUCCEEDED, said)
        self.assertNotIn(AGAIN, said, "the stage never stopped starting runs under this update")


class AttachCase(StartsBase):
    """A plain `./setup.sh --update` (what setup prints for the way on, and what another user's `ham update` runs) that attaches to the build of an update
    with --allow-new-runs before the deploy of that build has finished."""

    def test_b843_b841_a_run_that_attaches_without_the_option_keeps_both_lines_and_says_what_the_lock_says(self) -> None:
        self.deployed()
        self.commit_more()
        starts = len(self.deploy_starts())
        self.set_scenario(connection_lost=True, build={"opens_lock": True, "polls_until_done": 9})
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the update started its build")
        self.set_scenario(connection_lost=False)
        held = lock_lines(self.lock_now() or "")
        self.assertEqual(held.get("starts"), "yes", "TRIGGER PRESENT: the lock that update left lets the stage start runs")
        self.assertTrue(held.get("build"), "TRIGGER PRESENT: the lock records the build")
        self.assertNotIn("deploys", held, "TRIGGER PRESENT: the deploy of that build has not finished when the rerun attaches")
        n = len(self.world().get("lock_writes", []))
        q = self.run_full(*self.UPDATE, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the rerun attaches to the build: no second deploy of the stage")
        self.assertTrue(self.builds()[held["build"]].get("lock_opened"), "TRIGGER PRESENT: the deploy of the build finished while the rerun followed it")
        self.assert_both_lines_from_the_open_on(self.lock_writes()[n:])
        said = self.said(q)
        self.assertIn(f"a build of this commit is already running ({held['build']}); setup attaches to it instead of starting another", said)
        self.assertEqual(said.count(KEPT_SAID), 1, "the run that attached says what the lock says, whatever it was given itself")
        self.assertNotIn(AGAIN, said, "the stage never stopped starting runs under the lock of that update")
        self.assertNotIn("starts no new runs", said)
        self.assertIsNone(self.lock_now(), "the lock is given back at the end")


class DryPlanCase(StartsBase):
    """A dry run under both changes: it prints its plan, takes no lock and changes nothing."""

    stage_at = rl.RunnerRoutePlanCase.stage_at
    docs_only_commit = rl.RunnerRoutePlanCase.docs_only_commit
    # the options `ham update` gives setup for its plan
    ASKED = ("--update", "--profile", "work", "--region", "us-east-1", "--yes", "--wait-for-runs", "60", "--dry-run")

    def test_b843_b841_a_dry_run_with_the_two_options_prints_the_plan_takes_no_lock_and_changes_nothing(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={MACHINE: RUNS})
        before = len(self.mutations())
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--yes", "--dry-run", *BOTH)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(field(p.stdout, "update")), 1, p.stdout)
        self.assertEqual(len(field(p.stdout, "guard")), 1, p.stdout)
        self.assertEqual(len(field(p.stdout, "new runs")), 1, p.stdout)
        self.assertTrue(field(p.stdout, "new runs")[0].startswith("the stage keeps starting runs while this update deploys (--allow-new-runs)."), p.stdout)
        self.assertEqual(len(self.mutations()), before, "a dry run creates, uploads, deletes and starts nothing")
        self.assertIsNone(self.lock_now(), "and takes no lock")

    def test_b843_b841_the_plan_ham_update_asks_for_beside_the_build_of_such_an_update_leaves_its_lock_as_it_is(self) -> None:
        # an update with --allow-new-runs lost its connection: its build runs, its lock says starts=yes; another user's `ham update` asks setup for its plan
        self.deployed()
        self.commit_more()
        self.set_scenario(connection_lost=True, build={"opens_lock": True, "polls_until_done": 9})
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.set_scenario(connection_lost=False, executions={MACHINE: RUNS})
        lock = self.lock_now()
        self.assertIn("starts=yes", (lock or "").splitlines(), "TRIGGER PRESENT: the lock of the running update lets the stage start runs")
        before = len(self.mutations())
        q = self.run_full(*self.ASKED)
        self.assertEqual(q.returncode, 0, self.out(q))
        update, guard = field(q.stdout, "update"), field(q.stdout, "guard")
        self.assertEqual((len(update), len(guard)), (1, 1), q.stdout)
        self.assertFalse(any(update[0].startswith(w) for w in words("PLAN_UPDATE_NOTHING")), "the update deploys, so the client keeps its wait: " + update[0])
        self.assertIn("setup attaches to it instead of starting another", " ".join(q.stdout.split()), "TRIGGER PRESENT: the plan is that of a run that would attach")
        self.assertEqual(len(self.mutations()), before, "a dry run creates, uploads, deletes and starts nothing")
        self.assertEqual(self.lock_now(), lock, "and writes nothing into the lock of the update it would attach to")

    def test_b843_b841_the_plan_of_an_update_that_only_writes_the_label_still_reads_as_deploying_nothing(self) -> None:
        self.assertEqual((len(words("PLAN_UPDATE_NOTHING")), len(words("PLAN_GUARD_NOTHING"))), (2, 2), "TRIGGER PRESENT: the client's two constants are read")
        recorded = self.stage_at("9.98")
        target = self.docs_only_commit()
        self.assertNotEqual(recorded, target, "TRIGGER PRESENT: the update installs another commit than the one the stage records")
        self.set_scenario(executions={MACHINE: RUNS})
        before = len(self.mutations())
        p = self.run_full(*self.ASKED)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the stage already runs this source tree (its record names commit", " ".join(self.out(p).split()),
                      "TRIGGER PRESENT: the sentence of a record that names another commit")
        update, guard = field(p.stdout, "update"), field(p.stdout, "guard")
        self.assertEqual((len(update), len(guard)), (1, 1), p.stdout)
        self.assertTrue(any(update[0].startswith(w) for w in words("PLAN_UPDATE_NOTHING")), update[0])
        self.assertTrue(any(guard[0].startswith(w) for w in words("PLAN_GUARD_NOTHING")), guard[0])
        self.assertEqual(len(self.mutations()), before, "a dry run creates, uploads, deletes and starts nothing")
        self.assertIsNone(self.lock_now(), "and takes no lock")


if __name__ == "__main__":
    unittest.main()
